import * as w3 from '@solana/web3.js';
import * as spl from '@solana/spl-token';
import bs58 from 'bs58';
import nacl from 'tweetnacl';
import { createHash } from 'crypto';
import fs from 'fs';

const { PublicKey, Keypair, Connection, Transaction, TransactionInstruction, SystemProgram, Ed25519Program, SYSVAR_INSTRUCTIONS_PUBKEY, LAMPORTS_PER_SOL } = w3;
if (!process.env.HELIUS_API_KEY) throw new Error('HELIUS_API_KEY is required for Devnet tests');
const conn = new Connection('https://devnet.helius-rpc.com/?api-key=' + process.env.HELIUS_API_KEY, 'confirmed');
const PID = new PublicKey(process.env.CONDUIT_DEVNET_PROGRAM_ID || '11111111111111111111111111111111');
const K = JSON.parse(fs.readFileSync(process.env.CONDUIT_DEVNET_KEYS_PATH || './devnet-keys.json', 'utf8'));
const kp = (k) => Keypair.fromSecretKey(bs58.decode(k.secret_b58));
const owner = kp(K.devnet_owner), signer = kp(K.price_signer), lister = kp(K.lister);
const ERR = ['Unauthorized','Paused','NotListed','ZeroAmount','QuoteExpired','BadQuoteSignature','InsufficientLiquidity','InvalidSide','InvalidExpiry','InvalidKey','UnsupportedMint','BadNonce','LimitExceeded','InvalidLimits'];

const pda = (...s) => PublicKey.findProgramAddressSync(s, PID)[0];
const config = pda(Buffer.from('config')), solVault = pda(Buffer.from('sol_vault'));
const disc = (n) => createHash('sha256').update('global:' + n).digest().subarray(0, 8);
const u64 = (v) => { const b = Buffer.alloc(8); b.writeBigUInt64LE(BigInt(v)); return b; };
const i64 = (v) => { const b = Buffer.alloc(8); b.writeBigInt64LE(BigInt(v)); return b; };
const limits = (l) => Buffer.concat([u64(l.sl), u64(l.st), u64(l.wsl), u64(l.wst), i64(l.ws)]);
const A = (pubkey, w = false, s = false) => ({ pubkey, isWritable: w, isSigner: s });
const ix = (name, data, keys) => new TransactionInstruction({ programId: PID, keys, data: Buffer.concat([disc(name), data]) });

async function send(ixs, signers) {
  const tx = new Transaction().add(...ixs);
  return w3.sendAndConfirmTransaction(conn, tx, signers, { commitment: 'confirmed' });
}
function errName(e) {
  const logs = (e.logs || e.transactionLogs || []).join('\n') + ' ' + String(e.message || e);
  const m = logs.match(/custom program error: 0x([0-9a-f]+)/i) || logs.match(/Error Number: (\d+)/);
  if (m) { const n = m[0].includes('0x') ? parseInt(m[1], 16) : +m[1]; return n >= 6000 ? ERR[n - 6000] : 'code ' + n; }
  const a = logs.match(/AnchorError[^.]*\. Error Code: (\w+)/); if (a) return a[1];
  return logs.slice(0, 160);
}
const results = [];
async function expectOk(label, fn) {
  try { const s = await fn(); results.push(['PASS', label, 'ok']); console.log('PASS', label); return s; }
  catch (e) { results.push(['FAIL', label, errName(e)]); console.log('FAIL', label, errName(e)); }
}
async function expectErr(label, want, fn) {
  try { await fn(); results.push(['FAIL', label, 'unexpectedly succeeded']); console.log('FAIL', label, 'SUCCEEDED'); }
  catch (e) { const n = errName(e); const ok = !want || n === want || (Array.isArray(want) && want.includes(n));
    results.push([ok ? 'PASS' : 'FAIL', label, 'rejected: ' + n]); console.log(ok ? 'PASS' : 'FAIL', label, n); }
}
const chainNow = async () => (await conn.getBlockTime(await conn.getSlot('confirmed'))) ?? Math.floor(Date.now() / 1000);

