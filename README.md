# Lucky Lode — Plan Index

**Name:** Lucky Lode — decided 2026-10-11; primary domain `luckylode.cc`
**registered 2026-10-11** (RDAP-verified, expires 2027-10-11).
**Docs renamed to Lucky Lode 2026-10-11.** Remaining
legacy identifiers, kept deliberately: crate names `lm-*` and the wire
profile string `lowmine/1` (rename deferred to rebrand execution), and the
**frozen consensus strings** `LMv1` / `lowmine-pow/v1` — those never
change; they are construction identifiers, not branding (doc 05).

**One-liner:** a fair-launch, CPU/ARM-first mineable coin for the hardware
people already own — phones, miniPCs, laptops old and new — with mining
designed as a game you keep playing, not a chore you automate and forget.

**Status:** P1 underway — fork eval complete, EmberHash C++ port + KAT
gates green on the `verus/` fork branch; fork-decision record draft in
doc 08; naming decided (doc 13).

## Docs

1. [`01-thesis-and-positioning.md`](01-thesis-and-positioning.md) — why this
   niche is open, who it's for, what killed the predecessors.
2. [`02-technical-stack.md`](02-technical-stack.md) — fork-Verus analysis,
   algorithm strategy, modular modern stack (what to keep, what to rewrite,
   what language where).
3. [`03-fun-and-retention.md`](03-fun-and-retention.md) — the excitement
   layer: lottery blocks, device-class leagues, streaks, seasons, badges, and
   the anti-fade engine. This is the core differentiator — read it first
   after the thesis.
4. [`04-economics-launch-roadmap.md`](04-economics-launch-roadmap.md) —
   emission (incl. tail), dev funding without breaking fair-launch trust,
   launch sequence, phased roadmap, and honest risks (botnets, league
   spoofing, app-store policy, ASICs).
5. [`05-pow-algorithm.md`](05-pow-algorithm.md) — **EmberHash v1** (working
   name): the custom phone-ideal PoW, FROZEN rev 1.0 with real test vectors,
   difficulty rules, fairness posture, and mainnet gates.
6. [`06-jackpot-lottery.md`](06-jackpot-lottery.md) — jackpot blocks (~1%,
   PoW-hash-as-ticket, expectation-neutral, progressive pool splits),
   FROZEN rev 1.0 with testnet gates.
7. [`07-android-miner.md`](07-android-miner.md) — Android app + frozen
   thermal/power scheduler policy (hysteresis, derate, overnight mode),
   iOS companion scope, platform risks.
8. [`08-fork-decision.md`](08-fork-decision.md) — **DRAFT** P0 exit-gate
   record: fork-Verus recommendation with Oct-2026 evidence (PBaaS
   incompatibility + launch cost kill option D), PoW-swap scope, open
   questions, reversal conditions. Needs sign-off to start P1.
9. [`09-gpu-posture.md`](09-gpu-posture.md) — GPU policy (tunable, not
   spec): 10× tripwire vs best low-end box, pre-committed `m_cost`→4 MiB
   first lever, measurement cadence, explicit non-triggers.
10. [`10-pow-swap-map.md`](10-pow-swap-map.md) — fork-eval result: exact
    EmberHash swap plan against the pinned `verus/` tree (hashFunction
    seam, interface decisions D1–D6, file-level work list, keep list,
    ordered P1 actions). C++ port + KAT gates green (`verus/` branch
    `lowmine-pow-eval`).
11. [`11-transactions-and-staking.md`](11-transactions-and-staking.md) —
    exploration: cheap/fast/anon transaction posture (honest finality, no
    fake instant-send), why classic masternodes are redundant here, and
    two PoS extensions (season stakes now, keeper nodes P3+).
12. [`12-network-as-game.md`](12-network-as-game.md) — deep design: the
    network as a game under one law (variance-dressed labor, never
    wagering), mechanic catalog (fee-rebate lottery, bingo, stake draws,
    near-miss feed), funding conservation table, unique PoS tie-in
    (stake-bonded keepers), legal/platform gates, surfaces (official pool +
    website play section), and §12.10: the ten-class luck table
    (`lm-rewards::luck`, exact conservation tested), the fee wall
    (no fee→odds, no fee→privacy), and prize flow (auto-pay on-chain,
    claim-and-expire off-chain, keepers never custody).
13. [`13-naming.md`](13-naming.md) — naming: **DECIDED 2026-10-11 —
    "Lucky Lode", primary domain `luckylode.cc`** (§13.8–13.9 with
    execution checklist: register domain on sight, defensive
    `luckylode.net`, ticker choice pending, rebrand via doc 04 §4.3).
    Full screening history: finalists Stoke/Lode/Mote, authoritative-RDAP
    diligence, demoted candidates (Hearth/Trove/Phosphor/Kiln). Algo
    rename contest still open (params frozen).
14. [`14-licensing.md`](14-licensing.md) — license posture: fork base is
    MIT (one-way compatible); verified precedents (geth LGPL/GPL split,
    Algorand AGPL); **adopted provisionally under "proceed" 2026-10-11**:
    GPL-3.0-only for code (LICENSE), CC-BY-SA 4.0 for docs; AGPL deferred
    per SaaS component; DCO+CLA before outside contributions.

## License

Code: **GPL-3.0-only** (see [`LICENSE`](LICENSE)). Design docs: **CC-BY-SA
4.0** (share and adapt, credit Lucky Lode, keep derivatives open). The
Verus fork base is MIT — its notices are preserved in the fork repository
(`verus/README.LOWMINE.md` documents the mix). Decision recorded in
[doc 14](14-licensing.md); pre-release it is one commit to change.

Code: [`lm-pow/`](lm-pow/) — reference EmberHash v1 implementation
(hash + verify + exact LWMA/nBits difficulty + jackpot trigger `T/100`;
`cargo test` runs unit + frozen KAT + difficulty suites, `cargo bench` for
timing);
[`lm-rewards/`](lm-rewards/) — jackpot reserve state machine (levy/bonus/
cap/league-overflow, conservation-tested) + reference bonus split (sqrt
weights, cap, dust; conservation-tested) + offline mirror of the doc 06
§6.5 testnet gates (`tests/jackpot_gates.rs`: 20k-block rate, neutrality,
warm-up reboot, independent split reference); [`lm-sched/`](lm-sched/) —
thermal/power scheduler policy (scripted-scenario tested);
[`lm-miner/`](lm-miner/) — reference CPU/ARM miner (parallel nonce search
+ benchmark mode with JSON device-matrix rows, doc 05 §5.6; Stratum V2
pool client tracked as follow-up);
[`lm-pool/`](lm-pool/) — pool-side fun-layer accounting (streaks, comeback,
overnight weights, device-class leagues, PPLNS ledger, jackpot + league
payouts, leaderboard; docs 03, 06);
[`lm-stratum/`](lm-stratum/) — reference pool wire layer (Stratum V1 framing
+ Lucky Lode job profile, mock-chain template provider with real jackpot/money
rules, threaded server + `pool_server` binary + client; testnet-in-a-box,
Stratum V2 tracked).

```sh
cargo test --workspace   # all 100 tests across the six crates
cargo clippy --workspace --all-targets -- -D warnings
```

## Non-goals (for now)

- iOS on-device mining (Apple bans it; iOS gets non-mining participation).
- Privacy tech beyond standard addresses (no ZK scope creep — that's
  SilentNet's job).
- Smart contracts / DeFi at launch (possible later via Verus PBaaS-style
  inheritance *if* Verus is forked).
