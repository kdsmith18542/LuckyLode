# Lucky Lode Wallet (desktop)

Tauri v2 desktop wallet: self-custody keys (BIP39/BIP32, encrypted
vault), send/receive, pool and solo mining, on-chain **luckynodes**
(identity registration + heartbeats), rewards/game stats, and a managed
node. Design notes are in the project's internal docs (not in this
repository); the user-facing wiki lives in `wiki/`.

## Layout

- `ui/` — Svelte 5 + Vite + TypeScript frontend (design tokens match the
  explorer/pool/website: dark, gold, green).
- `src-tauri/` — Rust backend (standalone cargo project):
  - `node.rs` — node JSON-RPC (reuses `lm-stratum::rpc::RpcClient`)
  - `pool.rs` — pool HTTP API (std-only)
  - `draws.rs` — pre-committed draw math (mirrors `lucky_draw.h`; KAT
    vectors pinned in tests)
  - `luckynode.rs` — embedded identity registration + heartbeat service
  - `commands.rs` — the Tauri command surface

## Develop

```sh
cd lm-wallet/ui && npm install
cd ../src-tauri && cargo tauri dev
```

## Build

```sh
cd lm-wallet/ui && npm run build
cd ../src-tauri && cargo tauri build --no-bundle
```

The raw binary lands at `src-tauri/target/release/lucky-lode-wallet`;
install it to `~/Applications/LuckyLodeWallet/` (desktop entry points
there) and Xephyr-verify it renders before handing it over.

**Never ship a wallet binary built with plain `cargo build`.** The raw
`cargo` invocation resolves the `tauri` crate *without* the
`custom-protocol` feature, so the binary loads the Vite dev URL
(`http://localhost:5173`) instead of the embedded frontend and opens
as a blank white "Could not connect to localhost: Connection refused"
window (proven via strace: the broken binary dials 127.0.0.1:5173 and
nothing else; the CLI-built one only touches the node RPC). Only
`cargo tauri build` produces a working standalone binary.

Bundles (`deb` + AppImage) are enabled (`tauri.conf.json` →
`bundle.active`):

```sh
cd lm-wallet/src-tauri && cargo tauri build --bundles deb,appimage
```

AppImage note: linuxdeploy scans `$PATH` and aborts on unreadable
entries. A dangling `/usr/sbin/poetry` → `/root/.local/bin/poetry`
symlink breaks the build on this box — trim the `sbin` dirs:

```sh
cd lm-wallet/src-tauri
PATH=$(echo "$PATH" | tr ':' '\n' | grep -vE '/sbin$' | paste -sd:) \
  cargo tauri build --bundles appimage
```

Verified click-to-run: the built AppImage launches standalone (bundles
webkit/gtk; needs `libfuse2`).

## Defaults

- Node connection is preset-based (Settings → Node): **Shared testnet
  node** (`https://luckylode.cc/node/`, public `u`/`p` testnet creds,
  embedded by the app — nobody types them) or **Node on this computer**
  (the wallet manages the node daemon: auto-detects luckyd on PATH and the
  usual per-OS install locations, resolves the daemon's own default
  datadir per platform, and generates its own RPC credentials to pass
  to the node — the user never sees them; RPC is pinned to loopback).
  A fresh install points at `127.0.0.1:42253` (local-first, like Core;
  the flagless testnet RPC default, verified on a clean machine)
  and the top bar suggests the shared node until one of them is running.
  Advanced exposes the raw RPC URL/user/password and node location for
  custom setups — `http://`, `https://`, and URL path prefixes are all
  supported.
- Pool API: `https://luckylode.cc/pool`.
- Luckynode identities are on-chain `ln-` names (doc 06 §6.7) — no local
  key files; registration and heartbeats are node transactions driven
  from the Luckynodes tab or the standalone `luckynode` daemon.

## Status (M1+)

Built: shell + status bar, next-block draw banner (pre-committed draws),
overview balances/activity (**per-tx detail**), send/receive (**contacts
address book + receive QR**), **embedded miner** (Miner tab:
CPU/OpenCL — GPU device picker, batch (MiB VRAM), NVIDIA power limit,
duty + temperature guard, live hashrate/shares/temp/power/log — same
engine as the reference `lm-miner`; live-verified at ~21 kH/s on a GTX
1080 with accepted shares), solo mining against the wallet's own node,
luckynode registration + heartbeat service, Rewards (emission, next
block draw, jackpot/pot/prize fund), Settings with dark/light themes,
OS notifications, FAQ, and OS-keyring credential storage. Packaged: deb
+ AppImage (click-to-run verified).