// ---- setup ----
const user = Keypair.generate(), attacker = Keypair.generate();
await send([
  SystemProgram.transfer({ fromPubkey: owner.publicKey, toPubkey: lister.publicKey, lamports: 0.03 * LAMPORTS_PER_SOL }),
  SystemProgram.transfer({ fromPubkey: owner.publicKey, toPubkey: user.publicKey, lamports: 0.1 * LAMPORTS_PER_SOL }),
  SystemProgram.transfer({ fromPubkey: owner.publicKey, toPubkey: attacker.publicKey, lamports: 0.03 * LAMPORTS_PER_SOL }),
], [owner]);
const TP = spl.TOKEN_PROGRAM_ID;
const mint = await spl.createMint(conn, owner, owner.publicKey, null, 6);
const ownerAta = await spl.createAssociatedTokenAccount(conn, owner, mint, owner.publicKey);
await spl.mintTo(conn, owner, mint, ownerAta, owner, 1_000_000n * 1_000_000n);
// a second mint that still has a mint authority (must be rejected)
const badMint = await spl.createMint(conn, owner, owner.publicKey, null, 6);
await expectErr('List token with live mint authority rejected', 'UnsupportedMint', () => listIx(lister, badMint));
await spl.setAuthority(conn, owner, mint, owner, spl.AuthorityType.MintTokens, null);
const userAta = await spl.createAssociatedTokenAccount(conn, owner, mint, user.publicKey);
const listing = pda(Buffer.from('listing'), mint.toBuffer()), tokenVault = pda(Buffer.from('token_vault'), mint.toBuffer());

function listIx(auth, m, tokenProgram = TP) {
  return send([ix('list_token', limits({ sl: 1, st: 1, wsl: 1, wst: 1, ws: 1 }), [
    A(auth.publicKey, true, true), A(config), A(m),
    A(pda(Buffer.from('listing'), m.toBuffer()), true), A(pda(Buffer.from('token_vault'), m.toBuffer()), true),
    A(tokenProgram), A(SystemProgram.programId)])], [auth]);
}
const ownerOnly = (name, data, who = owner) => send([ix(name, data, [A(who.publicKey, false, true), A(config, true)])], [who]);
const ownerListing = (name, data, who = owner) => send([ix(name, data, [A(who.publicKey, false, true), A(config), A(mint), A(listing, true)])], [who]);

async function swap({ side, lamports, tokens, expiry, nonce, by = signer, tamperTokens, u = user, uAta = userAta, m = mint, tokenProgram = TP, mutateEd, omitEd = false, between = [], domain = 'CONDUIT_QUOTE_V1', signedProgram = PID, signedConfig = config }) {
  const msg = Buffer.concat([Buffer.from(domain), signedProgram.toBuffer(), signedConfig.toBuffer(), m.toBuffer(), u.publicKey.toBuffer(),
    Buffer.from([side]), u64(lamports), u64(tokens), i64(expiry), u64(nonce)]);
  const sig = nacl.sign.detached(msg, by.secretKey);
  const ed = Ed25519Program.createInstructionWithPublicKey({ publicKey: by.publicKey.toBytes(), message: msg, signature: sig });
  if (mutateEd) mutateEd(ed);
  const data = Buffer.concat([Buffer.from([side]), u64(lamports), u64(tamperTokens ?? tokens), i64(expiry), u64(nonce)]);
  const sw = ix('swap', data, [A(u.publicKey, true, true), A(config, true), A(pda(Buffer.from('listing'), m.toBuffer()), true), A(m), A(solVault, true), A(pda(Buffer.from('token_vault'), m.toBuffer()), true),
    A(uAta, true), A(pda(Buffer.from('user'), u.publicKey.toBuffer()), true), A(SYSVAR_INSTRUCTIONS_PUBKEY), A(tokenProgram), A(SystemProgram.programId)]);
  return send([...(omitEd ? [] : [ed]), ...between, sw], [u]);
}
async function nextNonce(u = user) {
  const info = await conn.getAccountInfo(pda(Buffer.from('user'), u.publicKey.toBuffer()));
  return info ? info.data.readBigUInt64LE(8 + 32) : 0n;
}

