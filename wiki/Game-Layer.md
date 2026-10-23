# Game Layer

The games are public rules, not a website. No wagering anywhere — all
of this pays *miners and stakers* for securing the chain.

## Jackpot (PoW finders)

Every PoW block carries a pre-committed draw (`SHA256D(parent + height)`,
fixed before grinding — nothing can be withheld). Roughly 1 in 100
heights wins: the block gains a bonus up to 5× the reward, paid from the
jackpot reserve. The bonus is split among PPLNS contributors with
square-root weights and a 25% per-miner cap.

## Lucky Pot (PoW finders)

Roughly 1 in 1,000 heights triggers a splash of 10/25/50% of the pot
(tiers from the same pre-committed draw). A pity timer forces a 10%
splash after 4,320 dry blocks at most. The pot is capped; overflow
levy burns.

## Luckynode Draw (stakers)

Each epoch the chain draws a winner from live `ln-` identities and pays
them from the **Prize Fund** on chain — no pool custody, no cosigns.
Jackpot-levy overflow that used to be unspendable funds the Prize Fund.
See [Luckynodes](Luckynodes).

## Divisions & streaks

- **Streaks:** mine consecutive days; day counts survive quiet spells.
- **Divisions:** phone / cpu / gpu / farm boards on 30-day effective
  work. Self-declared family, policed by trailing rate.
- Division prizes are not paid in v1 — the boards are rank and status,
  funded by nothing. The prize money that exists is the Luckynode Draw.

Track everything on the [explorer rewards page](https://luckylode.cc/explorer/rewards):
jackpot/pot reserves, prize fund, the current draw, hits, splashes and
draw claims. Division boards mirror the pool.
