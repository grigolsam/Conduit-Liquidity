//! Conduit liquidity pool.
//!
//! One shared SOL reserve plus one program-owned token vault per listed mint.
//! Swaps settle at a market price signed off-chain (Ed25519) and verified on-chain:
//! every quote is bound to one user, usable once, and valid for a short window.
//! Listing, trading limits and reserve withdrawals are restricted to the owner role.
use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    ed25519_program, system_instruction,
    program::{invoke, invoke_signed},
    sysvar::instructions::{load_current_index_checked, load_instruction_at_checked},
};
use anchor_spl::token_2022::spl_token_2022::{
    self,
    extension::{BaseStateWithExtensions, ExtensionType, StateWithExtensions},
    state::Mint as MintState,
};
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};

declare_id!("HF91uuFVZTa4vG9jsbe1mW6Tp6p81afNLofdKTuZhdAp");

// Conduit — https://x.com/Conduitliq

const MAX_USAGE_BPS: u128 = 9_500; // trades may use at most 95% of a reserve
const MAX_QUOTE_TTL: i64 = 10; // seconds; short to avoid free-option quotes on volatile tokens
const DOMAIN: &[u8; 16] = b"CONDUIT_QUOTE_V1";
// domain, program, config, mint, user, side, lamports, tokens, expiry, nonce
const QUOTE_LEN: usize = 16 + 32 + 32 + 32 + 32 + 1 + 8 + 8 + 8 + 8;
// Canonical single-signature Ed25519 precompile layout (as built by @solana/web3.js).
const ED_PK_OFF: usize = 16;
const ED_SIG_OFF: usize = 48;
const ED_MSG_OFF: usize = 112;

#[program]
pub mod conduit_pool {
    use super::*;

    /// Only the program's upgrade authority can initialize, so nobody can front-run setup.
    pub fn initialize(ctx: Context<Initialize>, owner: Pubkey, price_signer: Pubkey, global_limits: Limits, lister: Pubkey, default_limits: Limits) -> Result<()> {
        require!(owner != Pubkey::default() && price_signer != Pubkey::default(), ConduitError::InvalidKey);
        require!(lister != owner && lister != price_signer, ConduitError::InvalidKey);
        global_limits.validate()?;
        default_limits.validate()?;
        let c = &mut ctx.accounts.config;
        c.owner = owner;
        c.pending_owner = Pubkey::default();
        c.price_signer = price_signer;
        c.paused = false;
        c.bump = ctx.bumps.config;
        c.sol_vault_bump = ctx.bumps.sol_vault;
        c.global_limits = global_limits;
        c.global_window_start = Clock::get()?.unix_timestamp;
        c.global_window_sol_out = 0;
        c.lister = lister;
        c.default_limits = default_limits;
        Ok(())
    }

    /// Owner lists with custom limits; the backend lister key can only list with the
    /// owner-set default limits. The lister cannot withdraw, pause or change anything else.
    pub fn list_token(ctx: Context<ListToken>, limits: Limits) -> Result<()> {
        check_mint_supported(&ctx.accounts.mint)?;
        let c = &ctx.accounts.config;
        let who = ctx.accounts.authority.key();
        let limits = if who == c.owner { limits }
            else if c.lister != Pubkey::default() && who == c.lister { c.default_limits }
            else { return err!(ConduitError::Unauthorized) };
        limits.validate()?;
        let l = &mut ctx.accounts.listing;
        l.mint = ctx.accounts.mint.key();
        l.active = true;
        l.bump = ctx.bumps.listing;
        l.limits = limits;
        l.window_start = Clock::get()?.unix_timestamp;
        l.window_sol_out = 0;
        l.window_tokens_out = 0;
        Ok(())
    }

    /// Hard per-token risk caps the price signer can never override.
    pub fn set_listing_limits(ctx: Context<OwnerListing>, limits: Limits) -> Result<()> {
        limits.validate()?;
        ctx.accounts.listing.limits = limits;
        Ok(())
    }

    /// Owner sets or revokes (Pubkey::default) the automated lister key.
    pub fn set_lister(ctx: Context<OwnerOnly>, lister: Pubkey) -> Result<()> {
        let c = &mut ctx.accounts.config;
        require!(lister != c.owner && lister != c.price_signer, ConduitError::InvalidKey);
        c.lister = lister;
        Ok(())
    }