// ---- listing & access control ----
await expectErr('Random wallet cannot list a token', 'Unauthorized', () => listIx(attacker, mint));
await expectOk('Backend lister key lists the new token', () => listIx(lister, mint));
await expectErr('Lister key cannot withdraw SOL', 'Unauthorized', () => send([ix('withdraw_sol', u64(1), [A(lister.publicKey, false, true), A(config), A(solVault, true), A(lister.publicKey, true), A(SystemProgram.programId)])], [lister]));
await expectErr('Lister key cannot pause', 'Unauthorized', () => ownerOnly('set_paused', Buffer.from([1]), lister));
await expectErr('Attacker cannot change the price signer', 'Unauthorized', () => ownerOnly('set_price_signer', attacker.publicKey.toBuffer(), attacker));
await expectErr('Attacker cannot change token limits', 'Unauthorized', () => ownerListing('set_listing_limits', limits({ sl: 9e9, st: 9e15, wsl: 9e9, wst: 9e15, ws: 1 }), attacker));

// ---- deposits ----
await expectOk('Deposit 0.01 SOL into the shared reserve', () => send([ix('deposit_sol', u64(0.01 * LAMPORTS_PER_SOL), [A(owner.publicKey, true, true), A(config), A(solVault, true), A(SystemProgram.programId)])], [owner]));
await expectOk('Deposit 500,000 tokens into the token vault', () => send([ix('deposit_token', u64(500_000n * 1_000_000n), [A(owner.publicKey, false, true), A(config), A(listing), A(mint), A(ownerAta, true), A(tokenVault, true), A(TP)])], [owner]));

