# `lm-sync` — RPC-driven fast sync / stall healing

Small operator tool for small networks where P2P can stall (one banned
peer freezes headers and blocks with no self-recovery).

- `pull` — copy blocks from a source node to a target over JSON-RPC
  (`getblock` hex → `submitblock`, full validation on the target). Refuses
  to touch equal-height-but-diverged chains.
- `watch` — supervise a target; on a stall while the source is ahead,
  pull the gap and heal P2P (`clearbanned` + `addnode onetry`).
- `status` — one-line tip/headers/peers snapshot for monitors.

```sh
lm-sync pull --from HOST:PORT --from-user U --from-pass P \
             --to HOST:PORT --to-user U --to-pass P
```

Plain HTTP only: reach a remote node through an SSH tunnel
(`ssh -L 11927:127.0.0.1:9271 user@host`). Unknown flags and non-numeric
values are usage errors, not silent defaults.
