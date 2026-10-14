# 13 — Naming & Brand Record

**Status:** living record — decisions logged with dates. Nothing here is
consensus; the frozen strings below are called out precisely because they
*are* consensus.

## 13.1 Project identity

- **Lucky Lode** (decided 2026-10-11; working title "Lowmine" retired).
  Domain `luckylode.cc`; ticker **LKYL** (testnet **tLKYL**, regtest
  **rLKYL**).
- **Branding is total from genesis** (doc 02 §2.1): own name, ticker,
  domain, logo, address version bytes, and network magic. Forking code is
  not co-branding. Upstream copyright/license attribution stays in code
  and the `NOTICE` files (legal requirement, invisible to users).
- Rebrand pass executed 2026-10-13: testnet prefixes 111/196, network
  magic `LucT`, chain identity `LUCKYLODETEST` (datadir `~/.luckylodetest`),
  currency units, startup/RPC/datadir string purge, `NOTICE` files.

## 13.2 Feature names

- **LuckyID** (decided 2026-10-14): user-facing name for the identity /
  badges layer inherited with the fork (previously "VerusID"). Docs and
  node help/error strings use LuckyID; the internal serialization
  constants (`VERSION_VERUSID`, `TYPE_VERUSID_DEFAULT` — numeric wire
  values) keep their identifiers — they are format, not branding.
- **LuckyHash** (decided 2026-10-14): final name for the proof-of-work
  algorithm (previously the EmberHash working name; doc 05 title updated,
  docs/02/04/09 swept). The name-the-algo contest is concluded. Internal
  identifiers keep their names: `ember_*.cpp` files, `CEmberHash` /
  `GetEmberHash`, `ASSETCHAINS_EMBERHASH`, and `-ac_algo=emberhash` stays
  accepted as a legacy alias for `-ac_algo=luckyhash`. Parameters and KAT
  vectors never move with a rename.

## 13.3 Frozen strings (exempt from branding)

- `LMv1`, `lowmine-pow/v1` — LuckyHash construction identifiers; the KAT
  vectors depend on them byte-for-byte (doc 05 Addendum A naming note).
- `verusDataSignaturePrefix` (`"Verus signed data:\n"`) — signed-data
  format string; changing it breaks signature compatibility and would
  need a format-version decision of its own.
- Address version bytes and network magic — already Lucky Lode's own,
  deliberately distinct from Verus (cross-chain safety).

## 13.4 Doc numbering

- docs/08 and docs/10 were reserved and never written; the numbering is
  historical. This doc (13) was added 2026-10-14.

## 13.6 Casino game names (decided 2026-10-14)

- First-party casino at `luckylode.cc/casino` (doc 14).
- Launch titles: **Lucky Lode** (Book-style, 5×3) and **Lucky Lode Ways**
  (ways-style, up to 117,649 ways).
- **"Megaways" is not used** — it is a Big Time Gaming trademark; the ways
  title is named "Lucky Lode Ways". "Book of…" is likewise avoided.

## 13.5 Open naming items

- Residual "Verus" strings: **audited and swept 2026-10-14** — user-facing
  strings now say Lucky Lode (daemon/CLI banners, bind errors, wallet
  dumps, "Invalid Lucky Lode address", deprecation/sendalert notices,
  package name, bootstrap URLs). Deliberately kept: upstream copyright
  attribution (`COPYRIGHT_STR` + the license block), external product
  names (Verus Desktop / Agama / Verus Mobile import path), cross-chain
  references (Verus-or-Ethereum address fallbacks, parent-chain and
  merge-mining wording, `PROOF_PBAASMMR` Verus MMR proof), the frozen
  `verusDataSignaturePrefix`, and one legacy wallet-dump test fixture.
- **PBaaS / Verus Vault feature names** — still upstream names; rename
  pending (same exercise as VerusID → LuckyID). The PBaaS Windows
  data-dir subpaths (`Verus` / `VerusTest` / `Komodo`) ride on that
  decision.
- Name-the-algo contest — **concluded 2026-10-14: LuckyHash** (§13.2).
