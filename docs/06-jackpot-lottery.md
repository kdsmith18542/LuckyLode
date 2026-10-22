# 06 — Jackpot Lottery Blocks + Lucky Pot (spec, rev 1.3)

**Status:** rev 1.0 (2026-10-10) froze the jackpot — the one consensus
change in doc 02 §2.4(1): a deterministic per-block lottery with
progressive payouts. No VRF machinery, no committee, no extra grind
surface. **Rev 1.1 (2026-10-13)** added the Lucky Pot (§6.6), the
genesis game-fund seed, and the 4%/1% levy split. **Rev 1.2
(2026-10-14)** makes the draws **pre-committable** (parent-hash ticket):
the rev 1.0 digest-based trigger forced claim pools to commit a
bonus-bearing coinbase before knowing the draw and withhold ~99% of
their finds — uneconomic at real odds, so the reserve could never pay
. The payout math, caps, tiers, and levy are
unchanged. **Rev 1.3 (2026-10-21)** makes the prize fund real consensus
money and moves the Luckynode Draw from pool-side v1 into consensus
(§6.7) — stakers' game-layer income must not depend on any pool
(owner direction: the pool is hashrate aggregation; users choose pool,
wallet, or local node).

## 6.1 Trigger (pre-committable, no new cryptography)

A block at height `h` on parent `P` is a **jackpot block** iff:

```
draw = SHA256D(P.hash_internal || h_le32)          # uniform over 2^256
uint256_be(draw) < floor(2^256 / JACKPOT_ODDS),   JACKPOT_ODDS = 100
```

- The ticket is the **parent block's hash** mixed with the height — fixed
  before grinding. A pool knows the draw class of the next height at
  template time, builds the claim coinbase for that height, and **every
  find lands** (no withholding; the node validates the same formula).
- Exactly 1/100 of heights, independent of difficulty: a fixed fraction of
  the full 256-bit space, so the odds never drift as hashrate moves.
  SHA256D mixing keeps the digest uniform even though PoW parents are
  target-bounded and PoS parents are not.
- Untargetable: `hashPrevBlock` is fixed by the parent's PoW. Grinding a
  parent whose child is a jackpot costs ~100 parent-blocks of work for one
  bonus (irrational). Jackpot heights are public ~60 s in advance — a
  countdown for the dashboard, not an exploit.
- PoW-only (locked 2026-10-13): PoS blocks never claim; the draw is
  defined for every height but consensus gates it to PoW blocks.

## 6.2 Money (expectation-neutral by construction)

Per block, with base reward `R` (emission schedule, doc 04) and fees `F`:

| Param | Value |
|---|---|
| `LEVY_PERMYRIAD` | 500 (5% of `R + F`), split **4% jackpot reserve / 1% Lucky Pot** (§6.6) |
| `JACKPOT_BONUS_MULT` | 5 (bonus = `5 × R`, paid from reserve) |
| `JACKPOT_RESERVE_CAP` | `50 × R` (current `R`) |
| Normal block payout | `R + F − levy`; reserve `+= levy` (up to cap) |
| Jackpot block payout | `(R + F − levy) + min(5R, reserve)`; reserve `−= paid bonus` |

Neutrality: expected bonus outflow per block = `0.01 × 5R = 0.05R` =
expected levy inflow `0.05(R+F)` minus a hair (fees make it slightly
positive-sum — fine, fees feed the spectacle).

- **Genesis game-fund seed (rev 1.1):** the reserve and pot start seeded
  at height 0 from a consensus-locked game fund — jackpot **8,000 LKYL**
  (100 max-payouts at the 16 LKYL peak) + pot **1,000 LKYL** — ≈0.1% of
  year-one issuance, with no keys and no address: spendable only through
  the jackpot/pot payout rules (verifiable from genesis by any node). The
  seed front-loads game payouts into the launch window, where they reward
  launch miners proportionally to hash. Organic levy takes over as it
  drains.
