# Staking

Lucky Lode targets ~50% PoW / ~50% PoS blocks. The difficulty steers
itself (long PoW runs loosen the PoS target automatically) — what it
needs from you is eligible coins *online*.

On the current testnet almost all stake sits in one wallet, so PoS
blocks arrive in bursts rather than a smooth half-and-half. That is a
distribution artifact, not a rule change.

## How to stake

1. Run a node (wallet Settings → Node → Start, or your own `luckyd`).
2. Send coins to an address in the **node's** wallet (the wallet app
   gives you a fresh one — app-held coins never stake by themselves).
3. Unlock the node wallet and enable staking (`setgenerate true 0` or
   the wallet toggle). It mints automatically whenever one of your
   outputs wins a block.

## Rules that bite

- Outputs must be **150 blocks old** to be eligible
  (`eligible_staking_balance`).
- Coinbase rewards **mature after 100 blocks** before spending.
- Staking needs the wallet **unlocked** and the node **online**.
- A staked output is consumed and recycled by the coinstake — it must
  re-age 150 blocks before it can win again. This is normal, not a lock.
- PoS blocks pay 95% of the schedule to the staker; jackpots/pot are
  PoW-finder-only (stakers get the [Luckynode Draw](Luckynodes) instead).
  The other 5% is the game-fund levy.

Watch the chain's PoS share and stake supply on the
[explorer](https://luckylode.cc/explorer/).
