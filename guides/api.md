# Pool and explorer API reference

Both services are read-mostly HTTP servers. Paths are relative to the
service root, so they also work behind a reverse-proxy prefix
(`/pool/`, `/explorer/`). Amounts are **base units** (1 coin =
100,000,000) unless a `*_coins` field says otherwise; the ticker is
`LKYL` mainnet, `tLKYL` testnet, `rLKYL` regtest.

## Pool (`pool_server`)

### Public

| Route | Returns |
|---|---|
| `GET /api/stats` | pool counters (`pool.*`), `workers[]`, `last_blocks[]`, `payouts[]`, `payout_totals`, `jackpot` state, `prize.credits`, `divisions.boards`, and the current-epoch `draw` advisory (`epoch/budget/remaining/winner`; epoch 0 = none) |
| `GET /api/divisions` | seasonal Divisions boards per device family |
| `GET /api/worker/<miner>` | one worker row (live or idle): rates, shares/rejects, streak, division rank, owed/paid. Public on open pools; on token pools owes/paid require the matching token (`Authorization: Bearer <token>`) |
| `GET /` | embedded dashboard (HTML) |

### Worker-authenticated (token pools)

Tokens come from the operator's `--workers-file` (`worker:token` per
line). The token is always a header or POST body, never a URL.

| Route | Action |
|---|---|
| `GET /api/miner` + `Authorization: Bearer <token>` | your row + payout settings + pool defaults |
| `POST /api/payout/settings` `{token, address?, threshold?}` | set/clear your payout address and threshold. `threshold` is a decimal coin string; empty clears the override |
| `POST /api/attest` `{worker, provider:"self", detail?, token?}` | optional device label for Division boards (self-declared, not a security check) |

Status codes: `200` ok, `400` bad request, `403` missing/unknown token,
`404` unknown worker, `429` faucet-style rate limits.

## Explorer (`explorer`)

| Route | Returns |
|---|---|
| `GET /api/tip`, `/api/stats` | tip height and chain stats |
| `GET /api/blocks?page=N` | block index, 50 per page, newest first |
| `GET /api/block/<height\|hash>` | block summary |
| `GET /api/tx/<txid>` | transaction with inputs/outputs, fee, size |
| `GET /api/address/<addr>?page=N` | balance, received, txcount, first-seen, 100 history rows |
| `GET /api/game` | jackpot/pot/prize state + per-block game rows |
| `GET /api/divisions` | last-synced Divisions snapshot |
| `GET /api/mempool` | node mempool view (bounded entries) |
| `GET /api/richlist` | top unspent balances |
| `GET /api/charts?days=N` | daily series (hashrate, txs, fees, supply, addresses) |
| `GET /api/market` | supply/volume/largest balances; price only when `--price-url` is configured |

Unknown routes and unknown blocks/txs/addresses return a JSON
`{"error":"not found"}` with status 404. Pages show a banner when the
node is unreachable; game/divisions data is mirrored from the pool and
the Rewards page states when that snapshot is stale.

HTML routes mirror the JSON: `/`, `/blocks`, `/block/<key>`, `/tx/<txid>`,
`/address/<addr>`, `/rewards`, `/charts`, `/supply`, `/mempool`,
`/divisions`, `/richlist`, `/search?q=`.

## Units and money

- Pool money fields are base units; `threshold_coins` is a formatted
  decimal string.
- Explorer supply is the **unspent-output total** (fees are burned on
  this chain) and matches the node's `coinsupply` to the satoshi.
- Fee rates are sat/vB from the node's raw transaction bytes; unknown
  sizes render as "–", never a guess.
