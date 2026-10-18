package cc.luckylode.miner

/**
 * The Rust core (lm-mobile / liblm_mobile.so): the frozen thermal
 * scheduler (doc 07 §7.2) and the LuckyHash probe. Decisions cross the
 * boundary as packed longs — deci-°C integers end to end, no float
 * policy drift.
 */
object NativeCore {
    // Lazy load: the packed-decision decoder is pure Kotlin (and unit
    // tested on the host JVM, where the ARM Android .so cannot load);
    // the library binds the first time a native function is called on
    // the device.
    @Volatile private var loaded = false
    private fun ensureLoaded() {
        if (!loaded) {
            System.loadLibrary("lm_mobile")
            loaded = true
        }
    }

    /** Bit 0 mine, bits 8..15 threads, bits 16..17 reason, bit 24 overnight. */
    const val MASK_MINE: Long = 1L
    const val SHIFT_THREADS: Int = 8
    const val SHIFT_REASON: Int = 16
    const val SHIFT_OVERNIGHT: Int = 24

    const val REASON_NONE = 0
    const val REASON_OVERHEAT = 1
    const val REASON_LOW_BATTERY = 2
    const val REASON_NO_POWER = 3

    fun nativeSchedulerCreate(
        pauseDeci: Int, resumeDeci: Int, bandDeci: Int, battFloor: Int,
        chargeOnly: Boolean, overnightStart: Int, overnightEnd: Int, maxThreads: Int,
    ): Long {
        ensureLoaded()
        return nativeSchedulerCreate0(
            pauseDeci, resumeDeci, bandDeci, battFloor,
            chargeOnly, overnightStart, overnightEnd, maxThreads,
        )
    }

    private external fun nativeSchedulerCreate0(
        pauseDeci: Int, resumeDeci: Int, bandDeci: Int, battFloor: Int,
        chargeOnly: Boolean, overnightStart: Int, overnightEnd: Int, maxThreads: Int,
    ): Long

    fun nativeSchedulerDestroy(handle: Long) {
        ensureLoaded()
        nativeSchedulerDestroy0(handle)
    }

    private external fun nativeSchedulerDestroy0(handle: Long)

    /**
     * One decision. `tempDeci < 0` means the sensor is unavailable — the
     * core keeps the last thermal decision (fail operational on sensors).
     */
    fun nativeSchedulerDecide(
        handle: Long, tempDeci: Int, battPct: Int, charging: Boolean, hour: Int,
    ): Long {
        ensureLoaded()
        return nativeSchedulerDecide0(handle, tempDeci, battPct, charging, hour)
    }

    private external fun nativeSchedulerDecide0(
        handle: Long, tempDeci: Int, battPct: Int, charging: Boolean, hour: Int,
    ): Long

    /** LuckyHash `pow_hash(header, nonce, height)` as hex. */
    fun nativePowHash(headerHex: String, nonce: Long, height: Long): String {
        ensureLoaded()
        return nativePowHash0(headerHex, nonce, height)
    }

    private external fun nativePowHash0(headerHex: String, nonce: Long, height: Long): String

    /** The frozen scheduler vectors, run natively on this device. */
    fun nativeSchedulerSelfTest(): Boolean {
        ensureLoaded()
        return nativeSchedulerSelfTest0()
    }

    private external fun nativeSchedulerSelfTest0(): Boolean

    /** Kotlin-side decoder — mirrors the Rust pack layout exactly. */
    data class Decision(
        val mine: Boolean,
        val threads: Int,
        val reason: Int,
        val overnight: Boolean,
    )

    fun decode(packed: Long): Decision = Decision(
        mine = packed and MASK_MINE != 0L,
        threads = ((packed shr SHIFT_THREADS) and 0xff).toInt(),
        reason = ((packed shr SHIFT_REASON) and 0x3).toInt(),
        overnight = (packed shr SHIFT_OVERNIGHT) and 0x1 != 0L,
    )
}
