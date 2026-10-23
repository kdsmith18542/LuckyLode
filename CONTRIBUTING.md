# Contributing

Thanks for helping build Lucky Lode.

## Ground rules

- Code in this repository is GPL-3.0-only; by submitting a pull
  request you agree your contribution is licensed the same way.
- The design record in `docs/` is the project's source of truth. If
  your change implements or alters something a document describes,
  update the document in the same pull request; if your change
  contradicts a recorded decision, say so explicitly in the
  description and be ready to discuss it.
- Consensus-affecting changes (anything in `lm-pow`, `lm-rewards`, or
  the node repository) additionally require the KAT gates to stay
  green and a clear statement of what moves on chain. Renames never
  change construction constants or test vectors (docs/13).

## Development setup

### Rust workspace (miner, pool, explorer, tools)

Rust stable, plus the OpenCL headers (`ocl-icd-opencl-dev` on Debian/
Ubuntu) because `lm-miner` links `libOpenCL` by default. CPU-only
embedders build it with `default-features = false`.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release            # lm-miner, pool_server, explorer, faucet, ...
```

### Desktop wallet

The wallet is a **separate Cargo workspace** (`lm-wallet/src-tauri`) and
is not covered by `cargo test --workspace`.

```sh
cd lm-wallet/ui && npm install && npm run build
cd ../src-tauri && cargo test && cargo tauri build --no-bundle
```

Requirements: Node 20+, Rust, and the Tauri Linux dependencies
(webkit2gtk, gtk3). **Never ship a binary from plain `cargo build`** —
it loads the Vite dev URL instead of the embedded UI (see
`lm-wallet/README.md`). Use `cargo tauri dev` for development.

### Node (consensus fork)

The node lives in a separate repository
([LuckyLode-node](https://github.com/kdsmith18542/LuckyLode-node),
branch `luckyhash`) because it carries the upstream Verus history. Clone
it next to this repo (the local convention is `verus/`, which is
gitignored here). Build prerequisites and `autogen.sh`/`configure`
instructions are in that repository's README; the consensus gates run
without a full build:

```sh
cd LuckyLode-node && ./src/lucky_kat.sh
```

`SODIUM_INC`/`BOOST_INC`-style environment variables are only needed on
machines with non-standard include paths — never bake them into
committed files.

A quick local chain for testing the Rust side:

```sh
luckyd -regtest -ac_algo=luckyhash -datadir=/tmp/ll-regtest \
  -rpcuser=u -rpcpassword=p -rpcport=18888 -port=18889 -daemon=0
./target/release/pool_server --node-rpc 127.0.0.1:18888 \
  --rpc-user u --rpc-pass p --bind 127.0.0.1:3344
./target/release/lm-miner pool --pool 127.0.0.1:3344 --worker <address>
```

### Android miner

Toolchain: JDK 21, Android SDK platform 34 + build-tools, NDK r27+,
Gradle 8. See `lm-mobile/android/README.md` for the exact build recipe
(cross-compiling the Rust JNI library for both ABIs, signing, and the
emulator workflow).

## Before you open a pull request

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
(cd lm-wallet/src-tauri && cargo test && cargo clippy --all-targets -- -D warnings)
```

All must pass. Keep commits focused with imperative subjects
("pool: fix X", "docs: clarify Y"); don't include credentials,
machine-specific paths, or private notes in code, comments, or commit
messages — everything here is public.

## Areas that need help

Reference-quality ports of the miner to more platforms, third-party
pool integrations against `lm-stratum` (V1 and V2 are both
interoperable with their reference stacks), hardware-wallet support in
`lm-keys`, and documentation improvements. If you're unsure whether a
change is wanted, open an issue first.
