# Luckynodes

Luckynodes are the stakers' route into the game layer. A Luckynode is
an **on-chain identity in the `ln-` namespace** (a LuckyID name) whose
primary address receives draw winnings. There is no bond, no escrow and
no pool registry — eligibility is consensus state.

## Run one

1. Register an `ln-` identity on chain with the payout address you want
   (the wallet's **Luckynodes** tab does this; or `luckynode
   --register-only`). Registration costs the chain's identity fee
   (100 tLKYL on the current testnet).
2. Keep it live: the identity must be updated (a heartbeat transaction)
   within the liveness window — once per epoch is plenty. The wallet's
   Luckynodes tab or the standalone `luckynode` daemon submits
   heartbeats automatically while running.
3. Watch yourself on the wallet Luckynodes tab or the explorer rewards
   page.

## The Draw

Each epoch the chain picks a winner from live `ln-` identities. The
epoch boundary block's coinbase pays the winner straight from the
**Prize Fund** — any block builder (pool, solo miner, staker) can attach
the claim, and it always pays the winner, never the builder.

- Epoch length: **3 blocks on testnet** (2,880 on mainnet).
- Budget: 10% of the Prize Fund's trailing-epoch inflow plus a small
  fixed seed release, capped at the fund balance.
- The ticket is fixed before the epoch starts (a hash of the previous
  epoch's start block), so nothing can be withheld or ground.

The old pool-run registry, bonds and cosigns are retired. Pools are
pure hashrate aggregation now.
