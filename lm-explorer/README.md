# `lm-explorer` — reference block explorer

Tip-following SQLite index + read-only HTTP UI/API. No `-txindex`
needed: the index walks the chain from genesis and follows the tip,
rolling back to the fork point on reorgs.

## Run

```sh
cargo run --release -p lm-explorer -- \
  --node-rpc 127.0.0.1:19271 --rpc-user U --rpc-pass P \
  --bind 127.0.0.1:8888 --db explorer.db \
  --pool-api http://127.0.0.1:8081        # optional: game/divisions data
```

`--resync` rebuilds the database; `--price-url` adds an optional HTTPS
price feed for the supply page (otherwise it says no listing).

## Pages

`/` (home) · `/blocks` · `/block/<height|hash>` · `/tx/<txid>` ·
`/address/<addr>?page=N` · `/rewards` · `/charts` · `/supply` ·
`/mempool` · `/divisions` · `/richlist` · `/search?q=`

JSON: `/api/tip`, `/api/stats`, `/api/blocks`, `/api/block/<key>`,
`/api/tx/<txid>`, `/api/address/<addr>`, `/api/game`, `/api/divisions`,
`/api/mempool`, `/api/richlist`, `/api/charts`, `/api/market`.

## Notes

- Amounts are base units internally; the ticker follows the node's chain
  (`LKYL`/`tLKYL`/`rLKYL`) and is set once at startup.
- Supply is the **unspent-output total** (this chain burns fees), which
  matches the node's `coinsupply` to the satoshi.
- Game data and seasonal Divisions are mirrored from the pool's
  `/api/stats`; a pool outage leaves the last snapshot in place and the
  pages say how old it is. Node outages show a banner instead of zeros.
- Fee rates need exact tx sizes, resolved from the node's raw hex and
  cached (`tx_facts`); unknown sizes render "–", never a guess.
- Point it at a loopback RPC behind a reverse proxy when public.
