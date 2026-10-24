# Getting Started (testnet, five minutes)

1. **Build the wallet** from
   [the repository](https://github.com/kdsmith18542/LuckyLode)
   (`lm-wallet/README.md` has the Linux build/install steps), or make an
   address with your own node: `luckyd -testnet getnewaddress`.
2. **Copy a receive address** from the wallet (Receive tab).
3. **Get play money** from the [Faucet](Faucet) (10 tLKYL per address
   per day), or skip straight to mining — mining needs no starting
   balance.
4. **Mine:** wallet **Miner** tab → GPU/CPU → Start. Or CLI:
   `lm-miner pool --pool luckylode.cc:3346 --worker YOUR_ADDRESS`.
   Mining needs the reference miner for now: the V1 wire is custom
   (no `set_difficulty`/extranonce), so third-party V1 miners don't
   connect — see [Pool Mining](Pool-Mining).
5. **Watch:** your worker appears on https://luckylode.cc/pool/ within a
   minute; payouts are sent automatically once coins mature and your
   balance reaches the payout threshold.

No account, no password, no pool signup. Your address *is* your
identity: payouts, streaks and Divisions all key off it. Guard the
wallet seed like real money — testnet resets can wipe chain balances,
but your keys are yours (see [Testnet Rules](Testnet-Rules)).