    pub fn set_default_limits(ctx: Context<OwnerOnly>, limits: Limits) -> Result<()> {
        limits.validate()?;
        ctx.accounts.config.default_limits = limits;
        Ok(())
    }

    pub fn set_listing_active(ctx: Context<OwnerListing>, active: bool) -> Result<()> {
        ctx.accounts.listing.active = active;
        Ok(())
    }

    /// Hard cap on total SOL paid out of the shared reserve per window,
    /// so a compromised price signer cannot drain the pool across many quotes.
    pub fn set_global_limits(ctx: Context<OwnerOnly>, limits: Limits) -> Result<()> {
        limits.validate()?;
        let c = &mut ctx.accounts.config;
        c.global_limits = limits;
        c.global_window_start = Clock::get()?.unix_timestamp;
        c.global_window_sol_out = 0;
        Ok(())
    }

    pub fn deposit_sol(ctx: Context<DepositSol>, lamports: u64) -> Result<()> {
        require!(lamports > 0, ConduitError::ZeroAmount);
        invoke(
            &system_instruction::transfer(&ctx.accounts.payer.key(), &ctx.accounts.sol_vault.key(), lamports),
            &[ctx.accounts.payer.to_account_info(), ctx.accounts.sol_vault.to_account_info(), ctx.accounts.system_program.to_account_info()],
        )?;
        emit!(Deposited { mint: Pubkey::default(), amount: lamports });
        Ok(())
    }

    pub fn deposit_token(ctx: Context<DepositToken>, amount: u64) -> Result<()> {
        require!(amount > 0, ConduitError::ZeroAmount);
        let a = &ctx.accounts;
        token_interface::transfer_checked(
            CpiContext::new(a.token_program.to_account_info(), TransferChecked {
                from: a.from.to_account_info(), mint: a.mint.to_account_info(),
                to: a.token_vault.to_account_info(), authority: a.payer.to_account_info(),
            }),
            amount, a.mint.decimals,
        )?;
        emit!(Deposited { mint: a.mint.key(), amount });
        Ok(())
    }

    /// Owner withdrawal; always keeps the vault rent-exempt so it stays usable.
    pub fn withdraw_sol(ctx: Context<WithdrawSol>, lamports: u64) -> Result<()> {
        require!(lamports > 0, ConduitError::ZeroAmount);
        let rent = Rent::get()?.minimum_balance(0);
        let free = ctx.accounts.sol_vault.lamports().saturating_sub(rent);
        require!(lamports <= free, ConduitError::InsufficientLiquidity);
        let c = &ctx.accounts.config;
        invoke_signed(
            &system_instruction::transfer(&ctx.accounts.sol_vault.key(), &ctx.accounts.to.key(), lamports),
            &[ctx.accounts.sol_vault.to_account_info(), ctx.accounts.to.to_account_info(), ctx.accounts.system_program.to_account_info()],
            &[&[b"sol_vault", &[c.sol_vault_bump]]],
        )?;
        emit!(Withdrawn { mint: Pubkey::default(), amount: lamports, to: ctx.accounts.to.key() });
        Ok(())
    }

    pub fn withdraw_token(ctx: Context<WithdrawToken>, amount: u64) -> Result<()> {
        require!(amount > 0, ConduitError::ZeroAmount);
        let a = &ctx.accounts;
        let seeds: &[&[u8]] = &[b"config", &[a.config.bump]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(a.token_program.to_account_info(), TransferChecked {
                from: a.token_vault.to_account_info(), mint: a.mint.to_account_info(),
                to: a.to.to_account_info(), authority: a.config.to_account_info(),
            }, &[seeds]),
            amount, a.mint.decimals,
        )?;
        emit!(Withdrawn { mint: a.mint.key(), amount, to: a.to.key() });
        Ok(())
    }

    pub fn set_paused(ctx: Context<OwnerOnly>, paused: bool) -> Result<()> {
        ctx.accounts.config.paused = paused;
        Ok(())
    }

    pub fn set_price_signer(ctx: Context<OwnerOnly>, price_signer: Pubkey) -> Result<()> {
        require!(price_signer != Pubkey::default(), ConduitError::InvalidKey);
        ctx.accounts.config.price_signer = price_signer;
        Ok(())
    }

