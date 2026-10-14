# 06 — Jackpot Lottery Blocks + Lucky Pot (spec, rev 1.2)

**Status:** rev 1.0 (2026-10-10) froze the jackpot — the one consensus
change in doc 02 §2.4(1): a deterministic per-block lottery with
progressive payouts. No VRF machinery, no committee, no extra grind
surface. **Rev 1.1 (2026-10-13)** added the Lucky Pot (§6.6), the
genesis game-fund seed, and the 4%/1% levy split. **Rev 1.2
(2026-10-14)** makes the draws **pre-committable** (parent-hash ticket):
the rev 1.0 digest-based trigger forced claim pools to commit a
bonus-bearing coinbase before knowing the draw and withhold ~99% of
their finds — uneconomic at real odds, so the reserve could never pay
(internal/16 §16.9). The payout math, caps, tiers, and levy are
unchanged.

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
  **league prize pool** (doc 03 §3.3) instead of burning — the two fun
  systems fund each other. Fully traceable on the dashboard (doc 04 §4.2).
- **Reorgs:** reserve accounting reverts with blocks like any state. No
  special casing.
- **Worked example** (`R = 16`, `F = 2`): levy `0.9`; normal payout
  `17.1`; jackpot payout `17.1 + min(80, reserve)`.

## 6.3 Distribution (pool-side recommended rule — consensus pays the pool)

Consensus pays the whole jackpot amount to the block finder (coinbase).
Splitting is pool policy, with one **reference rule** all reference-pool
software implements and every pool MUST publish (transparency is the
anti-scam mechanism):

- **Solo miners keep everything** (hall of fame, doc 03 §3.5).
- **Pools:** bonus split over a PPLNS window (recommended: shares from the
  trailing ~3 days), weight per miner `w_i = sqrt(shares_i)`, share capped
  at 25% of the bonus per miner, remainder redistributed pro-rata in one
  pass. Sub-linear by construction: splitting into 100 fake workers gains
  ~nothing; real small miners gain a lot.
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
2. Reserve neutrality: cumulative drift within **±5%** of the neutral
   expectation over the same window.
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
  next block takes the 10% floor, so the pot can't sit dead.
- **Cap + burn:** pot cap `200 × R`; levy that would exceed it is burned
  (unspendable by construction). The jackpot reserve's overflow instead
  feeds the league pool (§6.2) — different valves for different funds.
- **Who can win:** draws are gated to **PoW finders** in consensus (the
  locked 2026-10-13 rule); PoS blocks never claim. The pity splash lands
  on the next finder regardless of the draw class. Stakers get their game-layer luck through the **Luckynode
  Draw**, a separate pool-side event (internal/18) — the finder's prize is
  never diluted.
- **Testnet gates (extend §6.5):** ≥20,000 blocks with the pot event rate
  inside [0.05%, 0.2%], pity-timer firings as expected, and exact
  reserve/pot conservation (already KAT-pinned).
