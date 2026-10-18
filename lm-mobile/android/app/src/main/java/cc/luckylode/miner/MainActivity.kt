package cc.luckylode.miner

import android.app.Activity
import android.content.Intent
import android.os.Bundle
import android.view.Gravity
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import android.util.Log

/**
 * One-tap start (doc 07 §7.4) with the trust anchor up front: the
 * frozen scheduler vectors run natively on first launch, so a
 * sideloaded binary proves it carries the same policy as every other
 * Lucky Lode surface before it asks for anything.
 */
class MainActivity : Activity() {
    companion object { private const val TAG = "LuckyLodeMiner" }

    private lateinit var status: TextView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val pad = (resources.displayMetrics.density * 16).toInt()
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(pad, pad, pad, pad)
            gravity = Gravity.CENTER_HORIZONTAL
        }
        status = TextView(this).apply { text = selfTestLine() }
        root.addView(status)
        root.addView(
            Button(this).apply {
                text = "Start mining service"
                setOnClickListener {
                    startForegroundService(Intent(this@MainActivity, MiningService::class.java))
                    Toast.makeText(this@MainActivity, "scheduler running — see the notification", Toast.LENGTH_SHORT).show()
                }
            }
        )
        root.addView(
            Button(this).apply {
                text = "Stop mining service"
                setOnClickListener {
                    stopService(Intent(this@MainActivity, MiningService::class.java))
                }
            }
        )
        setContentView(root)
    }

    private fun selfTestLine(): String {
        val vectorsOk = try {
            NativeCore.nativeSchedulerSelfTest()
        } catch (e: Throwable) {
            Log.e(TAG, "core self-test threw", e)
            false
        }
        Log.i(TAG, "core self-test: " + (if (vectorsOk) "PASS" else "FAIL"))
        return if (vectorsOk) {
            "core self-test: PASS (frozen scheduler vectors)\n" +
                "battery: ${batteryLine()}\n" +
                "mining threads: next milestone (scheduler is live now)"
        } else {
            "core self-test: FAIL — this build must not mine"
        }
    }

    private fun batteryLine(): String {
        val bm = getSystemService(BATTERY_SERVICE) as android.os.BatteryManager
        val pct = bm.getIntProperty(android.os.BatteryManager.BATTERY_PROPERTY_CAPACITY)
        val charging = bm.isCharging
        return "$pct%${if (charging) " charging" else " on battery"}"
    }
}
