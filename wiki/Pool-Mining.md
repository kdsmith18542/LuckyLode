# Pool Mining

Official pool: https://luckylode.cc/pool/ — open, no signup, no password
on testnet. Anyone can run an independent pool from the open-source
reference implementation.

## Connect

- Stratum V1: `luckylode.cc:3346` (TLS available)
- Stratum V2 (Noise-encrypted): `luckylode.cc:3347`
- CLI: `lm-miner pool --pool luckylode.cc:3346 --worker YOUR_ADDRESS`
  (`--device auto|cpu|opencl`, `--tls` for encrypted V1)
- Your **worker name doubles as your payout address**. Use a real
  address or your balance sits unpayable (visible on the dashboard).

## How pay works

- Every accepted share earns PPLNS weight (last 2,000 shares). Each
  block — reward, jackpot bonus, pot splash included — splits across
  contributors by that weight. Whale-blunted: square-root weights with
  a 25% cap at 5+ miners.
- Payouts are automatic, every block, to your worker address. No
  threshold games on the official pool.
- League overflow (reserve over cap) accrues to the league pool, not to
  miners directly — see [Game Layer](Game-Layer).

## Fees

Reference miner: 1% dev fee, opt-out with `--dev-fee-pct 0` (it nags).
Reference pool: 0.5pp slice to published dev funding. Treasury off.

## Streaks & leagues

Mine consecutive days to build a streak (shown per worker, survives
quiet days in the ledger). Leagues race by device family —
phone/cpu/gpu/farm — on 30-day effective work. Declared family,
policed by rate.
