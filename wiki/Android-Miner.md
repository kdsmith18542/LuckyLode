# Android Miner

APK: built from `lm-mobile/android` in
https://github.com/kdsmith18542/LuckyLode (see its README for the build
and signing notes). Sideload only — there is no Play Store version and
there never will be: miners are banned from Play.

## Install

Install the APK, allow install-from-unknown-sources once. Grant
notifications on first start or the mining service runs invisible.

## Use

Enter pool (`luckylode.cc:3346`, or `:3347` with V2 enabled) + worker
(your payout address), tap Start. The scheduler mines only when it
should (cool, charged or charging, overnight-leaning) with your advanced
thresholds respected. Dashboard tiles: local hashrate/shares, battery,
and your pool-side stats, streak and division rank.

## Honesty box

A phone burns ~3–5 W mining — about 3 hours from a full 15 Wh pack.
Default is charge-only. This is a testnet toy and a mainnet sidekick,
not a rig. Keep it cool, keep it charged, expect pennies.
