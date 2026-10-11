# 01 — Thesis & Positioning

## The gap

Every "mobile mining" project to date is either **dead** (TurtleCoin —
chain halted March 2023), **simulated** (Electroneum, Pi — no hashing
happens on the phone), or **a feature of a bigger chain** (Verus mobile
mining works, but Verus markets itself as an infra platform, not as *the*
low-end miner's coin). Nobody owns the identity: *"the coin you mine on
whatever you already own."*

Meanwhile the raw facts favor it:

- A modern phone does **5–7 MH/s on VerusHash 2.2** — roughly a tenth of a
  Ryzen 9 7950X and *faster* than a desktop Celeron/i3 of a few years ago.
- MiniPCs (N100/RK3588-class) and old laptops are sitting idle in millions
  of homes. RK3588 does ~7.5 MH/s.
- Lightweight PoW designs exist and are proven: TurtleCoin's **Chukwa**
  (Argon2id, 1 thread, 1 MB) was about as phone-ideal as PoW gets.

## Who it's for (personas)

1. **The hobbyist** — runs the miner on a miniPC next to the router, checks
   the dashboard like a tamagotchi. Pays for electricity, maybe. Here for fun.
2. **The phone dabbler** — installs the Android app, mines while charging
   overnight. Here for novelty + lottery dreams.
3. **The tinkerer** — old ThinkPad, Raspberry Pi cluster, underclocking
   experiments. Here to optimize and show off.
4. **The community native** — Discord/Reddit tip culture, faucet games,
   leagues. Here for people.

Explicitly **not** for: farms, ASIC operators, yield mercenaries. The design
should make them unwelcome or at least unadvantaged (see doc 03).

## Why predecessors faded (and the lesson each teaches)

| Project | What happened | Lesson |
|---|---|---|
| TurtleCoin | Great community + tipping culture, but dev burnout, no sustainable funding, chain halted 2023 | Fun without funding dies. Ship a dev-funding mechanism day one that doesn't break fair-launch trust (doc 04). |
| Electroneum | Simulated mining → users felt lied to once understood; app decayed to a wallet | Never fake the mining. If the phone hashes, say so; if it can't (iOS), offer honest non-mining roles. |
| Pi Network | Massive "miner" count, zero real mining → permanent credibility asterisk | Distribution ≠ mining. Don't call it mining unless work is verified. |
| Verus mobile | Real, but buried inside a platform story; VRSC economics don't reward phone miners (~$0.01/day) | A phone miner earning dust inside a server-dominated reward pool quits. **Segment rewards by device class** (doc 03) — this is the single biggest retention idea. |
| Generic CPU coins (Monero etc.) | Botnets + server farms squeeze hobbyists out | Expect adversaries; design leagues and lotteries so a phone can still *win* (doc 03). |

## Positioning statement (draft)

> *Lucky Lode is the fair-launch coin mined on everyday hardware — phones,
> miniPCs, laptops — where small miners compete in their own leagues and
> every block can be a lottery win. No premine, no ICO, no ASICs, no faking.*

## Success criteria (12 months post-launch)

- 10k+ distinct pool accounts with < 50 MH/s (i.e. real low-end, not farms).
- Median low-end miner still active at day 90 (retention, not just signups).
- ≥ 3 independent pools + solo-mining guides (no pool monoculture).
- Funding runway ≥ 18 months from transparent mechanisms (doc 04).
