# 06 — Jackpot Lottery Blocks (spec, FROZEN rev 1.0)

**Status:** frozen 2026-10-10. This is the one consensus change in doc 02
§2.4(1): a deterministic per-block lottery with progressive payouts. No
VRF machinery, no committee, no extra grind surface — the PoW hash *is*
the lottery ticket.

## 6.1 Trigger (no new cryptography)

A block at height `h` with PoW digest `out` (EmberHash v1, doc 05) and
difficulty target `T` is a **jackpot block** iff:

```
uint256_be(out) < floor(T / JACKPOT_ODDS),   JACKPOT_ODDS = 100
```

- The division is exact integer floor on the 256-bit target; verifiers
  recompute identically (same discipline as doc 05 §5.3).
- ~1% of blocks, independent of difficulty — the odds never drift as
  hashrate moves. At `T_block = 60 s`, expect ~14 jackpots/day (Poisson):
  roughly hourly spectacle.
- Untargetable: each lottery "ticket" costs a full PoW hash. A miner cannot
  test jackpot-ness without doing the work, and withholding a jackpot block
  to solo-mine it later just risks losing it to the network (same
  incentives as normal blocks, plus the whole world watching).

## 6.2 Money (expectation-neutral by construction)

Per block, with base reward `R` (emission schedule, doc 04) and fees `F`:

| Param | Value |
|---|---|
| `LEVY_PERMYRIAD` | 500 (5% of `R + F` into the jackpot reserve) |
| `JACKPOT_BONUS_MULT` | 5 (bonus = `5 × R`, paid from reserve) |
| `JACKPOT_RESERVE_CAP` | `50 × R` (current `R`) |
| Normal block payout | `R + F − levy`; reserve `+= levy` |
| Jackpot block payout | `(R + F − levy) + min(5R, reserve)`; reserve `−= paid bonus` |

Neutrality: expected bonus outflow per block = `0.01 × 5R = 0.05R` =
expected levy inflow `0.05(R+F)` minus a hair (fees make it slightly
positive-sum — fine, fees feed the spectacle).

- **Warm-up:** reserve starts at 0, so early jackpots pay thin
  (`min(5R, reserve)`). Documented, not patched — the first big jackpot
  landing weeks in is itself an event.
- **Overflow:** levy that would push the reserve past the cap flows to the
  **league prize pool** (doc 03 §3.3) instead of burning — the two fun
  systems fund each other. Fully traceable on the dashboard (doc 04 §4.2).
- **Reorgs:** reserve accounting reverts with blocks like any state. No
  special casing.
- **Worked example** (`R = 100`, `F = 2`): levy `5.1`; normal payout
  `96.9`; jackpot payout `96.9 + min(500, reserve)`.

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
