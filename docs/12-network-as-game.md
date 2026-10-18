# 12 — The Network as a Game (deep-design exploration)

**Status:** exploration, 2026-10-11. This is the "what if using the network
felt like a casino" thought, taken seriously and written down safely. It
extends doc 03 (shipped mechanics) and doc 06 (jackpot spec). Nothing here
is frozen; every mechanic carries a funding rule and a legal gate.

## 12.1 The one law: variance-dressed labor, never wagering

> **Update 2026-10-17 (owner-directed):** the first-party casino that was
> briefly an exception to §12.1/§12.6 is **withdrawn** (doc 14 deleted —
> tech-debt and risk outweighed the upside). The no-wager law is absolute
> again: nothing in the product lets a player lose coins to the game.
> §12.6's "never casino" branding rule now covers every surface.

Every mechanic in this doc obeys:

1. **No player ever stakes coins on an outcome.** You cannot lose coins by
   playing. Prizes are funded by explicit budgets (§12.4), never by other
   players' losses.
2. **Participation is the ticket.** Mining, staking, sending txs,
   uptime — these buy chances. Coins do not.
3. **Honest odds, public math.** Draws are deterministic from public seed
   data; probabilities published; payouts dashboard-traceable (doc 04 §4.2
   discipline applied to fun money).

Why so strict: the moment a player can lose a coin to the game, this stops
being a game layer and becomes regulated gambling — with licensing,
geo-blocking, KYC, and app-store death (doc 04 §4.5). The strictness *is*
the product strategy: everything a casino does to you emotionally, minus
the part where you can lose.

## 12.2 Decomposing "casino feel" into fundable mechanics

What actually hooks players in gambling products, translated:

| Casino hook | Neutral translation (usable here) |
|---|---|
| Anticipation before a draw | Jackpot countdown per block; scheduled event draws |
| Near-miss drama | "Almost" feed: this block was 1.3× from a jackpot, this share 2 hashes away |
| Streaks and comps | Exists (doc 03 §3.4): streaks, comeback, seasons |
| Table spectacle | Livestreamed draws, hall of fame, public odds |
| Collection/identity | Badges as LuckyIDs (doc 03 §3.5) |
| Escalating stakes | Escalating *status tiers* (leagues), never coin stakes |
| Free chips / comps | Faucet games, tip roulette, rebate lotteries |

The translation rule: **chance stays, cost leaves.** Variance is free to be
thrilling when the downside floor is "you did some work and didn't win."

## 12.3 Mechanics catalog (ranked: impact × cost × safety)

1. **Fee-rebate lottery — "the network deals you in."** Every block, a
   deterministic draw from the block hash picks one included tx; its fee is
   rebated from a **rebate budget** (small league-pool/treasury slice).
   Sending anything at all makes you eligible; you never risk the fee
   because you'd have spent it anyway. Ships pool/sidecar first; on-chain
   refunds only if [consensus] ever promoted. *This is the core "using the
   network = playing" mechanic.*
2. **Near-miss feed + jackpot countdown** (sidecar/explorer). Pure
   spectacle, near-zero cost, feeds the doc 06 draws. Pairs with push
   notifications: "jackpot paid 40 s ago — here's the block."
3. **Mining bingo seasons.** 5×5 card of achievable squares (find block,
   7-day streak, overnight-only day, phone-class share, first block after
   upgrade). Full rows pay from the league pool; cards are free, generated
   per miner per season. Goal-stacking on top of doc 03's linear systems.
4. **Stake-streak draws (Direction B tie-in, doc 11 §11.6).** Stakers with
   uninterrupted (or attested) uptime across a window enter a weekly draw
   funded by the league pool. Mining and staking both feed the same season
   loop: mine blocks → stake proceeds → stake draws.
5. **Tip roulette nights** (community tooling, faucet-funded): during
   events, random tips to online league members at random intervals. TRTL's
   tipping culture as a scheduled spectacle.
