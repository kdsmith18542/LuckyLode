# Pool Mining

Official pool: https://luckylode.cc/pool/ — open, no signup, no password
on testnet. Anyone can run an independent pool from the open-source
reference implementation (the project commits to pool neutrality: the
chain pays the same whoever builds the block).

## Connect

- Stratum V1: `luckylode.cc:3346` (**plaintext** — the reference
  pool does not serve TLS on V1)
- Stratum V2 (Noise-encrypted): `luckylode.cc:3347`
- CLI: `lm-miner pool --pool luckylode.cc:3346 --worker YOUR_ADDRESS`
  (`--device auto|cpu|opencl`; `--v2` for the V2 wire)
- Your **worker name doubles as your payout address**. Use a real
  address or your balance sits unpayable (visible on the dashboard).
- **Reference miner only (for now):** the V1 wire is custom (no
  `mining.set_difficulty`, empty extranonce, custom notify params), so
  today only `lm-miner` can mine here — third-party V1 miners expect
  extranonce + `set_difficulty` and will not work. A compat layer is
  queued; until then this page documents the reference path.

## How pay works

- Every accepted share earns PPLNS weight over the last 2,000 shares.
  Each block's value — normal reward plus any jackpot/pot bonus — is
  split **linear pro-rata** across that window.
- Jackpot bonuses and pot splashes redistribute within the window using
  square-root weights with a 25% per-miner cap, so one large rig cannot
  take a whole windfall.
- Payouts are sent from the pool wallet once the coins mature (100
  confirmations) and your balance clears the pool's payout threshold.
  On the official testnet pool the default threshold is small and the
  loop runs every minute.
- Prize-fund overflow from the jackpot levy accrues to the **Prize
  Fund**, which pays the Luckynode Draw — not to miners directly. See
  [Game Layer](Game-Layer).

## Fees

Reference miner: 1% dev fee, opt-out with `--dev-fee-pct 0` (it nags).
Reference pool: 0.5pp slice of the operator cut to published dev
funding. Treasury off.

## Streaks & Divisions

Mine consecutive days to build a streak (shown per worker; day counts
survive quiet days in the ledger). **Divisions** race by device family
— phone/cpu/gpu/farm — on 30-day effective work. Declared family,
policed by trailing rate.
