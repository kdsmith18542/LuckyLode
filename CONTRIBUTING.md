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

## Before you open a pull request

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Both must pass. Keep commits focused with imperative subjects
("pool: fix X", "docs: clarify Y"); don't include credentials,
machine-specific paths, or private notes in code, comments, or commit
messages — everything here is public.

## Areas that need help

Reference-quality ports of the miner to more platforms, third-party
pool integrations against `lm-stratum` (V1 and V2 are both
interoperable with their reference stacks), hardware-wallet support in
`lm-keys`, and documentation improvements. If you're unsure whether a
change is wanted, open an issue first.