- **Overflow:** levy that would push the reserve past the cap flows to the
  **prize fund** (§6.7). Before rev 1.3 the overflow was unspendable
  bookkeeping that pools tracked privately; rev 1.3 mints it into the
  consensus prize fund, spendable only by Luckynode Draw claims. Fully
  traceable on the dashboard (doc 04 §4.2).
- **Reorgs:** reserve accounting reverts with blocks like any state. No
  special casing.
- **Replay-derivability invariant (added 2026-10-17, after two live
  incidents):** every consensus field this system adds to block-index
  state (reserve, drought/pity timer, pot, prize inflow) must be
  *rebuildable by replay* — a fresh reindex of the chain reproduces it
  byte-for-byte. Serialization/reload paths (e.g. `LoadBlockIndexGuts`)
  are consensus code, not plumbing: a field that survives in memory but
  not on disk (or vice versa) is a consensus bug. Same rule for the
  pool-side credit-once gates: no backfill, no guessing across gaps.
- **Worked example** (`R = 16`, `F = 2`): levy `0.9`; normal payout
  `17.1`; jackpot payout `17.1 + min(80, reserve)`.

## 6.3 Distribution (pool-side recommended rule — consensus pays the pool)

Consensus pays the whole jackpot amount to the block finder (coinbase).
Splitting is pool policy, with one **reference rule** all reference-pool
software implements and every pool MUST publish (transparency is the
anti-scam mechanism):

- **Solo miners keep everything** (hall of fame, doc 03 §3.5).
- **Pools:** bonus split over the last **2,000 accepted shares**
  (traditional PPLNS, `PPLNS_WINDOW_SHARES`), aggregated to payout
  identity first, weight per miner `w_i = sqrt(shares_i)`, per-miner cap
  of 25% of the bonus, and the capped excess **water-filled** (a fixpoint:
  capped miners lock, the remainder re-flows pro-rata until no cap is
  breached — not a single redistribution pass). Sub-linear by
  construction: splitting into 100 fake workers gains ~nothing; real
  small miners gain a lot. This is the reference rule the reference pool
  implements; a pool deviating from it MUST publish its own rule.
- **Pool-hopping:** the PPLNS window is the defense — arriving after a
  jackpot earns nothing from it, leaving forfeits future windows. No
  protocol change needed.

## 6.4 Spectacle & verifiability

- Anyone verifies a jackpot from header + target alone
  (`lm-pow::meets_target` against `T/100` — same helper, §6.1).
- Sidecar/explorer emits jackpot events (webhook + dashboard fanfare);
  draws are livestream-friendly by construction.
- Pools MUST display the active rule + live reserve balance; hiding either
  is a pool-listing delisting offense (reference pool directory policy).

## 6.5 Testnet gates (before mainnet)

1. Simulate ≥ 20,000 blocks: jackpot rate within **[0.5%, 2%]** (Poisson
   tolerance around 1%).
2. Reserve balance: within **±5%** of the value the 4/1 levy split
   replays to over the same window. (The original "neutrality"
   expectation — that the reserve hovers at a fixed point — predates
   the rev 1.2 split; with the pot leg taking a fifth of the levy the
   reserve trends to its own attractor, so the gate compares against a
   replayed reference, not a neutrality constant.)
3. Warm-up drill: reboot reserve to 0 mid-testnet, confirm thin early
   jackpots + recovery, no consensus faults.
4. Reference-pool split drill: publish a jackpot distribution with the
   §6.3 rule, verify sqrt/cap math against an independent implementation.

## 6.6 Lucky Pot (added rev 1.1, 2026-10-13)

A second, rarer stochastic sink beside the jackpot — jackpot = frequent +
small-ish, pot = rare + tiered. Same levy, independent draw, no new
issuance. Executable spec: `lm-rewards/src/pot.rs`; consensus:
`lucky_pot.h`; regression gate: the POT KAT.

- **Funding:** the 5% levy splits **4% jackpot reserve / 1% Lucky Pot**
  (`nLuckyReserve` beside `nJackpotReserve`).