// ---- swaps ----
let now = await chainNow();
await expectOk('Buy: 0.01 SOL for 1,000 tokens with a valid quote', async () => swap({ side: 0, lamports: 0.01 * LAMPORTS_PER_SOL, tokens: 1000_000000, expiry: (await chainNow()) + 8, nonce: await nextNonce() }));
const bal1 = (await spl.getAccount(conn, userAta)).amount;
console.log('user token balance after buy', bal1);
// replay: same nonce already used
await expectErr('Replay of an already-used quote', 'BadNonce', async () => swap({ side: 0, lamports: 0.01 * LAMPORTS_PER_SOL, tokens: 1000_000000, expiry: (await chainNow()) + 8, nonce: 0 }));
await expectErr('Quote signed by a fake price signer', 'BadQuoteSignature', async () => swap({ side: 0, lamports: 1, tokens: 1000_000000, expiry: (await chainNow()) + 8, nonce: await nextNonce(), by: attacker }));
await expectErr('Tampered quote (more tokens than signed)', 'BadQuoteSignature', async () => swap({ side: 0, lamports: 0.01 * LAMPORTS_PER_SOL, tokens: 1000_000000, tamperTokens: 100000_000000, expiry: (await chainNow()) + 8, nonce: await nextNonce() }));
await expectErr('Expired quote', 'QuoteExpired', async () => swap({ side: 0, lamports: 0.01 * LAMPORTS_PER_SOL, tokens: 1000_000000, expiry: (await chainNow()) - 30, nonce: await nextNonce() }));
await expectErr('Quote valid too long (60s)', 'InvalidExpiry', async () => swap({ side: 0, lamports: 0.01 * LAMPORTS_PER_SOL, tokens: 1000_000000, expiry: (await chainNow()) + 60, nonce: await nextNonce() }));
await expectErr('Quote for user A used by attacker', ['BadQuoteSignature', 'ConstraintTokenOwner', 'code 2015', 'code 3015'], async () => {
  // attacker submits user's quote with their own wallet (message binds user, so signature check fails)
  const aAta = await spl.createAssociatedTokenAccount(conn, attacker, mint, attacker.publicKey);
  const msgUser = user; // signed for user
  const msg = Buffer.concat([Buffer.from('CONDUIT_QUOTE_V1'), PID.toBuffer(), config.toBuffer(), mint.toBuffer(), msgUser.publicKey.toBuffer(), Buffer.from([0]), u64(1000), u64(1000_000000), i64((await chainNow()) + 8), u64(0)]);
  const ed = Ed25519Program.createInstructionWithPublicKey({ publicKey: signer.publicKey.toBytes(), message: msg, signature: nacl.sign.detached(msg, signer.secretKey) });
  const sw = ix('swap', Buffer.concat([Buffer.from([0]), u64(1000), u64(1000_000000), msg.subarray(msg.length - 16, msg.length - 8), u64(0)]), [A(attacker.publicKey, true, true), A(config, true), A(listing, true), A(mint), A(solVault, true), A(tokenVault, true), A(aAta, true), A(pda(Buffer.from('user'), attacker.publicKey.toBuffer()), true), A(SYSVAR_INSTRUCTIONS_PUBKEY), A(TP), A(SystemProgram.programId)]);
  return send([ed, sw], [attacker]);
});
await expectOk('Sell: 500 tokens for 0.004 SOL', async () => swap({ side: 1, lamports: 0.004 * LAMPORTS_PER_SOL, tokens: 500_000000, expiry: (await chainNow()) + 8, nonce: await nextNonce() }));
await expectErr('Buy more than 95% of the token vault', 'InsufficientLiquidity', async () => swap({ side: 0, lamports: 0.01 * LAMPORTS_PER_SOL, tokens: 480000_000000n, expiry: (await chainNow()) + 8, nonce: await nextNonce() }));
await expectErr('Sell paying out more SOL than 95% of reserve', 'InsufficientLiquidity', async () => {
  const free = Math.max(0, await conn.getBalance(solVault) - await conn.getMinimumBalanceForRentExemption(0));
  return swap({ side: 1, lamports: Math.floor(free * 95 / 100) + 1, tokens: 1_000000, expiry: (await chainNow()) + 8, nonce: await nextNonce() });
});

// ---- risk limits ----
await expectOk('Owner sets tight token limits (0.01 SOL per sell, 0.015 per hour)', () => ownerListing('set_listing_limits', limits({ sl: 0.01 * LAMPORTS_PER_SOL, st: 2000_000000, wsl: 0.015 * LAMPORTS_PER_SOL, wst: 3000_000000, ws: 3600 })));
await expectErr('Sell above per-trade SOL limit', 'LimitExceeded', async () => swap({ side: 1, lamports: 0.02 * LAMPORTS_PER_SOL, tokens: 100_000000, expiry: (await chainNow()) + 8, nonce: await nextNonce() }));
await expectOk('Sell within limit (0.008 SOL)', async () => swap({ side: 1, lamports: 0.008 * LAMPORTS_PER_SOL, tokens: 100_000000, expiry: (await chainNow()) + 8, nonce: await nextNonce() }));
await expectErr('Second sell breaks hourly SOL cap', 'LimitExceeded', async () => swap({ side: 1, lamports: 0.008 * LAMPORTS_PER_SOL, tokens: 100_000000, expiry: (await chainNow()) + 8, nonce: await nextNonce() }));
await expectErr('Buy above per-trade token limit', 'LimitExceeded', async () => swap({ side: 0, lamports: 0.001 * LAMPORTS_PER_SOL, tokens: 2500_000000, expiry: (await chainNow()) + 8, nonce: await nextNonce() }));
await expectOk('Owner sets global SOL cap to 0.001 SOL/hour', () => ownerOnly('set_global_limits', limits({ sl: 0.001 * LAMPORTS_PER_SOL, st: 1, wsl: 0.001 * LAMPORTS_PER_SOL, wst: 1, ws: 3600 })));
await expectOk('Owner widens token limits again', () => ownerListing('set_listing_limits', limits({ sl: 1 * LAMPORTS_PER_SOL, st: 100000_000000, wsl: 10 * LAMPORTS_PER_SOL, wst: 1000000_000000, ws: 3600 })));
await expectErr('Sell blocked by global pool cap', 'LimitExceeded', async () => swap({ side: 1, lamports: 0.002 * LAMPORTS_PER_SOL, tokens: 10_000000, expiry: (await chainNow()) + 8, nonce: await nextNonce() }));
await expectOk('Owner restores global cap', () => ownerOnly('set_global_limits', limits({ sl: 10 * LAMPORTS_PER_SOL, st: 1e15, wsl: 100 * LAMPORTS_PER_SOL, wst: 1e15, ws: 3600 })));