6. **Ghost replays:** deterministic replay of a near-solved block draft
   ("0.4 s from a block") as an animated artifact. Cosmetic, delightful,
   zero economics.

**Explicit no-list** (never build, regardless of how fun): wagering pools,
prediction markets, pots funded by losing entries, stake-at-risk minigames,
"spin" mechanics converting coins→chances, jackpot tickets sold for money —
all conserved-structure violations by construction.

## 12.4 Funding conservation (who pays, always)

| Mechanic | Budget source | Rule |
|---|---|---|
| Jackpot bonus (exists) | levy → reserve, cap overflow (doc 06) | expectation-neutral, frozen |
| League prizes (exists) | reserve overflow + sponsored slice | prizefund ≤ budget, no debt |
| Rebate lottery | rebate slice of levy/treasury | Σ rebates ≤ slice · blocks |
| Bingo rows | league pool | per-season cap published before season |
| Stake draws | league pool | same cap line as bingo |
| Tips/faucet games | faucet budget (doc 03 §3.5) | no ICO money, no player deposits |

Every row is auditable from public dashboards; the accounting crate
(`lm-pool` league pool + `lm-rewards` reserve) already implements the two
existing rows. New rows are *allocations inside* those pools — no new money
sources appear, ever.

## 12.5 The unique PoS tie-in (the "never been done" bit)

Bring doc 11 §11.6 together with §12.3:

- **Season 1 (cheap version):** season stakes + stake-streak draws close
  the loop — *mine → stake → play* — with staking being both income and
  eligibility. Stakers become the game's sponsors-of-record (their staked
  uptime is what the draw celebrates), and season votes are limited to
  presentation parameters.
- **P3+ (deep version): Luckynodes** — bonded staking nodes that
  notarize draws, replicate verification, and hold the game layer honest.
  Bonded by the same PoS that secures the chain: misbehave and the bond
  burns; behave and the fun-layer budget pays you. Nobody has shipped
  "the game's trust anchors are stake-bonded luckynodes" as a first-class
  role. That is the unique thing — and it exists *because* of the hybrid
  consensus decision (2026-10-11), not despite it.

## 12.6 Legal & platform posture (honest, non-negotiable gates)

> **Withdrawn 2026-10-17:** the first-party casino exception (doc 14) is
> gone. The gates below govern **every** surface — there is no wagering
> product.

- No-wager design targets prize-competition framing, but **chance-based
  prizes are still regulated in many jurisdictions**; skill/chance mixes
  vary by country. Any mechanic with material prize value requires legal
  review before shipping — budget it like an audit.
- Safest class while small: badges, status, rebates, faucet-scale tips,
  free-to-enter draws with published odds. Keep actual prize value modest
  early (doc 03 already recommends status > money at these sums).
- **Branding discipline:** never "casino" in product surfaces or app
  stores. It is a *league with draws*. This is not squeamishness — it is
  the difference between shipping and store rejection (doc 04 §4.5).
- App stores: keep any lottery-adjacent surface in the web dashboard first;
  app binaries ship mining, wallet, leagues — not draws with prizes.

## 12.7 Sequencing

1. **Testnet:** near-miss feed + countdown (pure UX), rebate-lottery
   simulation in pool software (no real value).
2. **Season 1:** bingo, stake draws, season stakes (doc 11 Dir B); capped
   prizes; legal review of the Season-1 prize set.
3. **P3+:** luckynodes charter (doc 11 Dir A) — gated on Season 1–2
   retention evidence and real prize value only after review.

## 12.8 Open questions

1. Prize-value ceiling before formal legal review (candidate: sub-$5
   equivalent, status-weighted, no cash-out path beyond coin itself)?
