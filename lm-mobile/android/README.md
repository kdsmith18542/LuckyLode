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
  through the frozen policy, and keeps the notification honest.
- `app/src/main/java/cc/luckylode/miner/MainActivity.kt` — one-tap
  start with the trust anchor up front: the frozen scheduler vectors
  run natively on launch and the app says PASS or refuses to look
  functional on FAIL.
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
adb shell am start -n cc.luckylode.miner/.MainActivity
adb logcat -s LuckyLodeMiner   # → "core self-test: PASS"
```

## Status / next milestones

- [x] Rust JNI facade + native self-test (host + in-app)
- [x] Foreground service, scheduler live on real battery temperature
- [ ] Pool mining runtime (V2 Noise session over the wire; the
      scheduler's thread count gates the grind)
- [ ] Dashboard (hashrate, shares, streak, league — doc 07 §7.4)
- [ ] Battery-honesty copy + attestation hook (doc 07 §7.4/§7.6)
