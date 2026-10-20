# FAQ

**Are testnet coins worth anything?**
No. Worthless by design, chain may reset (see [Testnet Rules](Testnet-Rules)).

**How long do payouts take to spend?**
Pool payouts are normal transactions — spendable on arrival. Only
freshly minted coinbases wait 100 blocks, and only outputs younger
than 150 blocks sit out staking.

**Why is my balance lower than my total received?**
Received counts every inflow including recycled stake principal; the
difference is your own sends. Nothing locks.

**Why did stake supply drop?**
It's a difficulty-implied estimate over the trailing 100 blocks, not a
balance. Old PoS blocks aging out of the window move it ~10% a step.

**Where is the pool staking?**
Nowhere — the pool never stakes. PoS comes from node wallets
(see [Staking](Staking)).

**Why isn't my worker paid?**
Worker names double as payout addresses. A bare name (no address)
accrues unpayable balance — rename the worker to your address.

**Android app on the Play Store?**
No, and never — miners are banned. Sideload the APK from releases.

**Something broke / coins vanished on a reset?**
Testnet resets wipe chain state by procedure with advance notice;
back up your wallet seed regardless. Real support: the usual places
(see [Home](Home) links).