    pub fn propose_owner(ctx: Context<OwnerOnly>, new_owner: Pubkey) -> Result<()> {
        require!(new_owner != Pubkey::default(), ConduitError::InvalidKey);
        ctx.accounts.config.pending_owner = new_owner;
        Ok(())
    }

    pub fn accept_owner(ctx: Context<AcceptOwner>) -> Result<()> {
        let c = &mut ctx.accounts.config;
        require!(c.pending_owner != Pubkey::default(), ConduitError::Unauthorized);
        require_keys_eq!(c.pending_owner, ctx.accounts.new_owner.key(), ConduitError::Unauthorized);
        c.owner = c.pending_owner;
        c.pending_owner = Pubkey::default();
        Ok(())
    }

    /// side 0 = buy (user pays SOL, gets tokens), 1 = sell.
    /// Requires the preceding instruction to be an Ed25519 verify of the quote by price_signer.
    pub fn swap(ctx: Context<Swap>, side: u8, lamports: u64, tokens: u64, expiry: i64, nonce: u64) -> Result<()> {
        let a = &ctx.accounts;
        require!(!a.config.paused, ConduitError::Paused);
        require!(a.listing.active, ConduitError::NotListed);
        require!(side <= 1, ConduitError::InvalidSide);
        require!(lamports > 0 && tokens > 0, ConduitError::ZeroAmount);
        let now = Clock::get()?.unix_timestamp;
        let max_expiry = now.checked_add(MAX_QUOTE_TTL).ok_or(ConduitError::InvalidExpiry)?;
        require!(expiry >= now, ConduitError::QuoteExpired);
        require!(expiry <= max_expiry, ConduitError::InvalidExpiry);

        let mut msg = Vec::with_capacity(QUOTE_LEN);
        msg.extend_from_slice(DOMAIN);
        msg.extend_from_slice(crate::ID.as_ref());
        msg.extend_from_slice(a.config.key().as_ref());
        msg.extend_from_slice(a.mint.key().as_ref());
        msg.extend_from_slice(a.user.key().as_ref());
        msg.push(side);
        msg.extend_from_slice(&lamports.to_le_bytes());
        msg.extend_from_slice(&tokens.to_le_bytes());
        msg.extend_from_slice(&expiry.to_le_bytes());
        msg.extend_from_slice(&nonce.to_le_bytes());
        verify_ed25519(&a.instructions.to_account_info(), &a.config.price_signer, &msg)?;

        // One nonce account per user: the quote must use the next nonce, then it advances,
        // so a quote can never be replayed and sibling quotes with the same nonce die.
        let us = &mut ctx.accounts.user_state;
        if us.user == Pubkey::default() { us.user = ctx.accounts.user.key(); us.bump = ctx.bumps.user_state; }
        require!(nonce == us.next_nonce, ConduitError::BadNonce);
        us.next_nonce = us.next_nonce.checked_add(1).ok_or(ConduitError::BadNonce)?;

        // Per-token exposure caps, enforced on-chain regardless of what the signer approves.
        let l = &mut ctx.accounts.listing;
        let lim = l.limits;
        if now.saturating_sub(l.window_start) >= lim.window_secs {
            l.window_start = now; l.window_sol_out = 0; l.window_tokens_out = 0;
        }
        if side == 0 {
            require!(tokens <= lim.max_swap_tokens, ConduitError::LimitExceeded);
            l.window_tokens_out = l.window_tokens_out.checked_add(tokens).ok_or(ConduitError::LimitExceeded)?;
            require!(l.window_tokens_out <= lim.max_window_tokens_out, ConduitError::LimitExceeded);
        } else {
            require!(lamports <= lim.max_swap_lamports, ConduitError::LimitExceeded);
            l.window_sol_out = l.window_sol_out.checked_add(lamports).ok_or(ConduitError::LimitExceeded)?;
            require!(l.window_sol_out <= lim.max_window_sol_out, ConduitError::LimitExceeded);
        }

        // Hard global cap on SOL leaving the shared reserve, per window.
        if side == 1 {
            let cfg = &mut ctx.accounts.config;
            let gl = cfg.global_limits;
            if now.saturating_sub(cfg.global_window_start) >= gl.window_secs {
                cfg.global_window_start = now;
                cfg.global_window_sol_out = 0;
            }
            cfg.global_window_sol_out = cfg.global_window_sol_out.checked_add(lamports).ok_or(ConduitError::LimitExceeded)?;
            require!(cfg.global_window_sol_out <= gl.max_window_sol_out, ConduitError::LimitExceeded);
        }

        let a = &ctx.accounts;
        let decimals = a.mint.decimals;
        if side == 0 {
            let cap = (a.token_vault.amount as u128) * MAX_USAGE_BPS / 10_000;
            require!((tokens as u128) <= cap, ConduitError::InsufficientLiquidity);
            invoke(
                &system_instruction::transfer(&a.user.key(), &a.sol_vault.key(), lamports),
                &[a.user.to_account_info(), a.sol_vault.to_account_info(), a.system_program.to_account_info()],
            )?;
            let seeds: &[&[u8]] = &[b"config", &[a.config.bump]];
            token_interface::transfer_checked(
                CpiContext::new_with_signer(a.token_program.to_account_info(), TransferChecked {
                    from: a.token_vault.to_account_info(), mint: a.mint.to_account_info(),
                    to: a.user_token.to_account_info(), authority: a.config.to_account_info(),
                }, &[seeds]),
                tokens, decimals,
            )?;
        } else {
            let rent = Rent::get()?.minimum_balance(0) as u128;
            let free = (a.sol_vault.lamports() as u128).saturating_sub(rent);
            require!((lamports as u128) <= free * MAX_USAGE_BPS / 10_000, ConduitError::InsufficientLiquidity);
            token_interface::transfer_checked(
                CpiContext::new(a.token_program.to_account_info(), TransferChecked {
                    from: a.user_token.to_account_info(), mint: a.mint.to_account_info(),
                    to: a.token_vault.to_account_info(), authority: a.user.to_account_info(),
                }),
                tokens, decimals,
            )?;
            invoke_signed(
                &system_instruction::transfer(&a.sol_vault.key(), &a.user.key(), lamports),
                &[a.sol_vault.to_account_info(), a.user.to_account_info(), a.system_program.to_account_info()],
                &[&[b"sol_vault", &[a.config.sol_vault_bump]]],
            )?;
        }
        emit!(Swapped { mint: a.mint.key(), user: a.user.key(), side, lamports, tokens, nonce });
        Ok(())
    }
}