- **Trigger:** a height is a pot event iff
  `uint256_be(draw) < floor(2^256 / POT_ODDS)`, `POT_ODDS = 1000` (~0.1%
  of heights), same pre-committable `draw` as §6.1. Tier = `draw mod 100`:
  **0–69 → 10%** of the pot; **70–94 → 25%**; **95–99 → 50%**. Floor 10%,
  never the whole pot.
- **Pity timer:** after **4,320 event-less blocks** (~3 days at 60 s) the
  next block takes the 10% floor, so the pot can't sit dead. Only pot
  events reset the counter — a jackpot win on a dry run leaves the
  drought counting (live-proven: block 643 paid a jackpot bonus while
  the drought kept counting toward the next pot event). Rationale
  for a scheduled-looking payout: the pot's inflow is a fixed levy
  fraction and its outflow is a fixed tier fraction of the *balance*,
  so a forced 10% grab is bounded — it cannot drain the fund, only
  truncate droughts; the alternative (folding the drought into the draw
  digest) would make the payout depend on future blocks and break the
  pre-committed-ticket property. PoW finders only, like every pot
  payout.
- **Cap + burn:** pot cap `200 × R`; levy that would exceed it is burned
  (unspendable by construction). The jackpot reserve's overflow instead
  feeds the prize fund (§6.2) — different valves for different funds.
- **Who can win:** draws are gated to **PoW finders** in consensus (the
  locked 2026-10-13 rule); PoS blocks never claim. The pity splash lands
  on the next finder regardless of the draw class. Stakers get their game-layer luck through the **Luckynode
  Draw**, a separate pool-side event — the finder's prize is
  never diluted.
- **Testnet gates (extend §6.5):** ≥20,000 blocks with the pot event rate
  inside [0.05%, 0.2%], pity-timer firings as expected, and exact
  reserve/pot conservation (already KAT-pinned).

## 6.7 Prize Fund + consensus Luckynode Draw (added rev 1.3, 2026-10-21)

The stakers' game layer, promoted from pool-side v1 to consensus. No
pool, website, or third party sits between a staker and their draw
tickets; pools become pure hashrate aggregation and may attach claims
like any other block builder.

### 6.7.1 The fund

- Third consensus fund on the block index (`nPrizeReserve`), alongside
  the jackpot reserve and the Lucky Pot.
- **Credited** every block with the jackpot-levy slice that overflows
  the reserve cap (the 4% overflow — previously unspendable). No cap
  of its own: draws are the drain, and the per-epoch budget slice
  bounds outflow.
- **Genesis seed:** 1,000 LKYL at height 0, same consensus-locked
  pattern as the other game funds (no keys, no address; spendable only
  through draw claims). Mainnet value is finalized at the ceremony.
- **Debited** only by draw claims, exactly as claimed. Unclaimed
  budgets roll forward — the fund accumulates, it never vaporizes.
- New conservation identity (extends §6.2):
  `miner + Δreserve + Δpot + Δprize + to_burn == R + F`, where the
  coinbase ceiling grows to
  `normal + jackpot bonus + pot splash + prize claim`.

### 6.7.2 Registration (LuckyID namespace — no new chainstate)

A luckynode is an **on-chain identity** in a reserved top-level
namespace: names beginning `ln-` (exact pattern enforced in identity
name validation; the prefix is reserved by the protocol, not by a
controller). Registering `ln-<yourname>@` **is** registration:

- **Payout address:** the identity's primary address.
- **Liveness (heartbeat):** an `updateidentity` that bumps a heartbeat
  counter in the content map under the reserved key. The daemon sends
  one on cadence (the update tx **is** the approved heartbeat tx).
  Liveness window: last update within one epoch. Identities already
  revert with reorgs by construction — zero new index machinery.
- **Tenure:** first `ln-` registration height; tiers unchanged
  (1× <30d, 1.5× 30–90d, 2× ≥90d, in weight-millis).
- **Sybil economics:** identity registration + renewal + update fees
  are the entry cost (the pool-era refundable bond is retired at the
  consensus level — a refundable escrow would need new chainstate for
  no integrity gain; name fees are real, non-refundable cost).