2. Rebate lottery on-chain ([consensus], audit cost) vs pool-credited
   (today's recommendation) — revisit only if pool-credited can't be
   trusted at scale.
3. Naming decided 2026-10-13: **Luckynodes** (the bonded service node)
   and **stakers** (the people who run them; replaces "keepers"). Legal
   input still applies at charter time.
4. Does the rebate lottery create a tx-spam incentive (cheap lottery
   tickets)? Mitigate: rebate ≤ fee paid, eligibility weighted by confirmed
   value/min fee rate — design detail for the simulation phase.

## 12.9 Surfaces & operations: official website "play" section + official pool

**Status:** design exploration (2026-10-11, project direction). Same law as
§12.1; branding rules of §12.6 apply everywhere below.

### The official pool (bootstrap role, not an empire)

An official project pool is wanted for three legitimate reasons — and must
be constrained against the failure mode doc 01 already names (pool
monoculture):

**Why:** (1) day-one onboarding — a good pool must exist before third
parties build one; (2) the reference pool in production is the fun layer's
best-integrated home (leagues/streaks/draws are pool-side, doc 03 §3.7);
(3) it realizes the dev-funding leg of doc 04 §4.2(2) — the reference-pool
fee slice — with dashboard-traceable coins.

**Neutrality commitments (published, table stakes):**

- Same published fee schedule as the reference default; no official-pool
  exclusives. Every fun mechanic is an open spec (`lm-pool` ledger rules) —
  other pools implement identical rules; nothing works only on the official
  pool.
- **Live hashrate-share dashboard.** If official share exceeds ~40%
  sustained for a week: fee → 0, public notice, community asked to spread.
  Committed in writing before public testnet.
- League participation never requires the official pool. Cross-pool season
  boards aggregate from opt-in pools' public APIs via the sidecar.
- Solo-mining guide + reference miner are hosted with equal prominence
  (doc 01 success criteria: "no pool monoculture"; the official pool does
  **not** count toward the ≥3-independent-pools criterion — target is then
  ≥4 pools total).

**Ops posture:** project-multisig for fee collection (doc 04 §4.2), hot
wallet float policy documented, uptime + DDoS plan before Season 1,
incident post-mortems public (testnet discipline, doc 04 §4.3).

**Hosting plan (2026-10-11, hosting shortlist):**

- *Bootstrap (now):* one 6-core/12 GB/100 GB box (~$12/mo) — node (8 GB
  comfortable for the Verus-lineage daemon) + reference pool + website/
  sidecar. One firewall, one ops surface while the chain is tiny. Spare
  cores serve `lm-miner` benchmark runs.
- *Public testnet (Season 0–1):* split — dedicated 2-core/4 GB box
  (~$4.50/mo) for the pool (the only DDoS-visible service, own blast
  radius, restartable without touching the node); node stays private
  (RPC localhost-bound); website frontend static on a free CDN with the
  sidecar API served from the pool box. ~$17/mo total.
- *Later:* a few cheap 4 GB boxes as geographically spread bootstrap
  seed nodes — cheap decentralization, good optics for a network that
  preaches it.
- Day-one hardening: SSH keys + fail2ban, only pool port + 443 exposed,
  daily backups on, provider snapshots + DDoS protection before the pool
  is public.

### The website "play" section (name TBD, definitely not "casino")

The web dashboard is already the designated home for draw-adjacent surfaces
(§12.6): it can host everything below with **zero keys, zero deposits,
read-only by design** — connect by address (watch-only) or signed message,
never a private key.

**First wave (testnet → Season 1):**

- Live jackpot feed + countdown, near-miss feed (§12.3.2)
- League boards, season progress, hall of fame, badge gallery
- Bingo cards + stake-draw eligibility (personal view)
- Tip-roulette event nights (faucet budget), rebate-lottery feed

**Data flow (no consensus, no new trust):** pool API + chain indexer →
fun-layer sidecar (doc 02 §2.3) → website. The sidecar is the aggregation
point for cross-pool boards; the website is a client. Personal actions
(tips, plays) hand off to the wallet via deep link/QR — the site never
touches funds.

**Naming candidates** (branding decision, legal input): The Arcade,
Clubhouse, The Floor, League Hall. Avoid: casino, betting, wager, odds*
(*publish probabilities, but as "chances", per §12.6 language discipline).

**Gates:** same as §12.6 — any surface with material prizes clears legal
review first; store binaries keep shipping mining/wallet/leagues only.

## 12.10 Luck classes ("ten kinds of jackpots") & prize flow

**Status:** exploration with a working prototype (`lm-rewards::luck`,
21 tests green incl. exact conservation + simulation). Adoption requires a
doc 06 rev 1.2 (freeze + per-class gates) — nothing here is consensus yet.

### The fee wall (decided posture)

- **No fee buys odds.** Paying above the floor buys service (priority,
  inclusion), never chances. Fee→odds is a lottery trigger ("consideration")
  and converts the fun layer into regulated gambling. Hard line.
- **No fee buys privacy.** Shielded txs are equally anonymous at every fee.
  Inventing a "premium anon" tier implies cheaper txs are less private —
  false technically and poisonous optically. Privacy is free for everyone,
  or it is nothing.
- **Allowed fee hooks:** (a) *rider rebate* — if the block containing your
  tx wins any luck class, all its txs get fees rebated (capped at fee paid;
  odds identical for every tx, free with inclusion); (b) *patron status* —
  voluntary fee overflow donated to the rebate budget earns a cosmetic
  badge, never advantage; (c) *lucky hours* — scheduled odds boosts funded
  by the league pool, published calendar, same odds for everyone (free-to-
  enter event, not a purchase).

### The luck table (ten classes, one levy)

Replaces the single 1/100 → 5R class (doc 06 §6.1) with ten nested
thresholds on the same PoW hash — same ticket, reshaped variance, **same
5% levy, zero new money**. Rarest-cleared class wins (partitioned draws).

| Class | Odds | Bonus | Net hits @1440 blocks/day |
|---|---|---|---|
| spark | 1/10 | 0.2×R | ~86/day |
| glint | 1/25 | 0.25×R | ~29/day |
| flare | 1/50 | 0.5×R | ~14/day |
| ember | 1/100 | 1×R | ~8.6/day |
| cinder | 1/250 | 2.5×R | ~2.9/day |
| bonfire | 1/500 | 5×R | ~1.4/day |
| beacon | 1/1000 | 6.25×R | ~0.9/day |
| comet | 1/2500 | 12.5×R | ~1 per 2.3 days |
| aurora | 1/10000 | 25×R | ~1 per 7.7 days |
| mythic | 1/100000 | 225×R | ~1 per 69 days |

**Conservation (the subtle part):** rarest-wins *partitions* draws, so a
class's probability is `1/N − 1/N_prev`, not `1/N`. The table is solved
against partition weights: `Σ bonus_millis × w_i = 5_000_000` (= 5% levy ×
100k blocks) — integer-exact, asserted in `lm-rewards::luck`. (A naive
`Σ bonus/N` table underpays by ~40%; the test caught exactly that.)

**Consequences before adoption:** reserve cap must cover the largest class
(≥225R, vs doc 06's 50R) or prizes clip via `min(prize, reserve)` during
warm-up — resize consciously in rev 1.1; per-class rate gates join §6.5.

### Prize flow: auto-pay on-chain, claim-and-expire off-chain, luckynode stakers never custody

- **On-chain (luck classes, jackpot, league account credits): paid
  instantly to the block finder in the coinbase** (doc 06 §6.3). No claim
  step at consensus — ever. A claim mechanic would create phantom pots,
  abandonment accounting, and a regulator-shaped hole at the consensus
  layer. The reserve already functions as the progressive pot: it grows
  every block and big classes draw it down visibly.
- **Off-chain prizes (league payouts, sponsor prizes, event prizes):
  claim-based with a published 90-day expiry**, push-notified via wallet,
  pool, and website. Expired prizes flow to the league pool (auditable) →
  next season's events. "Unclaimed grand prize funds next season" is the
  honest rollover; *never* random-recipient redistribution (un-auditable,
  scam optics, and randomness belongs in draws, not money routing).
- **Luckynodes (doc 11 Dir A) may automate notification and notarization of
  claim windows — they never custody funds.** Custody would create a
  honeypot and push operators into money-transmitter territory. On-chain
  money is already paid; off-chain money moves multisig → winner address
  under a published rule that no luckynode staker can redirect.

### Adoption sequencing

Simulation in pool software on testnet (no real value) → legal review of
the Season-1 prize set → doc 06 rev 1.2 (table, reserve resize, gates) →
consensus only after review. The `lm-rewards::luck` module is the seed of
that revision's executable spec.


## 12.11 Lucky events calendar (designed 2026-10-11)

**Principle: events amplify existing systems; they never add new ones.** Every
event ships with a published budget cap *before* it runs, explicit funding
from a named pool, the §12.1 no-wager law untouched, a kill-switch, and a
four-week retention read. If it doesn't move engagement, it retires — no
zombie mechanics.

### A. Amplification events (boost what already exists)

- **Lucky Hours** — 2–4 h windows where luck-table thresholds tighten
  (e.g. mythic 1/100k → 1/25k). The odds delta is an explicit cost paid
  from the league pool; calendar public in advance; 1–2 per week max.
- **Seasonal luck tables** — each season reshapes the class mix *inside the
  same levy envelope* (winter boosts aurora; summer adds a "meteor shower"
  class). Seasons feel mechanically different without changing total
  expectation. Re-audited each season.
- **Double-rebate nights** — tx rider-rebate multiplier for the window,
  funded from the rebate budget. Rewards using the network during events
  (the doc 12 §12.3 core hook), still zero fee→odds selling.

### B. Milestone events (community-wide, threshold-triggered)

- **Rain** — when a public threshold is hit (block height, miner count,
  wallet count), a fixed league-pool budget drops across recent active
  miners by PPLNS weight. Announced in advance; one-time per threshold.
- **Eclipse blocks** — every N blocks (e.g. 10 000), one scheduled block's
  draw splits across the top contributors of its PPLNS window instead of a
  single finder. Group spectacle: everyone's watching the same block.
  Fully visible in the calendar beforehand.

### C. Community/social events (low prize value, high cheer)

- **So-Close prize** — weekly: the block *closest* to a mythic (without
  hitting) wins a small consolation from the league pool. Manufactures
  near-miss drama from real data (§12.3.2).
- **Tip roulette nights** — faucet-funded random tips to online league
  members (already catalogued); runs as event nights.
- **Ghost replay matinees** — scheduled replays of the week's near-blocks
  and mythic hits as animated artifacts.
- **Bingo season cards** — per-season cards, rows pay from league pool.
- **Stake-uptime draws** — weekly draw among stakers with uninterrupted
  attested uptime (doc 11 §11.6 Direction B tie-in; the mine→stake→play loop).
- **Luckynode-notarized draws** — when luckynodes exist (doc 11 Direction A,
  P3+), *all event draws* get cosigned notarization so any player can
  verify the game ran honestly. Luckynode stakers never custody, never lift odds.

### Cadence guardrails (anti-bloat, the part that protects the fun)

1. **One headline event at a time, ever.** Boosts never stack; a block is
   never under two odds multipliers at once.
2. **Marquee ≤ 1 per season; weekly slots rotate** between Lucky Hours,
   So-Close, and stake draws.
3. Every event = cap + cost + kill-switch + 4-week metric, on the
   dashboard (doc 04 §4.2 discipline applied to fun money).
4. Ledger of record: all settlements flow from the league pool
   (`lm-pool`) or rebate/faucet budgets; consensus-covered prizes auto-pay,
   off-chain prizes claim-and-expire (§12.10). **No event ever touches
   player principal.**

### Rejected by the law (for the record)

Buy-in tournaments, paid spins, wager-backed pots, prediction markets,
ad-hoc random-recipient drops, odds-for-fees (§12.10 fee wall), anything
with a rake. Also rejected: stacking events to manufacture hype — the
calendar's scarcity is what keeps any single event special.