/// Legacy SPL mints are fine. Token-2022 mints may only carry metadata extensions
/// (no transfer fees, hooks, permanent delegate, default-frozen, etc.). No freeze authority.
fn check_mint_supported(mint: &InterfaceAccount<Mint>) -> Result<()> {
    require!(mint.freeze_authority.is_none(), ConduitError::UnsupportedMint);
    require!(mint.mint_authority.is_none(), ConduitError::UnsupportedMint);
    let info = mint.to_account_info();
    if *info.owner == spl_token_2022::ID {
        let data = info.try_borrow_data()?;
        let state = StateWithExtensions::<MintState>::unpack(&data)?;
        for ext in state.get_extension_types()? {
            require!(
                matches!(ext, ExtensionType::MetadataPointer | ExtensionType::TokenMetadata),
                ConduitError::UnsupportedMint
            );
        }
    }
    Ok(())
}

/// Strict parser: exactly one signature, canonical offsets, all data inline.
fn verify_ed25519(ix_sysvar: &AccountInfo, signer: &Pubkey, msg: &[u8]) -> Result<()> {
    let cur = load_current_index_checked(ix_sysvar)?;
    require!(cur > 0, ConduitError::BadQuoteSignature);
    let ix = load_instruction_at_checked((cur - 1) as usize, ix_sysvar)?;
    require_keys_eq!(ix.program_id, ed25519_program::ID, ConduitError::BadQuoteSignature);
    require!(ix.accounts.is_empty(), ConduitError::BadQuoteSignature);
    let d = &ix.data;
    require!(d.len() >= ED_PK_OFF && d[0] == 1 && d[1] == 0, ConduitError::BadQuoteSignature);
    let rd = |o: usize| u16::from_le_bytes([d[o], d[o + 1]]) as usize;
    let (sig_off, sig_ix, pk_off, pk_ix, msg_off, msg_len, msg_ix) =
        (rd(2), rd(4), rd(6), rd(8), rd(10), rd(12), rd(14));
    let inline = u16::MAX as usize;
    require!(sig_ix == inline && pk_ix == inline && msg_ix == inline, ConduitError::BadQuoteSignature);
    require!(pk_off == ED_PK_OFF && sig_off == ED_SIG_OFF && msg_off == ED_MSG_OFF, ConduitError::BadQuoteSignature);
    let end = msg_off.checked_add(msg_len).ok_or(ConduitError::BadQuoteSignature)?;
    require!(d.len() == end, ConduitError::BadQuoteSignature);
    require!(&d[pk_off..pk_off + 32] == signer.as_ref(), ConduitError::BadQuoteSignature);
    require!(msg_len == msg.len() && &d[msg_off..end] == msg, ConduitError::BadQuoteSignature);
    Ok(())
}

