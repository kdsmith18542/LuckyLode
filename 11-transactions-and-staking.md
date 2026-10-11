# 11 — Transactions & Staking: cheap, fast, anon, and the PoS hook

**Status:** design exploration opened 2026-10-11 at project direction.
Nothing here is frozen or scheduled. Items marked **[consensus]** would need
to enter the doc 02 §2.4 allowlist before adoption; everything else is
policy, wallet, or sidecar work.

## 11.1 Ground truth (what the fork already gives us)

- 60 s blocks (doc 05), hybrid 50/50 PoW/PoS — **now decided to keep**
  (decided 2026-10-11).
- Verus/Zcash transaction model inherited: transparent txs + optional
  Sapling shielded txs, 2 GB-style memory considerations — but tx sizes are
  Zcash-lineage (larger than Bitcoin/Litecoin; shielded far larger).
- Fee policy in force (doc 04 §4.1): minimal anti-spam floor, no fee burns
  at launch.
- VerusID, PBaaS inheritance potential — kept for later (deferred decision).

## 11.2 "Cheap" — policy, not a promise to Litecoin parity

Cheap means: low relay floor + no burns + no artificial scarcity. Concrete
floor values are a testnet decision, but the posture is: **fees exist to
stop spam, not to fund anything.** Publish honest fee guidance per tx type
(transparent vs shielded); shielded txs cost more because they are bigger —
say so in the wallet instead of hiding it.

## 11.3 "Fast" — honest UX, no fake finality

There is no InstantSend and we do not invent one (see §11.5: the classic
masternode features are either inherited or redundant). What "fast" can
honestly mean:

1. **60 s blocks** with PoS making many blocks land faster than that on
   average under steady stake diversity.
2. **0-conf policy** for small values: wallet-enforced cap, progressive
   disclosure ("unconfirmed", cap shown), rely on no-RBF-style discipline
   inherited from the fork where available. Documented risk, never hidden.
3. **Status honesty** in the dashboard: unconfirmed ≠ settled, and the UI
   never implies otherwise.

Anti-goal: "express relay" theater — relaying announces nothing; promising
finality we can't deliver is the Electroneum mistake in another costume
(doc 01).

## 11.4 "Anon" — inherited, optional, no new crypto

Sapling shielded txs are inherited and battle-tested. Design decisions that
follow (all wallet/app layer, no consensus):

- Wallet default visibility per amount tier (small payments default
  shielded is the candidate; large/value-storage txs user-chooses).
- Mining payouts to shielded addresses: supported post-fork where the
  upstream plumbing allows; verify in P1.
- Keep privacy **optional and standards-based**. No ZK scope creep (README
  non-goals). Exchange compatibility note: "anon coin" positioning invites
  delistings; optional privacy + transparent rail keeps options open.

## 11.5 Masternodes: why classic ones are the wrong tool here

Dash-style masternodes exist for four jobs: InstantSend, PrivateSend,
governance, node rewards. Map them onto this fork:

| Masternode job | Verus-lineage equivalent | Verdict |
|---|---|---|
| InstantSend | none | We do not fake finality (§11.3) |
| PrivateSend (CoinJoin) | Sapling shielded txs | Redundant and worse |
| Governance | community process (doc 04) | Keep it out of protocol for now |
| Node rewards | VerusPoP staking already pays | Duplicate |

So a classic masternode adds nothing but a second reward claimant. If
"masternode" enters this project, it must be for a **job that doesn't
exist yet** — and the game layer (doc 12) is exactly where such jobs live.

## 11.6 Two candidate PoS extensions (the "cool unique" part)

### Direction A — Keeper nodes (service nodes for the game layer)

Bonded, opt-in staking role. Duties (all currently sidecar/pool-layer, no
consensus change if kept so):

- **Draw notarization**: cosign jackpot/league draw results (block hash +
  rule id) so any player can verify the game ran honestly;
- **Attestation relay**: sign and forward device-attestation verdicts
  (doc 03 §3.6) without ever seeing identity;
- **Index service**: replicate leaderboard/season state so the fun layer
  has no single host.

Economics: paid from **fun-layer budgets** (league pool slice — doc 06
§6.2 overflow already feeds it), never from inflation. Bond is slashable
for provable misbehavior (double-notarization, forged verdicts).

Why it's interesting: it entangles PoS security with *game integrity* — the
dealers are bonded by the same stake that secures consensus. No chain does
exactly this. [consensus?]: only if notarization must live on-chain; first
version stays pool/sidecar.

### Direction B — Season stakes (governance-lite)

Lock stake for a 4–8 week season → earn league-pool share (for uptime, not
for size alone) + one vote on **season presentation parameters** (prize
splits within published bounds, event themes, schedule). No consensus
parameters, no protocol governance — and say so loudly to avoid
vote-buying theater.

Why it's interesting: staking becomes season participation; the
"mine→stake→play" loop closes without any wagering. Low legal surface,
mostly sidecar.

**Recommendation:** B ships with Season 1 (cheap, safe, closes the loop).
A gets a P3 exploration charter — only if the game layer proves retention
(doc 01 success criteria) and needs decentralized trust. Do not build
keepers before the game is fun.

## 11.7 Decision points

| Question | Owner | When |
|---|---|---|
| Relay fee floor values + shielded guidance | P1 testnet | before public testnet |
| Wallet privacy defaults per amount tier | app design | P2 |
| Season stakes adoption (Direction B) | founder | before Season 1 |
| Keeper nodes charter (Direction A) | founder | P3, retention-gated |
| Any [consensus] promotion | doc 02 allowlist + review | per item |
