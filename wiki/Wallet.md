# Wallet

Build/install: see `lm-wallet/README.md` in
https://github.com/kdsmith18542/LuckyLode. No release build is
published yet.

## First run

Write down the 12-word seed before anything else. It is the only backup.
The wallet encrypts the seed with your spending password; losing the
password is recoverable **only** from that phrase — the lock screen has
a "Forgot password?" recovery form.

## Tabs

- **Overview** — balances, activity, network, emission, staking setup.
- **Send / Receive** — transfers with a review step, QR payment requests.
- **Miner** — CPU or GPU pool mining (or solo against your own node)
  with temperature/power caps. Paste the pool (`luckylode.cc:3346`)
  and your payout address, Start.
- **Luckynodes** — register/heartbeat an `ln-` identity
  (see [Luckynodes](Luckynodes)).
- **Rewards** — jackpot reserve, pot, prize fund, the next block's draw.
- **Settings** — node (managed or shared testnet), wallet, contacts,
  notifications, FAQ.

## Staking

Coins in the wallet app do NOT stake by themselves — PoS mints from a
*node* wallet. Send coins to your node's address, then Start staking
(see [Staking](Staking)). The Overview shows your node's staking state.
