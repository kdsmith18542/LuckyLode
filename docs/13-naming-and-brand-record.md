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
  docs/02/04/09 swept). The name-the-algo contest is concluded.
  **Internal identifiers renamed 2026-10-18** (2026-10-18): the C++
  layer is `lucky_hash.{h,cpp}` / `lucky_difficulty.{h,cpp}` /
  `lucky_kat.cpp` / `lucky_*_kat.cpp` / `lucky_kat.sh`,
  `CLuckyHash`/`GetLuckyHash`, `ASSETCHAINS_LUCKYHASH`; the Rust mirror
  names its salt builder `lucky_salt`. Renames are identifier-only — the
  KAT vectors and construction constants never move with a rename (all
  five gates green before and after). `-ac_algo=emberhash` stays
  accepted as a legacy alias for `-ac_algo=luckyhash`. The fire-themed
  luck class "ember" (docs/12 §12.10 ladder: mythic…ember, flare,
  glint, spark) is a THEME name, not the working title — untouched.

## 13.3 Frozen strings (exempt from branding)

- `LMv1`, `lowmine-pow/v1` — LuckyHash construction identifiers; the KAT
  vectors depend on them byte-for-byte (doc 05 Addendum A naming note).
- `verusDataSignaturePrefix` (`"Verus signed data:\n"`) — signed-data
  format string; changing it breaks signature compatibility and would
  need a format-version decision of its own.
- Address version bytes and network magic — already Lucky Lode's own,
  deliberately distinct from Verus (cross-chain safety).
- **Official wire ports (recorded 2026-10-17; frozen like the magic):**
  mainnet P2P **9270** / RPC **9271**, testnet P2P **19270** / RPC
  **42253** (the flagless `-testnet` default, verified empirically on a
  clean datadir — the chainparams ASSETCHAINS_RPCPORT value is not what
  the boot path binds). These are the defaults a clean install binds
  and the only values software may assume. A chain conf
  `rpcport=`/`port=` line overrides them *for that install only* —
  overrides must never leak into shipped defaults (a wallet once
  hard-coded a dev box's conf override as its default and would have
  missed every fresh user's node).

## 13.4 Doc numbering

- docs/08 and docs/10 were reserved and never written; the numbering is
  historical. This doc (13) was added 2026-10-14.

## 13.6 Casino game names (decided 2026-10-14; **withdrawn 2026-10-17**)

- The first-party casino (doc 14, deleted) is **withdrawn** — names below
  are kept only as a trademark record.
- Launch titles had been: **Lucky Lode** (Book-style, 5×3) and **Lucky
  Lode Ways** (ways-style, up to 117,649 ways).
- **"Megaways" is never used** — it is a Big Time Gaming trademark.
  "Book of…" is likewise avoided. This rule survives the withdrawal: if a
  game layer ever ships a ways-style title, it is "Lucky Lode Ways".

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
- **PBaaS / Verus Vault feature names — RESOLVED 2026-10-18 (owner
  direction):** the user-facing names are **LuckyVault** (vault
  feature — no user-facing strings existed in the node; the only
  occurrences are internal activation constants, `ACTIVATE_VERUSVAULT`
  / `SOLUTION_VERUSV6`, which are format, not branding, same policy as
  `VERSION_VERUSID`) and **LuckyBridge** (the chain-launch / cross-chain
  layer, upstream "PBaaS" — decided 2026-10-18 over LuckyLink to avoid
  Chainlink/LINK adjacency). 68 user-facing strings swept (help,
  validation errors, RPC text). Deliberately kept: internal identifiers
  (`CPBaaS*`, `pbaas.cpp`, `ASSETCHAINS_PBAAS*`), wire keys
  (`pbaassystemregistrationfee`), and the upstream datadir paths for
  genuine PBaaS chains — same format-not-branding policy. The data-dir
  subpaths are done for Lucky Lode chains (2026-10-17).
- Name-the-algo contest — **concluded 2026-10-14: LuckyHash** (§13.2).
