# 04 — Economics, Launch, Roadmap, Risks

## 4.1 Emission: fair + forever (anti-fade by construction)

- **No premine, no ICO** (non-negotiable for the narrative; TRTL lesson).
  The only genesis allocation is the **game-fund seed** (doc 06 §6.2/§6.6):
  consensus-locked, no keys, no address, spendable only through the
  jackpot/pot payout rules — it pays miners, never insiders.
- **Emission schedule — LOCKED 2026-10-13** (60 s blocks, 1,440/day):
  - **Week 1 (blocks 1–10,080): linear ramp 0 → 16 LKYL/block.** Early
    miners earn in proportion to hash as the reward climbs — no block-1
    payout spike to front-run, no head start for insiders.
  - **Years 1–8: 16 LKYL/block, halving every 2 years** (16 → 8 → 4 → 2).
  - **Year 9 onward: a fixed 1 LKYL/block forever** (Monero-style tail).
    The tail is the single most important anti-fade mechanism: miners can
    never face a future of fees-only on a low-fee chain.
  - ≈31.6M LKYL over 8 years, +525.6k/year tail. **Testnet mirrors this
    schedule exactly**, so the ramp and halvings are exercised before
    mainnet.
- **Jackpot + Lucky Pot funding** (§3.2, doc 06): a 5% per-block levy,
  split **4% jackpot reserve / 1% Lucky Pot**; jackpots pay the finder up
  to `5×R`, pot draws pay tiered splashes (10/25/50%, pity-timer floor);
  jackpot-reserve overflow feeds the prize fund, pot overflow burns.
  Expectation-neutral by construction — the game layer redistributes
  miner-earned value, it never mints.
