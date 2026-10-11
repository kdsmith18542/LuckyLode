# 07 — Android Miner & Thermal Scheduler (spec + policy)

**Status:** scheduler policy frozen rev 1.0 (reference: `lm-sched`
crate). App UX is draft — it follows the policy, not vice versa.

## 7.1 Why the scheduler is a first-class deliverable

On a phone, the difference between a miner that respects thermals and one
that doesn't is the difference between retention and a 1-star review
citing a hot pocket and a dead battery. The scheduler is what makes phone
mining *sustainable*: mine overnight while charging, derate instead of
crash, never drain a daytime battery. Reference implementation:
`lm-sched` (pure logic, integer math) — the Kotlin app calls it over JNI
with identical behavior guaranteed by shared vectors (every test below is
mirrored in-app).

## 7.2 Frozen policy (normative defaults)

| Parameter | Default | Notes |
|---|---|---|
| Pause line | 43.0 °C | At/above: stop workers |
| Resume line | 38.0 °C | Hysteresis: resume only here or below (no flapping) |
| Derate band | 5.0 °C under pause | Within band: half threads instead of stopping |
| Battery floor | 25% | Below: always pause, even on charger |
| Charge-only | ON | Battery mining only inside overnight window |
| Overnight window | 22:00–06:00 local | Opt-in; battery mining needs ≥ floor + 20% |
| Full threads | core count | Derate halves, minimum 1 |

Temperatures are deci-°C integers end to end (no float policy drift
between Rust core and Kotlin UI). Unknown sensor (`None`) skips thermal
checks but never power/battery gates — fail operational on sensors, fail
safe on power. Degenerate configs (resume ≥ pause, zero threads) are
rejected at construction.

## 7.3 Decision order (exact — `Scheduler::decide`)

1. Battery below floor → pause (`LowBattery`), latch cleared.
2. Power gate: charging, or charge-only off, or (overnight active and
   battery ≥ floor + 20) — else pause (`NoPower`).
3. Thermal latch: set at/above pause, cleared at/below resume; latched →
   pause (`Overheat`). Latch overrides the overnight window.
4. Derate band → mine at half threads; else full threads.
5. Output reports `overnight_active` for pool-side bonus accounting
   (doc 03 §3.4 overnight mode).

## 7.4 App requirements (Android)

- **Foreground service + persistent notification** (modern Android kills
  background workers; be honest in UI about why the icon stays).
- **One-tap start**: wallet → pool → mine, with the scheduler preconfigured
  to §7.2 defaults; advanced screen exposes thresholds.
- **Dashboard**: hashrate, shares, streak day-count, league rank, jackpot
  alerts, temp/battery tiles — the tamagotchi surface (doc 03 §3.1).
- **Battery honesty**: show projected drain (typical phone ~15 Wh pack at
  ~5 W mining ≈ 3 h from full) and default to charge-only. Never promise
  free energy.
- **Attestation hook**: Play Integrity verdict plumbing for phone-league
  entry (doc 03 §3.6) — verdict only, no identity, opt-in per league.

## 7.5 iOS companion (no on-device mining — store ban, doc 04 §4.5)

Wallet, league spectating + jackpot alerts, faucet games, badge gallery,
tipping. iOS users participate in everything except hashing; say so
plainly in the app description (Electroneum lesson, doc 01).

## 7.6 Platform risks (stated)

- OEM battery "optimizers" will kill the service on some skins; ship a
  vendor-specific exemption guide (standard practice, e.g. dontkillmyapp).
- Thermal ceilings vary by SoC; §7.2 defaults are conservative starting
  points, user-tunable, and the benchmark matrix (doc 05 §5.6) re-validates
  them per device class.
