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
- **luckyd** (decided 2026-10-18): the node daemon binary builds as
  `luckyd` (was `verusd`). Build target, pid-file default (`luckyd.pid`),
  startup help, and the packaging/CI steps all follow; chain selection is
  unaffected (the chain defaults to LUCKYLODE regardless of the binary
  name). The wallet auto-detects `luckyd` first and still accepts a
  legacy `verusd` as a fallback so pre-rename installs keep working.
  The `verus` CLI and `verus-tx` utility binaries keep their upstream
  names.
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
- **Official pool stratum ports (recorded 2026-10-18):** V1 **3346**,
  V2 (Noise NX) **3347**. The V2 endpoint pins a stable authority key
  whose URL form the pool prints at startup and publishes on the
  dashboard; clients pin that key as the responder certificate.

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
- **"League" retired — 2026-10-21 (owner direction: "I don't like the
  league naming").** The old name covered two different things; both
  carry new names everywhere (code, docs, dashboards, APIs — a
  deliberate testnet API break, done once, before the consensus prize
  fund is named in code):
  - The device-class boards are **Divisions** (phone / cpu / gpu /
    farm divisions, seasons, ranks). `lm-pool` `league` module →
    `division`, JSON `league_class`/`league_rank`/`league_size` →
    `division_*`, pool route `/api/league` → `/api/divisions`, the
    stats object `league.boards` → `divisions.boards`, explorer
    `/league` page + `league_boards` table → divisions.
  - The staker-side game money is the **Prize Fund** (funds the
    Luckynode Draw; consensus-tracked since testnet reset #5, with the
    rules live in doc 06 §6.7 and mainnet activation at the ceremony). Money
    identifiers renamed: `to_league` → `to_prize` (node + Rust
    mirror), GBT `leagueoverflow` → `prizeoverflow`, pool
    `league_pool` → `prize_pool` (serde alias loads pre-rename state
    files), block JSON `league_overflow` → `prize_inflow`, explorer
    `game_blocks.league` column → `prize` (with an `ALTER TABLE`
    migration), `prize.credits` on `/api/stats`.
- **Rebrand finished — 2026-10-21 (owner direction: "finish the verus
  rebrand").** The CLI utilities are now **`lucky`** (was `verus`) and
  **`lucky-tx`** (was `verus-tx`); the missed daemon banner now prints
  "luckyd … server starting"; the no-CLI error and rewind hint point
  at `lucky`; thread names are `lucky-*`; regtest-only test knobs take
  `-lucky-min-stakeage` / `-lucky-coinbase-maturity` (the old
  `-verus-*` spellings still read, both hard-gated to regtest).
  Deliberately kept, per the format-not-branding policy: the frozen
  `verusDataSignaturePrefix`, `VERSION_VERUSID` /
  `ACTIVATE_VERUSVAULT` constants, upstream copyright + internal file
  names (`verus_hash.h`, `veruslaunch.*`), the `verushash` algorithm
  name (real algorithm foreign chains use), `.verus`/`.verustest`
  PBaaS datadir paths, the wallet's `verusd` fallback detection, and
  cross-chain import strings (Verus Desktop / Agama / Verus Mobile).
- **Deep identifier pass — 2026-10-21 (owner direction: rename
  everything renameable, provided it does not lie or misdescribe).**
  The fork's own consensus machinery now carries Lucky names:
  `CLuckySolutionVector`/`CConstLuckySolutionVector` (was CVerus*),
  `SOLUTION_Vn` (was SOLUTION_VERUSVn), `ACTIVATE_LUCKYVAULT` (was
  ACTIVATE_VERUSVAULT), `LUCKY_V1`/`LUCKY_V2` (block/nonce format
  versions, was VERUS_V1/V2), `LUCKY_MINTBLOCKS`, `LUCKY_MIN_STAKEAGE`,
  the PoS steering knobs `LUCKY_[PBAAS_]CONSECUTIVE_POS_THRESHOLD` /
  `LUCKY_[PBAAS_]NOPOS_THRESHOLD` (typo fixed), the PoS path
  `IsLuckyPOSBlock` / `GetLuckyPOSTarget` / `SetLuckyPOSTarget` /
  `GetRawLuckyPOSHash` / `_GetLuckyPOSHash` / `GetLuckyPOSHash` /
  `GetLuckyEntropyHashComponent`, the staker `LuckyStaker` /
  `LuckyStakeTransaction` / `LuckySelectStakeOutput` / `LuckyPoP`,
  `LUCKYD_VERSION`, and the miner's notary-currency fee labels
  (`notaryFees`). Deliberately still Verus-named because the names are
  TRUE: the VerusHash algorithm and its implementation
  (`CVerusHash*`, `verusclhash*`, `verus_hash.*`, `mine_verus_v2` —
  foreign chains mine it), external-chain machinery
  (`IsVerusActive` family, `VERUS_CHAINID`/`CHAINNAME`/`NOTARYID`/
  `DEFAULTID`/`DEFAULT_ZADDR`/`DEFAULT_ARBADDRESS`/`NODEID`/
  `ARBITRAGE_CURRENCIES`/`PRIVATECHANGE`, `veruslaunch.*` — the
  external ID-launch whitelist), VRSC-mainnet history
  (`VERUS_STAKE_EXPLOIT_*` heights, `VerusEthBridgeExceptionExportTxes`),
  the frozen `verusDataSignaturePrefix`, and the PBaaS datadir paths.
  Upstream copyright and license notices are retained throughout, per
  the MIT license — names were never the license's property.
