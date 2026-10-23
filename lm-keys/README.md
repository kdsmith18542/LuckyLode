# `lm-keys` — self-custody wallet core

Keys, addresses and transaction signing for the desktop/mobile wallets.
No networking: callers pass UTXOs and broadcast the signed bytes.

- **Seed**: BIP39 12-word mnemonic (official vectors pinned).
- **HD**: BIP32/BIP44 (`m/44'/coin'/0'/change/index`) with
  node-compatible xprv/tprv serialization.
- **Addresses**: P2PKH/P2PK, WIF, chain-correct version bytes for
  mainnet/testnet/regtest.
- **Transactions**: legacy v1 transparent spends plus the Overwinter v3
  and Sapling v4 sighash paths (ZIP-143/243) the fork activates later.
  Low-S enforcement, dust/change/fee handling, and byte-exact sighash
  vectors from the node's own test set.
- **Vault**: Argon2id + ChaCha20-Poly1305 seed encryption (the wallet
  backend owns the files; this crate owns the crypto).

```sh
cargo test -p lm-keys
```

Known limits: transparent only (no shielded spends), no PSBT, fixed fee
policy in the caller. `-addressindex` nodes are required for the
wallet's balance scanner.