#[account]
#[derive(InitSpace)]
pub struct Config { pub owner: Pubkey, pub pending_owner: Pubkey, pub price_signer: Pubkey, pub paused: bool, pub bump: u8, pub sol_vault_bump: u8, pub global_limits: Limits, pub global_window_start: i64, pub global_window_sol_out: u64, pub lister: Pubkey, pub default_limits: Limits }

#[account]
#[derive(InitSpace)]
pub struct Listing {
    pub mint: Pubkey, pub active: bool, pub bump: u8, pub limits: Limits,
    pub window_start: i64, pub window_sol_out: u64, pub window_tokens_out: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, InitSpace)]
pub struct Limits {
    pub max_swap_lamports: u64,     // max SOL paid out by one sell
    pub max_swap_tokens: u64,       // max tokens paid out by one buy
    pub max_window_sol_out: u64,    // max SOL paid out by sells per window
    pub max_window_tokens_out: u64, // max tokens paid out by buys per window
    pub window_secs: i64,
}
impl Limits {
    fn validate(&self) -> Result<()> {
        require!(self.window_secs > 0
            && self.max_swap_lamports <= self.max_window_sol_out
            && self.max_swap_tokens <= self.max_window_tokens_out, ConduitError::InvalidLimits);
        Ok(())
    }
}

#[account]
#[derive(InitSpace)]
pub struct UserState { pub user: Pubkey, pub next_nonce: u64, pub bump: u8 }

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)] pub payer: Signer<'info>,
    #[account(init, payer = payer, space = 8 + Config::INIT_SPACE, seeds = [b"config"], bump)]
    pub config: Account<'info, Config>,
    /// CHECK: system-owned PDA that holds the shared SOL reserve
    #[account(mut, seeds = [b"sol_vault"], bump)] pub sol_vault: UncheckedAccount<'info>,
    #[account(constraint = program.programdata_address()? == Some(program_data.key()) @ ConduitError::Unauthorized)]
    pub program: Program<'info, crate::program::ConduitPool>,
    #[account(constraint = program_data.upgrade_authority_address == Some(payer.key()) @ ConduitError::Unauthorized)]
    pub program_data: Account<'info, ProgramData>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ListToken<'info> {
    #[account(mut)] pub authority: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mint::token_program = token_program)]
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(init, payer = authority, space = 8 + Listing::INIT_SPACE, seeds = [b"listing", mint.key().as_ref()], bump)]
    pub listing: Account<'info, Listing>,
    #[account(init, payer = authority, seeds = [b"token_vault", mint.key().as_ref()], bump,
        token::mint = mint, token::authority = config, token::token_program = token_program)]
    pub token_vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct OwnerListing<'info> {
    pub owner: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = owner @ ConduitError::Unauthorized)]
    pub config: Account<'info, Config>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mut, seeds = [b"listing", mint.key().as_ref()], bump = listing.bump,
        constraint = listing.mint == mint.key() @ ConduitError::NotListed)]
    pub listing: Account<'info, Listing>,
}

#[derive(Accounts)]
pub struct OwnerOnly<'info> {
    pub owner: Signer<'info>,
    #[account(mut, seeds = [b"config"], bump = config.bump, has_one = owner @ ConduitError::Unauthorized)]
    pub config: Account<'info, Config>,
}

#[derive(Accounts)]
pub struct AcceptOwner<'info> {
    pub new_owner: Signer<'info>,
    #[account(mut, seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
}

