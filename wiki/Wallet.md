# Wallet

Download: https://github.com/kdsmith18542/LuckyLode/releases (AppImage).

## Install (Linux)

Run the AppImage. If it won't execute, check it is marked executable;
very old systems without FUSE need the raw binary instead (ask in the
 usual places — see [FAQ](FAQ)).

## First run

Write down the 12-word seed before anything else. It is the only backup.
Settings → Wallet can change the password later; there is no recovery
flow — lost seed means lost coins, even play ones.

## Tabs

- **Overview** — balances, network, emission, your referrals to pool blocks.
- **Mining** — CPU or GPU pool mining with temperature/power caps.
  Paste the pool (`luckylode.cc:3346`) and your payout address, Start.
- **Send/Receive** — normal transfers, QR payment requests.
- **Luckynodes** — register/heartbeat if you run one (see [Luckynodes](Luckynodes)).
- **Node** — run your own node (Settings → Node → Start), or use the
  shared testnet node with one click.

## Staking

Coins in the wallet app do NOT stake by themselves — PoS mints from a
*node* wallet. Send coins to your node's address, then Start staking
(see [Staking](Staking)). The Overview shows your node's staking state.
