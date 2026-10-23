# `lm-rewards` — consensus game-money rules (Rust mirror)

Pure functions mirroring the node's game-money consensus, with frozen
KAT vectors. No I/O, no floats on money paths; integer math only.

- `reserve` — jackpot reserve: 5% levy, 4/1 split, cap overflow to the
  Prize Fund, jackpot payout `min(5×R, reserve)`.
- `pot` — Lucky Pot: 10/25/50% tiers from the pre-committed draw,
  cap overflow burns, pity-timer floor.
- `prize` — Prize Fund: per-block inflow, trailing-inflow budget,
  winner selection over weighted entrants (mirrors `lucky_prize.h`).
- `seed` — faucet/dev reserves and the capped per-block claims.
- `luck` — the ten themed luck classes (display layer, not consensus
  payouts).
- `payout` split helpers — sqrt weights with a 25% cap and dust rules,
  shared with the pool.

```sh
cargo test -p lm-rewards
```

The node is authoritative; these functions exist so the pool, explorer
and wallet can predict and display exactly what the node will validate.
