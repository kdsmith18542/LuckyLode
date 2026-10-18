# Lucky Lode — Design Docs & Reference Code

**Name:** Lucky Lode — decided 2026-10-11; primary domain `luckylode.cc`
**registered 2026-10-11** (RDAP-verified, expires 2027-10-11).
**Naming:** Lucky Lode (docs/13 records every decision: LuckyID,
LuckyHash, LuckyVault, LuckyBridge). Kept deliberately: the `lm-*` crate
names, the legacy `-ac_algo=emberhash` alias for `-ac_algo=luckyhash`,
and the **frozen consensus strings** `LMv1` / `lowmine-pow/v1` — those
never change; they are construction identifiers, not branding (doc 05).

**One-liner:** a fair-launch, CPU/ARM-first mineable coin for the hardware
people already own — phones, miniPCs, laptops old and new — with mining
designed as a game you keep playing, not a chore you automate and forget.

**Status:** public testnet live — the node fork
([LuckyLode-node](https://github.com/kdsmith18542/LuckyLode-node), KAT
gates in CI) runs the chain; the reference pool, miner, explorer, faucet,
and desktop wallet are deployed against it; Stratum V2 ships alongside
V1.

## Docs

- [`01-thesis-and-positioning.md`](docs/01-thesis-and-positioning.md) — why this
  niche is open, who it's for, what killed the predecessors.
- [`02-technical-stack.md`](docs/02-technical-stack.md) — fork-Verus analysis,
  algorithm strategy, modular modern stack (what to keep, what to rewrite,
  what language where).
- [`03-fun-and-retention.md`](docs/03-fun-and-retention.md) — the excitement
  layer: lottery blocks, device-class leagues, streaks, seasons, badges, and
  the anti-fade engine. This is the core differentiator — read it first
  after the thesis.
- [`04-economics-launch-roadmap.md`](docs/04-economics-launch-roadmap.md) —
  emission (incl. tail), dev funding without breaking fair-launch trust,
  launch sequence, phased roadmap, and honest risks (botnets, league
  spoofing, app-store policy, ASICs).
- [`05-pow-algorithm.md`](docs/05-pow-algorithm.md) — **LuckyHash v1**: the
  custom low-end-hardware PoW, FROZEN rev 1.0 with real test vectors,
  difficulty rules, fairness posture, and mainnet gates.
- [`06-jackpot-lottery.md`](docs/06-jackpot-lottery.md) — jackpot blocks (~1%,
  PoW-hash-as-ticket, expectation-neutral, progressive pool splits),
  FROZEN rev 1.0 with testnet gates.
- [`07-android-miner.md`](docs/07-android-miner.md) — Android app + frozen
  thermal/power scheduler policy (hysteresis, derate, overnight mode),
  iOS companion scope, platform risks.
- [`09-gpu-posture.md`](docs/09-gpu-posture.md) — GPU policy (tunable, not
  spec): 10× tripwire vs best low-end box, pre-committed `m_cost`→4 MiB
  first lever, measurement cadence, explicit non-triggers.
- [`11-transactions-and-staking.md`](docs/11-transactions-and-staking.md) —
  design: cheap/fast/anon transaction posture (honest finality), why
  classic masternodes are redundant here, and two PoS extensions (season
  stakes now, keeper nodes later).
- [`12-network-as-game.md`](docs/12-network-as-game.md) — deep design: the
  network as a game under one law (variance-dressed labor, never
  wagering), mechanic catalog (fee-rebate lottery, bingo, stake draws,
  near-miss feed), funding conservation table, stake-bonded keeper tie-in,
  legal/platform gates, surfaces (official pool + website play section),
  and the ten-class luck table (`lm-rewards::luck`, exact conservation
  tested).
- [`13-naming-and-brand-record.md`](docs/13-naming-and-brand-record.md) —
  every naming decision with dates, the frozen consensus strings, and
  the ports and datadirs on record.

## License

Code: **GPL-3.0-only** (see [`LICENSE`](LICENSE)). Design docs: **CC-BY-SA
4.0** (share and adapt, credit Lucky Lode, keep derivatives open). The
Verus fork base is MIT — its notices are preserved in the fork repository
(the node repository's NOTICE documents the mix).

Code: [`lm-pow/`](lm-pow/) — reference LuckyHash v1 implementation
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
+ benchmark mode with JSON device-matrix rows, doc 05 §5.6; Stratum V2 pool client included);
[`lm-pool/`](lm-pool/) — pool-side fun-layer accounting (streaks, comeback,
overnight weights, device-class leagues, PPLNS ledger, jackpot + league
payouts, leaderboard; docs 03, 06);
[`lm-stratum/`](lm-stratum/) — reference pool wire layer (Stratum V1 framing
+ Lucky Lode job profile, mock-chain template provider with real jackpot/money
rules, threaded server + `pool_server` binary + client; **real-node
provider verified live** (getblocktemplate/submitblock, blocks mined
through the pool on regtest); Stratum V2 framing, encrypted sessions,
and multi-channel mining are implemented and interop-tested against the
reference codec).

```sh
cargo test --workspace   # the full workspace suite (every crate)
cargo clippy --workspace --all-targets -- -D warnings
```

## Non-goals (for now)

- iOS on-device mining (Apple bans it; iOS gets non-mining participation).
- Privacy tech beyond standard addresses (no ZK scope creep in v1).
- Smart contracts / DeFi at launch (possible later via LuckyBridge-style
  inheritance; out of scope for v1).
