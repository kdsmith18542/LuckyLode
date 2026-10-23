# FAQ

**Are testnet coins worth anything?**
No. Worthless by design, chain may reset (see [Testnet Rules](Testnet-Rules)).

**How long do payouts take to spend?**
Pool payouts are normal transactions — spendable once confirmed. Freshly
minted coinbases wait 100 blocks, and outputs younger than 150 blocks
sit out staking.

**Why is my balance lower than my total received?**
Received counts every inflow including recycled stake principal; the
difference is your own sends. Nothing locks.

**Why did stake supply drop?**
It is a difficulty-implied estimate over the trailing 100 blocks, not a
balance. Old PoS blocks aging out of the window move it ~10% a step.

**Why are there bursts of PoS blocks?**
On the testnet almost all eligible stake sits in one wallet. When it
wins it can mint several blocks in a row (consensus allows up to 9).
More stakers would smooth the mix.

**Where is the pool staking?**
Nowhere — the pool never stakes. PoS comes from node wallets
(see [Staking](Staking)).

**Why isn't my worker paid?**
Worker names double as payout addresses. A bare name (no address)
accrues unpayable balance — rename the worker to your address.

**How do I get paid from the Pool?**
Check **My miner** on the pool dashboard: on open pools, look up your
worker name; on token pools, paste your worker token. Payouts run on an
interval once coins mature and the balance clears the threshold.

**Android app on the Play Store?**
No, and never — miners are banned. Sideload the APK.

**Something broke / coins vanished on a reset?**
Testnet resets wipe chain state by procedure with advance notice;
back up your wallet seed regardless. Real support: the usual places
(see [Home](Home) links).
