# Lucky Lode Miner — Android app

The phone miner (doc 07). Kotlin shell over the shared Rust core
(`lm-mobile`): the frozen thermal scheduler and LuckyHash live in the
same crates the desktop stack uses, so behavior is identical by
construction and pinned by shared vectors (Rust host tests, Kotlin
JVM tests, and the in-app native self-test all run the same cases).

**Distribution:** sideloaded APK from luckylode.cc — Google Play
policy prohibits on-device mining, so this app is not (and must not
be) listed there. Updates are signature-checked against our signing
key; `adb install -r` works for development.

## Layout

- `app/src/main/java/cc/luckylode/miner/NativeCore.kt` — JNI bindings
  (lazy library load; the pure-Kotlin decision decoder is host-testable).
- `app/src/main/java/cc/luckylode/miner/MiningService.kt` — the
  foreground service: the scheduler ticker (1 Hz) reads real battery
  temperature (deci-°C straight from `ACTION_BATTERY_CHANGED`), decides
  through the frozen policy, and starts/stops/restarts the pool miner
  at the decided thread count (a derate restarts it mid-session). The
  notification reports real counters from the mining loop — never a
  fabricated rate.
- `app/src/main/java/cc/luckylode/miner/MainActivity.kt` — one-tap
  start with the trust anchor up front: the frozen scheduler vectors
  run natively on launch and the app says PASS or refuses to look
  functional on FAIL. Pool address + worker name are set here and
  persist locally (defaults: the public testnet pool `luckylode.cc:3346`,
  worker `android`).
- `app/src/main/jniLibs/{arm64-v8a,x86_64}/liblm_mobile.so` — the Rust
  core (x86_64 is for the emulator and CI; phones load arm64-v8a).

Zero third-party app dependencies (plain `android.app`, no androidx):
the sideloaded binary stays auditable and the build hermetic.

## Build

Prerequisites: Android SDK (platform 34, build-tools 34.0.0), NDK
r27+, JDK 21, Gradle 8.14+.

1. Build the Rust core for both ABIs (from the repo root):

```sh
NDKBIN=$ANDROID_HOME/ndk/<ver>/toolchains/llvm/prebuilt/linux-x86_64/bin
for T in aarch64-linux-android x86_64-linux-android; do
  env PATH=$NDKBIN:$PATH \
    CC_${T//-/_}=$NDKBIN/$T24-clang \
    AR_${T//-/_}=$NDKBIN/llvm-ar \
    CARGO_TARGET_${T//-/_}_LINKER=$NDKBIN/$T24-clang \
    cargo build -p lm-mobile --release --target $T
done
cp target/aarch64-linux-android/release/liblm_mobile.so app/src/main/jniLibs/arm64-v8a/
cp target/x86_64-linux-android/release/liblm_mobile.so app/src/main/jniLibs/x86_64/
```

2. Build and sign (from this directory):

```sh
gradle :app:testDebugUnitTest :app:assembleDebug
$ANDROID_HOME/build-tools/34.0.0/apksigner sign \
  --ks <signing keystore> --out lucky-lode-miner.apk \
  app/build/outputs/apk/debug/app-debug.apk
```

The signing keystore is the app's identity — updates must be signed
with the same key. It lives off-repo (the repo is public).

## Test on the emulator

KVM-backed x86_64 emulator (the jniLibs includes x86_64 for this):

```sh
avdmanager create avd -n test34 -k "system-images;android-34;google_apis;x86_64"
emulator -avd test34 -no-window -gpu swiftshader_indirect
adb install -r lucky-lode-miner.apk
adb shell pm grant cc.luckylode.miner android.permission.POST_NOTIFICATIONS
adb shell am start -n cc.luckylode.miner/.MainActivity
adb logcat -s LuckyLodeMiner   # → "core self-test: PASS"
```

API 34 note: a fresh install has no runtime `POST_NOTIFICATIONS` grant,
so the service's notification (the only progress surface) stays
invisible until granted. The mining loop still runs and reconnects
with capped backoff — the notification text reports its real state
(`mining: N shares · X H/s`, `pool connect failed: …; retrying in Ns`,
pause reasons).

A no-node local pool for wire testing: `cargo run -p lm-stratum --bin
mock_pool -- 0.0.0.0:3344`, then point the app at `10.0.2.2:3344` (the
emulator's alias for the host loopback). Every share wins on the mock
chain — useful for fast UI/runtime verification without testnet coins
at stake.

## Status / next milestones

- [x] Rust JNI facade + native self-test (host + in-app)
- [x] Foreground service, scheduler live on real battery temperature
- [x] Pool mining runtime (V1 stratum; the scheduler's thread count
      gates the grind) — proven live on the emulator against the
      reference mock pool (`lm-stratum`'s `mock_pool` on the host,
      reached at `10.0.2.2:<port>`) and against the public testnet
      pool `luckylode.cc:3346` (worker `android-emulator`: shares
      accepted, 0 rejects, pool-side rate agreed with the app's
      self-report)
- [x] Dashboard (doc 07 §7.4): local tiles from the mining loop
      (hashrate, shares, rejected), device tiles (battery temp/level/
      charging), pool tiles from the official pool's public API every
      30 s (this worker's pool-side shares, pool-wide stats, jackpot
      reserve/pot, latest block), battery-honesty copy, and the
      advanced screen exposing the §7.2 thresholds (persisted; the
      service reads them at start). Live-verified on the emulator
      against the testnet pool (in-app tiles matched a direct API
      fetch).
- [ ] Stratum V2 Noise session as a selectable wire (the runtime is
      wire-agnostic; V2 rides the same loop behind a flag)
- [ ] Attestation hook (doc 07 §7.6/§7.4) — verdict-only plumbing for
      phone-league entry; needs the league side to exist first
- [ ] Streak/league-rank tiles: not in the pool's public API yet —
      add pool-side endpoints first (streak day-count per worker)