- **Luckynodes** (docs/11 §11.6 Direction A,: cheap-bond
  service nodes paid from fun-layer budgets (prize fund), **never from
  inflation** — so the emission schedule funds only PoW miners and PoS
  stakers.
- Fees: minimal anti-spam floor; no fee burns at launch (keep the economy
  legible while small).

## 4.2 Dev funding that doesn't break fair-launch trust

TRTL died partly of volunteer burnout. **Policy LOCKED 2026-10-13** (the
least-evil transparent mix; implementation pending before mainnet):

1. **Reference-miner dev fee 1%** (XMRig-norm): visible in logs,
   opt-out-able with a nag — transparency over coercion.
2. **Reference-pool fee slice 0.5pp** to a published multisig address,
   dashboard-traceable coin-for-coin.
3. **Treasury block share: OFF by default** (≤5%, governance-capped,
   sunsets unless renewed by community vote) — only if 1+2 underfund.
4. **Seed capital, not premine** (internal/20): the faucet (100,000
   testnet) and the dev fund (50,000 testnet) start as consensus
   balances at genesis and pay out only through capped per-block
   coinbase claims to pinned addresses (faucet: any block; dev: every
   100th block). No spendable genesis outputs — the fair-launch story
   holds. Testnet pins: faucet `mxVc37AiDBhP4FcydkCu5AEZqgfMYy8hC9`,
   dev 2-of-3 `2NFdM6aq2Rtr444aGVrE9CZXLzZ2JF8dadE` (test keys;
   mainnet needs real keyholders + ceremony). Mainnet pins are empty
   until the ceremony — the rules are inert there, fail closed.

All funding addresses + spends on a public dashboard from day one. The
moment funding looks sneaky, the fair-launch story dies and so does the
coin. None of this touches the emission schedule (§4.1) or the game-fund
seed.

## 4.3 Launch sequence (fairness is the marketing)

1. **Public testnet** (4–8 weeks):/resettable, faucet, competitions to
   break things, published post-mortems. No real value.
2. **Fair-launch countdown**: binaries + pools + guides published ≥2 weeks
   before block 1; genesis timestamp public; no friends-and-family head start.
3. **Rebrand checklist (must clear before countdown):** name/ticker/domain/
   socials secured + collision-searched; address version bytes and network
   magic distinct from Verus (cross-chain safety) — **done 2026-10-13**
   (prefixes 111/196, magic `LucT`, `LUCKYLODETEST`); user-facing string
   purge — **node/miner/pool done 2026-10-13; feature names done
   2026-10-14** (VerusID → LuckyID, doc 13); upstream license/copyright
   attribution in code + NOTICE file — **done**; genesis message written —
   **done** (mainnet pin + testnet's own genesis); name-the-algo contest
   concluded (doc 02 §2.2) — **done 2026-10-14** (**LuckyHash**, docs/13).
3. **Launch day = event**: live leaderboards, first-jackpot watch party,
   division signups open. Treat genesis like a game launch, not a deploy.
4. **Season 1 starts at block N** (e.g. 1 month in): divisions, streaks,
   badges live.

**Pre-mainnet engineering gates (added 2026-10-17; each traces to a live
testnet incident or a known gap):**

- **Divergence rule:** any consensus-adjacent fix lands on testnet *and*
  mainnet params in the same commit (the `fCoinbaseMustBeProtected`
  bug made all transparent payouts invalid on testnet while mainnet
  still inherited the wrong value). Keep one divergence table in the
  fork and make every entry justified.
- **Replay-derivability audit** (doc 06 invariant): every added
  consensus field rebuilds byte-for-byte from a fresh reindex;
  serialization/reload paths re-tested after any field changes.
- **Node repo CI:** the fork gets its own build + KAT-gate CI (all four
  KAT gates) before the countdown — consensus-critical code must not
  depend on one machine's build ritual.
- **Registry persistence decision:** the luckynode registry is
  in-memory today (a pool restart unregisters every node) — persist it
  or move registration on-chain, decided and shipped before mainnet.
- **Wallet signature-hash coverage — DONE 2026-10-17:** the
  self-custody signer now carries all three epochs (sprout legacy +
  ZIP-143 + ZIP-243, `lm-keys::zip243`), vector-pinned against the
  fork's own regenerated test set (45 transparent v3 vectors, every
  hash-type shape, `lm-keys/tests/zip143.rs`); the wallet picks the
  epoch from the node's upgrade status at build time. Live end-to-end
  proof lands when sapling activates (testnet h207500, mainnet h227520
  as of 2026-10-17) — until then the wallet keeps signing the sprout
  format the chain validates.
- **Default-port truth:** fresh-install defaults come from chainparams
  only (P2P 9270/RPC 9271 mainnet, 19270/19271 testnet, doc 13 §13.3);
  per-install conf overrides are documented, never baked into software
  defaults.

## 4.4 Phased roadmap

| Phase | Scope | Exit gate |
|---|---|---|
| **P0 — Decision** (4–6 wks) | Fork eval (doc 02 options A/D), brand + rebrand checklist, PoW spec frozen ([doc 05](05-pow-algorithm.md) rev 1.0 ✓) | Written fork decision + frozen PoW params + vectors |
| **P1 — Testnet** | Forked node mining LuckyHash from block 1, reference Rust miner (`lm-pow`) + thermal scheduler, 1–2 ref pools (Stratum V1 proven on testnet; V2 framing in P2), wallets, faucet, miner bake-off (third-party ports) | Public testnet ≥8 weeks on frozen params, 1k+ miners, device-matrix benchmarks published (doc 05 §5.6 gate), third-party PoW review commissioned |
| **P2 — Mainnet + Season 1** | Genesis event, Android app beta, divisions/streaks/badges live, 3+ pools | Review clean (no open High/Critical), 10k low-end accounts; median day-90 retention tracked |
| **P3 — Mobile GA** | Thermal engine v1 (`lm-sched` policy, doc 07), overnight mode, iOS companion (wallet/spectate/faucet games) | Phone-division participation doubled vs S1 |
| **P4 — Depth** | Attestation-gated divisions, solo guides push, P2 optional treasury vote | Farm/spoof rate bounded; funding runway ≥18 mo |
| **P5 — PoW tuning future** | Parameter levers (`t_cost`/`m_cost`) per measured GPU/ASIC pressure | Benchmarks + review, or explicit keep-params call |

## 4.5 Honest risks

1. **Botnets.** Any CPU coin gets them. They add hashrate but poison the
   story and invite exchange/AV trouble. Mitigation: pool-side anomaly
   detection + clear ToS, no illusions of protocol-level prevention.
2. **Division spoofing by farms** (doc 03 §3.6). Bounded by sub-linear rewards
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