// ---- pause ----
await expectOk('Owner pauses trading', () => ownerOnly('set_paused', Buffer.from([1])));
await expectErr('Swap while paused', 'Paused', async () => swap({ side: 0, lamports: 0.001 * LAMPORTS_PER_SOL, tokens: 10_000000, expiry: (await chainNow()) + 8, nonce: await nextNonce() }));
await expectOk('Owner unpauses', () => ownerOnly('set_paused', Buffer.from([0])));
await expectOk('Owner turns this token off', () => ownerListing('set_listing_active', Buffer.from([0])));
await expectErr('Swap on a turned-off token', 'NotListed', async () => swap({ side: 0, lamports: 0.001 * LAMPORTS_PER_SOL, tokens: 10_000000, expiry: (await chainNow()) + 8, nonce: await nextNonce() }));
await expectOk('Owner turns token back on', () => ownerListing('set_listing_active', Buffer.from([1])));
await expectOk('Buy works again after re-enable', async () => swap({ side: 0, lamports: 0.001 * LAMPORTS_PER_SOL, tokens: 10_000000, expiry: (await chainNow()) + 8, nonce: await nextNonce() }));

// ---- withdrawals ----
const wsol = (who, to, lam) => send([ix('withdraw_sol', u64(lam), [A(who.publicKey, false, true), A(config), A(solVault, true), A(to, true), A(SystemProgram.programId)])], [who]);
const wtok = (who, to, amt) => send([ix('withdraw_token', u64(amt), [A(who.publicKey, false, true), A(config), A(mint), A(tokenVault, true), A(to, true), A(TP)])], [who]);
await expectErr('Attacker withdraws SOL', 'Unauthorized', () => wsol(attacker, attacker.publicKey, 1000));
const atkAta = spl.getAssociatedTokenAddressSync(mint, attacker.publicKey);
await expectErr('Attacker withdraws tokens', 'Unauthorized', () => wtok(attacker, atkAta, 1));
const vaultLam = await conn.getBalance(solVault);
await expectErr('Owner cannot drain below rent minimum', 'InsufficientLiquidity', () => wsol(owner, owner.publicKey, vaultLam));
await expectOk('Owner withdraws 0.1 SOL', () => wsol(owner, owner.publicKey, 0.1 * LAMPORTS_PER_SOL));
await expectOk('Owner withdraws 1,000 tokens', () => wtok(owner, ownerAta, 1000_000000));
await expectErr('Attacker takes ownership directly', 'Unauthorized', () => send([ix('accept_owner', Buffer.alloc(0), [A(attacker.publicKey, false, true), A(config, true)])], [attacker]));

