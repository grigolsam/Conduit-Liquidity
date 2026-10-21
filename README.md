# Conduit liquidity pool

Solana program that holds a shared SOL reserve and one token vault per listed mint, and
settles swaps at a market price signed off-chain and verified on-chain.

Mainnet program: `HF91uuFVZTa4vG9jsbe1mW6Tp6p81afNLofdKTuZhdAp`
Explorer: https://solscan.io/account/HF91uuFVZTa4vG9jsbe1mW6Tp6p81afNLofdKTuZhdAp

## What the swap guarantees

Every swap is checked by the program itself, before any transfer happens:

| Check | Effect |
| --- | --- |
| Quote signature (Ed25519) | Only a price signed by the configured signer is accepted; a tampered price fails. |
| Quote bound to the user | A quote issued for one wallet cannot be used by another. |
| One-time nonce | The same authorization can never be replayed. |
| Short expiry | A stale quote is rejected instead of filled at an old price. |
| Reserve cap | A trade can use at most 95% of a reserve, so one order cannot drain it. |
| Per-swap and per-window limits | Global and per-token caps on size and daily throughput. |
| Atomic settlement | Both legs execute in one instruction — the swap completes fully or not at all. |
| Mint validation | Token-2022 mints with transfer fees or unexpected extensions are refused. |

Funds are held in program-owned accounts. There is no custody wallet and no private key
involved in holding or moving liquidity.

## Instructions

| Instruction | Who | Purpose |
| --- | --- | --- |
| `initialize` | owner | Set owner, price signer, auto-lister and trading limits. One time. |
| `list_token` | auto-lister | Add a listed mint with its own vault and limits. |
| `swap` | anyone | Buy or sell a listed token at a signed quote. No platform fee. |
| `deposit_sol` / `deposit_token` | anyone | Add SOL or tokens to the reserves. |
| `withdraw_sol` / `withdraw_token` | owner | Move reserves out of the program. |
| `set_paused` | owner | Stop swaps without blocking withdrawals. |
| `set_global_limits` / `set_default_limits` / `set_listing_limits` | owner | Change trading caps. |
| `set_price_signer` / `set_lister` | owner | Rotate the signer or lister key. |
| `propose_owner` / `accept_owner` | owner, then new owner | Two-step ownership transfer. |

## Layout

```text
programs/conduit-pool/
  src/lib.rs     program source
  Cargo.toml     crate manifest (anchor-lang / anchor-spl 0.30.1)
  Cargo.lock     pinned dependency versions
  tests/         on-chain test battery (Devnet only)
Anchor.toml
Cargo.toml
```

## Build

Requires the Solana CLI and Anchor CLI.

```bash
anchor build
```

## Tests

The battery runs against a Devnet deployment and needs a Devnet key fixture and a Devnet
RPC key. It never touches Mainnet or a database.

```bash
HELIUS_API_KEY=... CONDUIT_DEVNET_KEYS_PATH=./devnet-keys.json node programs/conduit-pool/tests/devnet-battery.mjs
```

## Notes

- Program-owned reserves are derived accounts; the program address itself holds only code.
- The owner role can withdraw reserves, pause swaps, and change configuration. This is
  visible on-chain.
- This code has not been independently audited. Read it before you rely on it.

## License

All rights reserved. Contact us if you want to review or build on it.
