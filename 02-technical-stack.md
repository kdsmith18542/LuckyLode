# 02 — Technical Stack: Fork, Algorithm, Modules

## 2.1 Fork Verus? — decision framework (Phase 0 gate)

**What a Verus fork buys:** battle-tested hybrid consensus, VerusHash 2.2
(ARM miners like VerusMiner9000/Primo point at your chain on day one),
VerusID (free identity/badges layer for doc 03 achievements), possible
PBaaS/DeFi inheritance later, merged-mining research surface.

**What it costs:** a large, old C++ (Komodo/Zcash-derived) monolith —
slow builds, gnarly consensus code you must not break, big audit surface,
and every future change pays the "don't touch consensus" tax.

**Options:**

| Option | Pros | Cons | Verdict |
|---|---|---|---|
| A. Fork Verus, keep consensus, modernize edges | Launch in months; ARM miner codebases portable with a small algo-variant patch; VerusID badges free | Inherit tech debt; C++ core forever | **Recommended default** |
| B. Fork TurtleCoin lineage | Tiny codebase, Chukwa/Argon2id already phone-ideal | Dead upstream, small audit base, C++ anyway, no ecosystem | Fallback if Verus eval fails |
| C. Fresh chain (Rust consensus) | Clean, modular, auditable | 12–24 months before mainnet; consensus bugs are existential | Reject for v1; revisit for v2 modules |
| D. No fork: launch as Verus PBaaS chain | Verus security + ecosystem free | Couples fate/schedule to Verus; weaker "own coin" story | Research in Phase 0, decide fast |

**Strangler-fig rule:** whatever the base, *consensus code is frozen* except
for the two planned changes below (lottery rewards §3.2, league/accounting
fields if needed). All innovation goes into replaceable modules around it.
Consensus changes are the #1 killer of small-chain projects.

## 2.2 Algorithm strategy (custom at genesis — rev 1.2)

**Decision (2026-10-10):** the project launches with purpose-built
**EmberHash v1** (Argon2id-based, phone-ideal) at genesis — full spec,
parameters, and test vectors frozen in [doc 05](05-pow-algorithm.md).
One algorithm means one identity and one benchmark story; the added risk
vs. a VerusHash-compatible launch is contained by conservative parameters
(1 MiB, single-lane), an 8+-week testnet mining the exact frozen
parameters, and a no-mainnet gate on third-party review (doc 05 §5.7).

**Branding is total from genesis.** Forking code is not co-branding: own
name, ticker, domain, logo, address version bytes, and network magic. The
address/magic change is safety-critical, not cosmetic — shared prefixes
with Verus risk cross-chain fund loss. Upstream copyright/license
attribution stays in code and a NOTICE file (legal requirement, invisible
to users). Pre-launch checklist (see doc 04 §4.3): name/ticker collision
search, domain + socials, string purge of user-facing surfaces, genesis
message.

**How custom (decided): full construction at genesis, staged tuning after.**

- **Genesis: EmberHash v1** — purpose-built Argon2id construction
  (1 MiB, single-lane, height-bound, domain-separated), frozen with real
  test vectors in [doc 05](05-pow-algorithm.md). This *is* the custom
  algorithm — no VerusHash-compatible stage, no later migration. The
  name-the-algo contest can still rename it; parameters don't move without
  a scheduled upgrade.
- **Later: parameter levers only** — `t_cost` up (GPU pressure), `m_cost`
  up (ASIC pressure, trading away old phones explicitly), via scheduled
  upgrades with testnet bake-in. Never a surprise fork.
- **Rejected for v1: dual-algo** (e.g. VerusHash queue + Argon2id queue
  with separate rewards — superficially perfect for leagues). It halves the
  effective 51% threshold per algorithm on a small chain and doubles audit
  surface. Revisit only at large hashrate.

Consequence, stated plainly: existing third-party miners do **not** work
day one — the reference Rust miner (`lm-pow` + scheduler + app) is
launch-critical path, and third-party ports follow our spec (doc 05 is
written to be implementable from). Testnet miner Bake-off is a P1 gate.

**ASIC policy (written, public):** scheduled resistance forks are a
*deterrent*, not a war. If ASICs appear, fork within 90 days; don't promise
more. RandomX-style "GPU-friendly accident" (GPUs dominating) is the real
historical killer of CPU coins — monitor GPU efficiency ratios quarterly.

## 2.3 The modern modular stack (language per job, not religion)

Keep the consensus core (C++ if Verus-forked). Rewrite/own everything a
user touches — that's where "modernize the stack" actually pays:

| Module | Language | Notes |
|---|---|---|
| Reference miner (CPU/ARM) | **Rust** core + C/ASM kernels | Rust for stratum, scheduling, thermal logic; hand-tuned NEON/AArch64 kernels where it counts (hashrate lives in kernels, safety lives in Rust). Static binaries for miniPC/Pi. |
| Thermal-aware scheduler | Rust (in-miner, ref: `lm-sched`, spec: doc 07) | big.LITTLE pinning, temp ceilings, charge-only mode, "overnight mode". This is *the* phone-mining feature — Primo-style, but built in. |
| Android app | Kotlin + Rust core (JNI) | One-tap: wallet → pool → start. Dashboard (shares, streaks, league rank), temp/battery guards, pause-on-discharge. No iOS miner (store ban) — iOS gets wallet + league spectating + faucet games. |
| Desktop wallets | Tauri (TS + Rust) or Electron-lite | Light, reproducible builds. Faucet + tipping built in (TRTL's killer social feature, resurrected). |
| Pool software | Rust or Go, **Stratum V2** | Open-source reference pool with the fun layer hooks (streaks, leagues, lottery accounting). Multiple independent pools required (doc 01 criteria). Solo-mining guide day one. |
| Fun-layer backend | TS/Go sidecar + indexer | Leaderboards, badges, seasons — reads chain + pool APIs, writes nothing to consensus. Swappable without forks. |
| Explorer + dashboard | TS | Network health, league tables, subsidy transparency (every dev-fee coin traceable). |

**Why Rust for the miner (honest):** not 2× hashrate — expect parity with
XMRig/ccminer. The wins are memory safety in long-running daemons, fearless
concurrency for heterogeneous topology handling, single static binaries for
ARM boards, and a contributor pool that actually wants to work in it. Kernels
stay intrinsics/ASM regardless of host language.

## 2.4 Consensus changes allowed (short allowlist — everything else is no)

1. **Lottery-block rewards** (doc 03 §3.2): VRF-style per-block draw,
   multiplier range, reserve/burn accounting. Needs spec + audit.
2. **League/accounting commitments** (only if pool-side leagues prove
   unworkable): minimal on-chain fields, never identity.
3. **Stage-2 algorithm swap**: pre-scheduled, testnetted, reviewed.

Anything else proposed for consensus must survive the question: *"can this
live pool-side or sidecar instead?"* If yes, it does.