// Canonical verifier checks: these valid precompile signatures must still be
// rejected by the pool's strict layout/domain parser, not unrelated failures.
const attackQuote = async (extra = {}) => swap({ side: 0, lamports: 1000, tokens: 1_000000, expiry: (await chainNow()) + 8, nonce: await nextNonce(), ...extra });
await expectErr('Missing Ed25519 verifier rejected', 'BadQuoteSignature', () => attackQuote({ omitEd: true }));
await expectErr('Nonadjacent verifier rejected', 'BadQuoteSignature', () => attackQuote({ between: [SystemProgram.transfer({ fromPubkey: user.publicKey, toPubkey: attacker.publicKey, lamports: 1 })] }));
await expectErr('Verifier nonzero padding rejected', 'BadQuoteSignature', () => attackQuote({ mutateEd: ed => { ed.data[1] = 1; } }));
await expectErr('Verifier trailing bytes rejected', 'BadQuoteSignature', () => attackQuote({ mutateEd: ed => { ed.data = Buffer.concat([ed.data, Buffer.from([0])]); } }));
await expectErr('Verifier relocated message rejected', 'BadQuoteSignature', () => attackQuote({ mutateEd: ed => {
  ed.data = Buffer.concat([ed.data.subarray(0, 112), Buffer.from([0]), ed.data.subarray(112)]); ed.data.writeUInt16LE(113, 10);
} }));
await expectErr('Verifier account metas rejected', 'BadQuoteSignature', () => attackQuote({ mutateEd: ed => { ed.keys.push(A(attacker.publicKey)); } }));
await expectErr('Quote for wrong program rejected', 'BadQuoteSignature', () => attackQuote({ signedProgram: SystemProgram.programId }));
await expectErr('Quote for wrong config rejected', 'BadQuoteSignature', () => attackQuote({ signedConfig: attacker.publicKey }));
await expectErr('Old quote domain rejected', 'BadQuoteSignature', () => attackQuote({ domain: 'INVALID_QUOTE_V1' }));
const beforeFailed = { nonce: await nextNonce(), sol: await conn.getBalance(solVault), tokens: (await spl.getAccount(conn, tokenVault)).amount };
await expectErr('Invalid side rejected atomically', 'InvalidSide', () => attackQuote({ side: 2 }));
await expectOk('Failed swap preserves nonce and both reserves', async () => {
  if (await nextNonce() !== beforeFailed.nonce || await conn.getBalance(solVault) !== beforeFailed.sol || (await spl.getAccount(conn, tokenVault)).amount !== beforeFailed.tokens) throw new Error('Failed swap changed state');
});

// Both token programs: real Devnet mints, no database fixtures.
const TP22 = spl.TOKEN_2022_PROGRAM_ID;
const m22 = await spl.createMint(conn, owner, owner.publicKey, null, 6, undefined, undefined, TP22);
const o22 = await spl.createAssociatedTokenAccount(conn, owner, m22, owner.publicKey, undefined, TP22);
await spl.mintTo(conn, owner, m22, o22, owner, 10000_000000n, [], undefined, TP22);
await spl.setAuthority(conn, owner, m22, owner, spl.AuthorityType.MintTokens, null, [], undefined, TP22);
const u22 = await spl.createAssociatedTokenAccount(conn, owner, m22, user.publicKey, undefined, TP22);
const l22 = pda(Buffer.from('listing'), m22.toBuffer()), v22 = pda(Buffer.from('token_vault'), m22.toBuffer());
await expectOk('Token-2022 mint lists successfully', () => listIx(lister, m22, TP22));
await expectOk('Token-2022 deposit succeeds', () => send([ix('deposit_token', u64(5000_000000n), [A(owner.publicKey, false, true), A(config), A(l22), A(m22), A(o22, true), A(v22, true), A(TP22)])], [owner]));
await expectOk('Token-2022 buy settles', () => attackQuote({ m: m22, tokenProgram: TP22, uAta: u22 }));
await expectOk('Token-2022 sell settles', () => attackQuote({ m: m22, tokenProgram: TP22, uAta: u22, side: 1 }));
await expectOk('Token-2022 owner withdrawal succeeds', () => send([ix('withdraw_token', u64(1_000000n), [A(owner.publicKey, false, true), A(config), A(m22), A(v22, true), A(o22, true), A(TP22)])], [owner]));
const frozenMint = await spl.createMint(conn, owner, owner.publicKey, owner.publicKey, 6);
await spl.setAuthority(conn, owner, frozenMint, owner, spl.AuthorityType.MintTokens, null);
await expectErr('Freeze-authority mint rejected', 'UnsupportedMint', () => listIx(lister, frozenMint));
const feeMint = Keypair.generate();
const feeSpace = spl.getMintLen([spl.ExtensionType.TransferFeeConfig]);
await send([SystemProgram.createAccount({ fromPubkey: owner.publicKey, newAccountPubkey: feeMint.publicKey, space: feeSpace, lamports: await conn.getMinimumBalanceForRentExemption(feeSpace), programId: TP22 }),
  spl.createInitializeTransferFeeConfigInstruction(feeMint.publicKey, owner.publicKey, owner.publicKey, 100, 100n, TP22),
  spl.createInitializeMintInstruction(feeMint.publicKey, 6, owner.publicKey, null, TP22)], [owner, feeMint]);
