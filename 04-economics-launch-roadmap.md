# 04 — Economics, Launch, Roadmap, Risks

## 4.1 Emission: fair + forever (anti-fade by construction)

- **No premine, no ICO** (non-negotiable for the narrative; TRTL lesson).
- **Declining base schedule** (e.g. halving-ish every 2–4 years) **plus a
  permanent tail emission** (Monero-style, e.g. fixed minimum per block
  forever). The tail is the single most important anti-fade mechanism:
  miners can never face a future of fees-only on a low-fee chain.
- **Jackpot funding** (§3.2): jackpots paid from a small per-block lottery
  levy (e.g. 2–5% of each block reward into a jackpot reserve), smoothed so
  jackpot variance doesn't starve base rewards. Progressive split on payout.
- Fees: minimal anti-spam floor; no fee burns at launch (keep the economy
  legible while small).

## 4.2 Dev funding that doesn't break fair-launch trust

TRTL died partly of volunteer burnout. Pick the least-evil transparent mix:

1. **Reference-miner dev fee 1%** (XMRig-norm, visible in logs, ideally
   opt-out-able with a nag — transparency over coercion).
2. **Reference-pool fee slice** (e.g. 0.5pp of pool fee) to a published
   multisig address, dashboard-traceable coin-for-coin.
3. **Optional treasury block share, governance-capped** (e.g. ≤5%, sunsets
   unless renewed by community vote) — only if 1+2 underfund; contentious,
   so default to *off* at launch and let the community vote it on.

All funding addresses + spends on a public dashboard from day one. The
moment funding looks sneaky, the fair-launch story dies and so does the coin.

## 4.3 Launch sequence (fairness is the marketing)

1. **Public testnet** (4–8 weeks):/resettable, faucet, competitions to
   break things, published post-mortems. No real value.
2. **Fair-launch countdown**: binaries + pools + guides published ≥2 weeks
   before block 1; genesis timestamp public; no friends-and-family head start.
3. **Rebrand checklist (must clear before countdown):** name/ticker/domain/
   socials secured + collision-searched; address version bytes and network
   magic distinct from Verus (cross-chain safety); user-facing string purge;
   upstream license/copyright attribution in code + NOTICE file; genesis
   message written; name-the-algo contest concluded (doc 02 §2.2).
3. **Launch day = event**: live leaderboards, first-jackpot watch party,
   league signups open. Treat genesis like a game launch, not a deploy.
4. **Season 1 starts at block N** (e.g. 1 month in): leagues, streaks,
   badges live.

## 4.4 Phased roadmap

| Phase | Scope | Exit gate |
|---|---|---|
| **P0 — Decision** (4–6 wks) | Fork eval (doc 02 options A/D), brand + rebrand checklist, PoW spec frozen ([doc 05](05-pow-algorithm.md) rev 1.0 ✓) | Written fork decision + frozen PoW params + vectors |
| **P1 — Testnet** | Forked node mining EmberHash from block 1, reference Rust miner (`lm-pow`) + thermal scheduler, 1–2 ref pools (Stratum V2), wallets, faucet, miner bake-off (third-party ports) | Public testnet ≥8 weeks on frozen params, 1k+ miners, device-matrix benchmarks published (doc 05 §5.6 gate), third-party PoW review commissioned |
| **P2 — Mainnet + Season 1** | Genesis event, Android app beta, leagues/streaks/badges live, 3+ pools | Review clean (no open High/Critical), 10k low-end accounts; median day-90 retention tracked |
| **P3 — Mobile GA** | Thermal engine v1 (`lm-sched` policy, doc 07), overnight mode, iOS companion (wallet/spectate/faucet games) | Phone-league participation doubled vs S1 |
| **P4 — Depth** | Attestation-gated leagues, solo guides push, P2 optional treasury vote | Farm/spoof rate bounded; funding runway ≥18 mo |
| **P5 — PoW tuning future** | Parameter levers (`t_cost`/`m_cost`) per measured GPU/ASIC pressure | Benchmarks + review, or explicit keep-params call |

## 4.5 Honest risks

1. **Botnets.** Any CPU coin gets them. They add hashrate but poison the
   story and invite exchange/AV trouble. Mitigation: pool-side anomaly
   detection + clear ToS, no illusions of protocol-level prevention.
2. **League spoofing by farms** (doc 03 §3.6). Bounded by sub-linear rewards
   + attestation; residual accepted in free tiers.
3. **Verus-core weight.** Big C++ base = slow iteration + audit surface.
   Mitigated by strangler-fig rule (doc 02 §2.1): consensus frozen except
   the allowlist.
4. **ASIC/GPU creep.** Written 90-day fork policy (doc 02 §2.2); monitor
   efficiency ratios quarterly.
5. **App-store policy.** Android only for mining; iOS gets honest
   non-mining roles. Never ship a "miner" that doesn't hash (Electroneum
   lesson).
6. **Exchange reality.** Small PoW coins struggle for listings; plan around
   DEX/atomic paths + tipping economy so the coin is *usable* before it's
   *listed*.
7. **Founder burnout (the TRTL killer).** Funding ( §4.2) + shared
   maintenance (multiple pool/client teams, seasonal cadence with breaks)
   are structural answers, not vibes.
