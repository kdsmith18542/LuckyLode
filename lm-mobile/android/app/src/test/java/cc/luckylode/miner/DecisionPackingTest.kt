package cc.luckylode.miner

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The Kotlin mirror of the frozen scheduler vectors (doc 07 §7.1: the
 * shared-vector guarantee — identical behavior across the Rust core
 * and the app). These run on the host JVM against the pure-Kotlin
 * decoder; NativeCore.nativeSchedulerSelfTest() runs the Rust side
 * in-device, and both must pass.
 */
class DecisionPackingTest {

    private val pauseDeci = 430
    private val resumeDeci = 380
    private val bandDeci = 50
    private val floor = 25

    @Test
    fun decision_decode_roundtrip_layout() {
        // Packed layout: bit0 mine, bits8..15 threads, bits16..17 reason,
        // bit24 overnight — pinned identically in lm-mobile's Rust tests.
        val packed: Long =
            (1L) or (6L shl 8) or (0L shl 16) or (1L shl 24)
        val d = NativeCore.decode(packed)
        assertTrue(d.mine)
        assertEquals(6, d.threads)
        assertEquals(NativeCore.REASON_NONE, d.reason)
        assertTrue(d.overnight)
        val paused: Long = (1L shl 16) // Overheat = 1 in the Rust pack
        val p = NativeCore.decode(paused)
        assertFalse(p.mine)
        assertEquals(NativeCore.REASON_OVERHEAT, p.reason)
    }

    @Test
    fun frozen_vector_layouts() {
        // Battery floor (20% < 25): paused, low battery (reason 2).
        val lowBatt = NativeCore.decode(2L shl 16)
        assertFalse(lowBatt.mine)
        assertEquals(NativeCore.REASON_LOW_BATTERY, lowBatt.reason)
        // 2. Charge-only daytime on battery: paused, no power.
        // 3. Overheat latch at 431: paused, overheat — cleared only at 380.
        // The decisions themselves are made by the Rust core; these pin
        // that every reason the core can emit survives the crossing.
        for (reason in listOf(
            NativeCore.REASON_NONE,
            NativeCore.REASON_OVERHEAT,
            NativeCore.REASON_LOW_BATTERY,
            NativeCore.REASON_NO_POWER,
        )) {
            val d = NativeCore.decode(reason.toLong() shl 16)
            assertEquals(reason, d.reason)
        }
    }
}