- **Eligibility at a boundary:** namespace identities whose latest
  update is within the liveness window, sorted by identity ID.

### 6.7.3 The draw (pre-committed, per epoch)

- Epoch `E` spans heights `[E·epoch, (E+1)·epoch)`; `epoch = 2880`
  mainnet (~48 h), 3 on testnet — unchanged from the v1 cadence.
- **Seed:** `SHA256D(blockhash(E·epoch − epoch) ‖ (E+1)·epoch)` — the
  epoch-start block hash plus the boundary height, fixed a full epoch
  before the draw. Same pre-commitment posture as §6.1: nothing to
  withhold, no grind surface.
- **Winner:** `u64(seed[..8]) % Σweights`, walked id-ascending — the
  exact v1 `draw_winner` math (mirrored in `lm-luckynode`), computed
  over the entrant set as of the boundary's parent state.
- **Budget:** `floor(trailing-epoch prize inflow × 1000 / 10000)` (10%
  slice, frozen — v1's `DEFAULT_LUCKYNODE_SLICE_BPS`) **plus a fixed
  seed release** of `seed / 1000` per epoch (1 LKYL at the 1,000 LKYL
  seed — the fund's front-load, draining the seed over ~1,000 epochs;
  a pure inflow-slice budget would recycle organic inflow forever while
  the seed sat dead), capped at the fund balance. Zero inflow still
  pays the release; the slice scales with hashrate. No pity timer —
  the formula self-throttles.
- **Claim (directed, like the seed funds):** any block in epoch
  `E+1` may carry a coinbase output paying the **winner's registered
  primary-address script**, up to the epoch budget. `ConnectBlock`
  recomputes the winner from parent identity state and validates the
  script and amount — a computed pin, not a chainparams one.
  Credit-once per epoch. Whoever builds the block (pool, solo miner,
  staker, wallet) attaches it; it costs the builder nothing and pays
  the winner, so there is no withholding incentive.
- **Dedicated claim output (builder rule):** the directed-sum check
  counts every coinbase output paying the winner's script, so the claim
  must ride its own output. A builder whose normal payout already uses
  the winner's script (e.g. `-mineraddress` set to the registered
  primary address) skips the claim for that block instead of building a
  block validation will refuse; the budget stays available to another
  block in the epoch.
- PoS-mined blocks carry claims exactly like PoW blocks — the draw is
  the stakers' layer.

### 6.7.4 Gates and migration

- **KAT gate 7** (`lucky_prize_kat`): pure-math pins — fund
  transitions (credit/claim/rollover conservation across epochs),
  budget math, and winner-selection vectors, mirrored in
  `lm-rewards::prize` (Rust) and the C++ header.
- **Replay-derivability invariant (§6.2) applies in full:** the fund
  balance, per-epoch claim state, and the identity-derived entrant set
  must rebuild byte-for-byte from a reindex.
- **Migration:** testnet chain reset (new block-index fields), same
  procedure as prior resets. Pool-side prize ledger + draw loop are
  **deleted** (the pool attaches consensus claims from GBT advisories
  exactly as it does jackpot/pot). The luckynode daemon and wallet
  switch registration/heartbeat to identity RPCs; the pool's HTTP
  register/heartbeat/notarize routes retire (draws are self-verifying
  from chain state — notarization remains optional spectator
  evidence).
- **Pool consumer shipped (unit 3a):** the reference pool splices the
  node's exact `prizedraw` remaining to the winner's script (dedicated
  output, same collision skip as the internal miner), records claims
  third-party-only (outside the payout identity), and surfaces them on
  `/api/stats` + the dashboard. Pool-side prize money, the v1 draw
  loop/registry/routes, and `--luckynode*` flags are deleted;
  per-height prize inflow stays as the explorer's display feed.
  Pending: daemon/wallet identity-RPC flows (registration +
  heartbeats + a consensus-draw history surface for the wallet tab),
  testnet reset #5, VPS redeploy.
