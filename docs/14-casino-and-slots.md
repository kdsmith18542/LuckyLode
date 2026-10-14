# 14 — Casino & Slots (Lucky Lode casino at `/casino`)

**Status:** owner-directed, 2026-10-14. This document supersedes the "no-wager"
law previously stated in docs 03 §3.2 and docs 12 §12.1/§12.6 **for the
first-party casino only**. Everything in this doc is a product/business spec;
nothing here changes consensus.

## 14.1 What it is

A first-party, real-wager **LKYL** casino served at `luckylode.cc/casino`,
owned and operated by the project. No third-party distribution, no aggregator,
no white-label. Players wager **small amounts of LKYL** and win LKYL.

Two launch titles, both Lucky-Lode-themed (the chain's ten luck classes are the
symbol ladder — `lm-rewards/src/luck.rs`):

- **Lucky Lode** — *Book*-style: 5×3, 10 lines, a scatter+wild
  "Prospector's Almanac", 3+ scatters → 10 free spins with a randomly chosen
  **expanding** symbol, plus a gamble round. Very high volatility, max win
  ≈ ×8000.
- **Lucky Lode Ways** — *ways* style: 6 reels × 2–7 rows (up to **117,649
  ways**), cascading reels with a rising win multiplier, free spins with an
  escalating multiplier.

**RTP = 91%** (owner-set) for both titles → **house edge ≈ 9%**.

**Naming:** the second title must not use "Megaways" — that is a Big Time
Gaming trademark. "Lucky Lode Ways" (or another clear name) is used instead.
"Book of…" is also avoided.

## 14.2 Fairness: provably-fair commit-reveal

Every spin is verifiable by the player, independent of the house:

1. The server generates a 32-byte secret `server_seed` and publishes its
   SHA-256 commitment before play.
2. The player supplies (or edits) a `client_seed`.
3. Spin `i` derives its reel outcomes from
   `HMAC-SHA256(server_seed, client_seed ‖ nonce ‖ cursor)` — a keystream of
   uniform values mapped to reel stops.
4. The player may rotate the server seed at any time; the old seed is revealed,
   so every past spin can be recomputed and checked.

Optional extra salt: a future LKYL block hash may be mixed in ("chain-linked"),
purely additive to the trust story.

Executable spec: `lm-casino/src/fair.rs`.

## 14.3 House edge flow (~9%)

On a public dashboard, the house edge is split (weights configurable):

| Destination | Purpose |
|---|---|
| Dev fund | docs 04 §4.2 funding policy |
| Lucky Pot top-up | feeds the on-chain pot (doc 06 §6.6) |
| Auto-airdrop to active miners | rewards verified PoW/PoS/Luckynode participation |
| Future game-layer services | league pool, tournaments, further games |

Accounting is exact: `Σ bets = Σ wins + Σ edge allocations` (asserted in tests,
same discipline as the rest of the money paths in this repo).

## 14.4 Mine-to-play / stake-to-play / serve-to-play

Participation feeds the casino in two ways:

1. **Tiers** — verified PoW shares, PoS stake, and active Luckynode service
   build a participation tier that grants rakeback (a share of your own edge
   back), free-spin drops, and tournament/lucky-hour eligibility.
2. **House-funded free spins** — a small, event-driven allowance (from the
   game-layer slice) lets newcomers play without depositing; free wins are
   withdrawable after a published wagering requirement.

**Owner/manager tooling:** an admin bonus engine can push **exclusive bonuses
to top miners / top casino spenders** (manual + rule-based), with a full audit
log.

Executable specs: `lm-casino/src/tier.rs`, `lm-casino/src/bonus.rs`.

## 14.5 Legal & launch posture

- **Testnet first:** `/casino` runs on play-money **tLKYL** with no real value
  (same legal footing as the faucet/regtest work). No gambling licence needed to
  build and iterate.
- **Licence in parallel:** obtain a crypto-friendly licence (Curaçao eGaming or
  Anjouan) plus a short counsel review before enabling real LKYL wagering.
- **Gates when real value is on:** 18+ age gate, geo-block restricted markets
  (US/UK/FR and others), KYC above a small threshold, responsible-gaming and
  self-exclusion surfaces.
- Gambling surfaces stay **web-only**; app-store binaries keep shipping
  mining/wallet/leagues only.

## 14.6 Marketing

Positioning: *"The only slots where every spin is verifiable and the edge comes
back to the people who mine, stake and serve."*

- Audience order: (1) existing miners/stakers/Luckynodes, (2) crypto-gambling
  crossover, (3) general crypto-casino traffic **after** licensing.
- Pillars: verifiable-not-certified (public spin verifier); edge-to-community
  (live dashboard); one game three ways in (PoW/PoS/Luckynode); fair-launch
  casino (no premine, transparent bankroll).
- Channels: Discord/Telegram/X + mining subreddits pre-launch; crypto
  YouTube/Twitch/Kick streamers + launch tournament; affiliate(revshare/CPA) +
  rakeback + SEO post-licence.
- Guardrails: 18+, geo-gated copy, no "guaranteed/risk-free" claims,
  responsible-gaming links everywhere.

## 14.7 Risks

- Licensing is the hard gate for real wagering.
- Bankroll variance (very high volatility, ×8000 max) — bound with bet caps,
  exposure limits, and an automatic bankroll throttle.
- Custody/hot-wallet security; cold reserve; strict float limits.
- Brand tension with the fair-launch story — mitigated by the mine-to-play
  framing, edge-return, and provable fairness.
- Trademark ("Megaways"/"Book of") — use owned names only.

## 14.8 Open items

- Exact edge-split weights; bet denominations and min/max; max-win cap.
- Licence jurisdiction and final geo-block list.
- Whether to add the LKYL-block salt and whether any prize links the on-chain
  Lucky Pot.
- Free-spin wagering-requirement multiplier and expiry.