await spl.setAuthority(conn, owner, feeMint.publicKey, owner, spl.AuthorityType.MintTokens, null, [], undefined, TP22);
await expectErr('Token-2022 transfer-fee extension rejected', 'UnsupportedMint', () => listIx(lister, feeMint.publicKey, TP22));

// Full two-step transfer, restored to the original test owner on success.
const acceptOwner = who => send([ix('accept_owner', Buffer.alloc(0), [A(who.publicKey, false, true), A(config, true)])], [who]);
await expectOk('Owner proposes next owner', () => ownerOnly('propose_owner', attacker.publicKey.toBuffer()));
await expectErr('Proposed owner has no authority before acceptance', 'Unauthorized', () => ownerOnly('set_paused', Buffer.from([1]), attacker));
await expectErr('Unproposed wallet cannot accept ownership', 'Unauthorized', () => acceptOwner(lister));
await expectOk('Proposed owner accepts ownership', () => acceptOwner(attacker));
try {
  await expectErr('Former owner loses control after transfer', 'Unauthorized', () => ownerOnly('set_paused', Buffer.from([1])));
  await expectOk('Accepted owner controls pause', () => ownerOnly('set_paused', Buffer.from([0]), attacker));
  await expectOk('Accepted ownership clears pending owner', async () => {
    const c = await conn.getAccountInfo(config);
    if (!c || !new PublicKey(c.data.subarray(8, 40)).equals(attacker.publicKey) || !new PublicKey(c.data.subarray(40, 72)).equals(SystemProgram.programId)) throw new Error('Ownership state incorrect');
  });
} finally {
  await ownerOnly('propose_owner', owner.publicKey.toBuffer(), attacker); await acceptOwner(owner);
}
await expectOk('Original owner restored', () => ownerOnly('set_paused', Buffer.from([0])));

const final = { solVault: (await conn.getBalance(solVault)) / LAMPORTS_PER_SOL, tokenVault: String((await spl.getAccount(conn, tokenVault)).amount / 1_000_000n), userTokens: String((await spl.getAccount(conn, userAta)).amount / 1_000_000n), mint: mint.toBase58() };
console.log('FINAL', JSON.stringify(final));
fs.mkdirSync('/tmp/devnet', { recursive: true });
fs.writeFileSync('/tmp/devnet/results.json', JSON.stringify({ results, final }, null, 1));
// RPC WebSocket handles otherwise keep the process alive after results are saved.
process.exit(results.some(([status]) => status === 'FAIL') ? 1 : 0);
