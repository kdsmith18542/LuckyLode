package cc.luckylode.miner

import android.app.Activity
import android.content.Intent
import android.os.Bundle
import android.text.InputType
import android.view.Gravity
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import android.util.Log

/**
 * One-tap start (doc 07 §7.4) with the trust anchor up front: the
 * frozen scheduler vectors run natively on first launch, so a
 * sideloaded binary proves it carries the same policy as every other
 * Lucky Lode surface before it asks for anything.
 *
 * The pool address and worker name persist locally (SharedPreferences);
 * they are the only mining identity this app ever holds.
 */
class MainActivity : Activity() {
    companion object { private const val TAG = "LuckyLodeMiner" }

    private lateinit var status: TextView
    private lateinit var poolInput: EditText
    private lateinit var workerInput: EditText

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val prefs = getSharedPreferences("miner", MODE_PRIVATE)

        val pad = (resources.displayMetrics.density * 16).toInt()
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(pad, pad, pad, pad)
            gravity = Gravity.CENTER_HORIZONTAL
        }
        status = TextView(this).apply { text = selfTestLine() }
        root.addView(status)

        val fieldPad = (resources.displayMetrics.density * 4).toInt()
        poolInput = EditText(this).apply {
            hint = "pool (host:port)"
            inputType = InputType.TYPE_CLASS_TEXT
            setText(prefs.getString(MiningService.KEY_POOL, MiningService.DEFAULT_POOL))
            setSingleLine()
        }
        workerInput = EditText(this).apply {
            hint = "worker name"
            inputType = InputType.TYPE_CLASS_TEXT
            setText(prefs.getString(MiningService.KEY_WORKER, MiningService.DEFAULT_WORKER))
            setSingleLine()
        }
        fun label(text: String): TextView = TextView(this).apply {
            this.text = text
            setPadding(fieldPad, fieldPad, fieldPad, 0)
        }
        root.addView(label("Pool"))
        root.addView(poolInput)
        root.addView(label("Worker"))
        root.addView(workerInput)

        root.addView(
            Button(this).apply {
                text = "Start mining service"
                setOnClickListener {
                    prefs.edit()
                        .putString(MiningService.KEY_POOL, poolInput.text.toString().trim())
                        .putString(MiningService.KEY_WORKER, workerInput.text.toString().trim())
                        .apply()
                    startForegroundService(Intent(this@MainActivity, MiningService::class.java))
                    Toast.makeText(this@MainActivity, "mining — see the notification", Toast.LENGTH_SHORT).show()
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
                "battery: ${batteryLine()}"
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
