# Official pool: neutrality commitments

**Status:** published pre-testnet, binding from the first public block.
The project runs a reference pool so day-one miners have somewhere good
to point — and constrains it in writing so it can never become an empire
(doc 12 §12.9; doc 01 success criteria: no pool monoculture).

## Why an official pool exists

1. Day-one onboarding — a good pool must exist before third parties build one.
2. The reference pool in production is the fun layer's best-integrated home
   (divisions, streaks, draws are pool-side).
3. It realizes the dev-funding leg (doc 04 §4.2): the reference-pool fee
   slice, dashboard-traceable coin-for-coin.

## Commitments (all public, all verifiable)

1. **Same fees as the reference default.** No official-pool exclusives, no
   hidden cuts. The fee schedule is published; every coin is traceable on
   the dashboard (doc 04 §4.2 discipline).
2. **Every fun mechanic is an open spec.** Divisions, streaks, draws run the
   same `lm-pool` ledger rules everywhere — nothing works only on the
   official pool. Other pools implement identical rules from the same code.
3. **40% hashrate-share circuit breaker.** A live dashboard shows the
   official pool's share. If it exceeds ~40% sustained for a week: fee goes
   to 0, a public notice goes out, and the community is asked to spread.
   Committed here, before public testnet.
4. **Divisions never require the official pool.** Cross-pool season boards
   aggregate from opt-in pools' public APIs via the sidecar. Your rank
   follows you, not your pool.
5. **Solo mining gets equal prominence.** The solo guide ships alongside
   every pool recommendation, forever. The official pool does **not** count
   toward the ≥3-independent-pools success criterion — the target is ≥4
   pools total.
6. **Project-multisig fee custody.** Fee collection is project-multisig
   (doc 04 §4.2), with a documented hot-wallet float policy.
7. **Public post-mortems.** Uptime + DDoS plan before Season 1; every
   incident gets a public write-up (testnet discipline first).

## Ops posture

- Node RPC stays localhost-bound (never exposed); only the pool port + 443
  face the network. SSH keys + fail2ban, daily backups on, provider
  snapshots + DDoS protection before the pool is public.
- No mining on project infrastructure, ever (provider ToS + neutrality).

## How to hold us to this

Watch the dashboard: fee schedule, hashrate share, fee-wallet flows, and
the division-board source list are all public. If any commitment above
breaks, say so loudly — the circuit breaker in (3) exists to be used.
