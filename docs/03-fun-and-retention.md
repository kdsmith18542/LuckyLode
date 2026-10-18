# 03 — Fun & Retention: The Excitement Layer (core differentiator)

Mining dies when it becomes invisible background process with dust payouts.
This doc designs mining as a **game with seasons, leagues, streaks, and
jackpots** — most of it *without* consensus changes (pool-side + sidecar),
so it ships fast and iterates faster.

> **Update 2026-10-17 (owner-directed):** the first-party casino that was
> briefly specified in doc 14 is **withdrawn** — tech-debt and risk outweigh
> the upside, and the crate/deployment are removed. The "no-wager" posture of
> this doc stands absolute again: the excitement layer is mining leagues,
> streaks, jackpots, the Lucky Pot, and the Luckynode Draw only.

## 3.1 Design principles

1. **A phone must be able to win something weekly.** If all rewards scale
   linearly with hashrate, EPYC owners take everything and phones churn.
   Non-linear rewards (lotteries, streaks, leagues) fix this.
2. **Never fake work.** Every reward must trace to verified shares/blocks.
   (Electroneum/Pi lesson.)
3. **Separate the competitions.** Phones don't race servers; they race
   phones. Leagues by device class.
4. **Seasons, not infinity.** 4–8 week seasons with resets keep newcomers
   from facing a 3-year leaderboard they'll never climb.
5. **Spectacle is a feature.** Public draws, live leaderboards, draw
   livestreams — witnessing wins retains more than winning does.

## 3.2 Block lottery (the one consensus change — port of a proven idea)

Every PoW block is a deterministic lottery ticket fixed before the find:
`draw = SHA256D(parent hash ‖ height)` — no VRF machinery, the parent's
PoW *is* the ticket, so a pool can advertise the exact bonus before it
commits a coinbase.

> **Full frozen spec: [doc 06](06-jackpot-lottery.md) rev 1.2.** Summary:
> jackpot iff `draw < 2²⁵⁶/100` (~1%); 5% per-block levy split 4% jackpot
> reserve / 1% Lucky Pot; jackpot pays base + up to `5×R` bonus
> (expectation-neutral, reserve-capped); pot splash `draw < 2²⁵⁶/1000`
> with tiered draws (10/25/50%) and a 4,320-block pity timer; pot
> cap-overflow burns, reserve overflow feeds the league prize pool
> (§3.3); **PoW-finder-only** — PoS blocks never claim (stakers get the
> Luckynode Draw instead, doc 06 §6.6); pools split progressively
> (sqrt-weight, 25% cap, PPLNS window), solo miners keep everything.
> Pool dashboards celebrate draws loudly — spectacle is specified in
> doc 06 §6.4.

## 3.3 Device-class leagues (pool-side first, consensus only if forced)

Classes (self-declared + statistically policed, see §3.6):

- 📱 **Phone league** (< ~10 MH/s sustained)
- 🖥️ **MiniPC/SBC league** (~10–60 MH/s)
- 💻 **Laptop/desktop league** (> 60 MH/s, capped — farms go elsewhere)

Each league has its own seasonal leaderboard (shares + uptime + streaks),
its own prize pool (sponsored from treasury/dev fund + pool fee slice), and
its own heroes. A phone finishing #3 in the phone league gets celebrated
like a pool finding a block. **Nobody cares they were 4,000th globally.**

Prizes: jackpot multipliers on their shares for a week, badge NFTs/IDs,
fee rebates, merch for season winners. Keep cash prizes modest — status >
money at these sums.

## 3.4 Streaks & habits (the retention engine)

- **Uptime streaks**: consecutive-day mining multipliers (pool-side share
  weighting +1%/day up to +30%, resets on 48h gap). Rewards routine.
- **Overnight mode** (mobile): bonus weight for charge-only sessions —
  leans into how phones actually mine.
- **Comeback bonus**: returning after 14+ days idle gets a 7-day boost.
  Churned users are the cheapest users to re-acquire.
- **Heating season events** (northern winter): themed sprints — mining for
  heat is a meme with real energy-economics behind it.

## 3.5 Badges, tipping, social (TRTL's playbook, modernized)

- **Achievement badges** as on-chain IDs/NFTs (free if LuckyID inherited):
  "First share", "30-day streak", "Phone-only month", "Solo block hero",
  season placements. Portable across pools.
- **Faucet + tipping in-wallet**: every wallet ships with one-tap tipping
  and a faucet claim. TRTL proved tipping culture *is* retention.
- **Solo-block hall of fame**: solo miners who find blocks get permanent
  leaderboard + outsized celebration. Keeps solo mining (decentralization)
  aspirational.

## 3.6 Anti-gaming (honest limits — perfect is impossible)

Hard truth: **a server can pretend to be 100 phones.** No protocol fully
prevents league spoofing. Mitigations, layered:

1. **Android attestation (pool-side):** Play Integrity verdicts prove
   genuine Android devices for the phone league. Android-only, but that's
   where the phones are. (Privacy-preserving: verdict only, no identity.)
2. **Statistical policing:** hashrate stability fingerprints, share-timing
   analysis, duplicate-IP clustering flag farm-split accounts for review.
3. **Sub-linear rewards (§3.2):** spoofing gains little by construction.
4. **Human review + community reports** for prize tiers, with public
   disqualification logs (transparency cuts both ways).
5. **Stake-to-enter prize tiers** (tiny, slashable on proof of spoofing):
   optional, Phase 2+.

Accept residual gaming in free tiers; harden only where money is on the line.

## 3.7 What lives where (consensus-minimalism)

| Mechanic | Layer | Consensus change? |
|---|---|---|
| Jackpot blocks + progressive split | Consensus | Yes (the one) |
| Leagues, leaderboards, seasons | Pool + sidecar | No |
| Streaks, comeback bonuses | Pool | No |
| Badges | IDs/NFTs (inherited) or sidecar | No |
| Faucet/tipping | Wallet + treasury | No |
| Attestation-gated league entry | Pool | No |

Rule: a full season of the game must be runnable with **zero hard forks**
after the lottery change ships.