#[derive(Accounts)]
pub struct DepositSol<'info> {
    #[account(mut)] pub payer: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump)] pub config: Account<'info, Config>,
    /// CHECK: PDA verified by seeds
    #[account(mut, seeds = [b"sol_vault"], bump = config.sol_vault_bump)] pub sol_vault: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct DepositToken<'info> {
    pub payer: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump)] pub config: Account<'info, Config>,
    #[account(seeds = [b"listing", mint.key().as_ref()], bump = listing.bump)] pub listing: Account<'info, Listing>,
    #[account(mint::token_program = token_program)]
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mut, token::mint = mint, token::token_program = token_program)]
    pub from: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, seeds = [b"token_vault", mint.key().as_ref()], bump,
        token::mint = mint, token::authority = config, token::token_program = token_program)]
    pub token_vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct WithdrawSol<'info> {
    pub owner: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = owner @ ConduitError::Unauthorized)]
    pub config: Account<'info, Config>,
    /// CHECK: PDA verified by seeds
    #[account(mut, seeds = [b"sol_vault"], bump = config.sol_vault_bump)] pub sol_vault: UncheckedAccount<'info>,
    /// CHECK: any destination chosen by the owner
    #[account(mut)] pub to: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct WithdrawToken<'info> {
    pub owner: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump, has_one = owner @ ConduitError::Unauthorized)]
    pub config: Account<'info, Config>,
    #[account(mint::token_program = token_program)]
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mut, seeds = [b"token_vault", mint.key().as_ref()], bump,
        token::mint = mint, token::authority = config, token::token_program = token_program)]
    pub token_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, token::mint = mint, token::token_program = token_program)]
    pub to: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
#[instruction(side: u8, lamports: u64, tokens: u64, expiry: i64, nonce: u64)]
pub struct Swap<'info> {
    #[account(mut)] pub user: Signer<'info>,
    #[account(mut, seeds = [b"config"], bump = config.bump)] pub config: Account<'info, Config>,
    #[account(mut, seeds = [b"listing", mint.key().as_ref()], bump = listing.bump,
        constraint = listing.mint == mint.key() @ ConduitError::NotListed)]
    pub listing: Account<'info, Listing>,
    #[account(mint::token_program = token_program)]
    pub mint: InterfaceAccount<'info, Mint>,
    /// CHECK: PDA verified by seeds
    #[account(mut, seeds = [b"sol_vault"], bump = config.sol_vault_bump)] pub sol_vault: UncheckedAccount<'info>,
    #[account(mut, seeds = [b"token_vault", mint.key().as_ref()], bump,
        token::mint = mint, token::authority = config, token::token_program = token_program)]
    pub token_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, token::mint = mint, token::authority = user, token::token_program = token_program)]
    pub user_token: InterfaceAccount<'info, TokenAccount>,
    // Single per-user nonce account (created once, reused forever).
    #[account(init_if_needed, payer = user, space = 8 + UserState::INIT_SPACE,
        seeds = [b"user", user.key().as_ref()], bump)]
    pub user_state: Account<'info, UserState>,
    /// CHECK: instructions sysvar, address-checked
    #[account(address = anchor_lang::solana_program::sysvar::instructions::ID)]
    pub instructions: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[event] pub struct Deposited { pub mint: Pubkey, pub amount: u64 }
#[event] pub struct Withdrawn { pub mint: Pubkey, pub amount: u64, pub to: Pubkey }
#[event] pub struct Swapped { pub mint: Pubkey, pub user: Pubkey, pub side: u8, pub lamports: u64, pub tokens: u64, pub nonce: u64 }

#[error_code]
pub enum ConduitError {
    #[msg("Only the owner can do this")] Unauthorized,
    #[msg("Trading is paused")] Paused,
    #[msg("Token is not listed")] NotListed,
    #[msg("Amount must be greater than zero")] ZeroAmount,
    #[msg("Quote expired")] QuoteExpired,
    #[msg("Quote signature invalid")] BadQuoteSignature,
    #[msg("Not enough liquidity")] InsufficientLiquidity,
    #[msg("Side must be 0 (buy) or 1 (sell)")] InvalidSide,
    #[msg("Quote expiry is too far in the future")] InvalidExpiry,
    #[msg("Key cannot be empty")] InvalidKey,
    #[msg("This token type is not supported")] UnsupportedMint,
    #[msg("Quote nonce is not the next nonce for this user")] BadNonce,
    #[msg("Trade exceeds this token's risk limits")] LimitExceeded,
    #[msg("Invalid risk limits")] InvalidLimits,
}
