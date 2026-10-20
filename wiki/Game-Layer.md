# Game Layer

The chain is the casino cage and the games are public rules, not a
website. No wagering anywhere — all of this pays *miners and stakers*
for securing the chain.

## Jackpot (PoW finders)

Every PoW block carries a pre-committed draw (`SHA256D(parent + height)`,
fixed before grinding — nothing can be withheld). ~1% of heights win:
the block gains a bonus up to 5× the reward, paid from the jackpot
reserve, split by PPLNS like the block itself.

## Lucky Pot (PoW finders)

~0.1% of heights trigger a splash of 10/25/50% of the pot (tiers from
the same draw). A pity timer forces a 10% splash after 4,320 dry blocks
max. Reserve overflows feed the league pool instead of burning.

## Luckynode Draw (stakers)

Each epoch a draw over heartbeating luckynodes pays 10% of trailing
league inflow to one winner, cosigned by the set. See [Luckynodes](Luckynodes).

## Leagues & streaks

- **Streaks:** mine consecutive days; day-counts survive quiet spells.
- **Leagues:** phone / cpu / gpu / farm boards on 30-day effective
  work. Self-declared family, policed by trailing rate; rewards
  whale-blunted like jackpots.
- Live boards: pool dashboard + explorer `/league`.

Track everything on the [explorer game page](https://luckylode.cc/explorer/game):
jackpot/pot reserves, drought, draws with txids, hits and splashes.
