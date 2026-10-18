package cc.luckylode.miner

import android.app.Activity
import android.content.Intent
import android.content.SharedPreferences
import android.os.Bundle
import android.text.InputType
import android.view.Gravity
import android.view.View
import android.widget.Button
import android.widget.CheckBox
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import android.util.Log
import org.json.JSONObject
import java.io.ByteArrayOutputStream
import java.net.HttpURLConnection
import java.net.URL
import java.util.concurrent.atomic.AtomicReference

/**
 * One-tap start (doc 07 §7.4) with the trust anchor up front: the frozen
 * scheduler vectors run natively on first launch, so a sideloaded binary
 * proves it carries the same policy as every other Lucky Lode surface
 * before it asks for anything.
 *
 * The dashboard is the tamagotchi surface (doc 03 §3.1): local tiles
 * from the mining loop (always truthful — hashrate, shares, session),
 * device tiles (battery temp/level/charging), and pool tiles fetched
 * from the official pool's public API (this worker's pool-side shares,
 * jackpot/pot reserve, league pool, latest block). Battery honesty is
 * stated, never promised around (doc 07 §7.4).
 *
 * The advanced section exposes the §7.2 thresholds; the service reads
 * them at start. Pool address + worker name persist locally — the only
 * mining identity this app ever holds.
 */
class MainActivity : Activity() {
    companion object { private const val TAG = "LuckyLodeMiner" }

    private lateinit var status: TextView
    private lateinit var dashboard: TextView
    private lateinit var poolInput: EditText
    private lateinit var workerInput: EditText
    private lateinit var advanced: LinearLayout
    private lateinit var prefs: SharedPreferences
    private val ui = HandlerHelper()

    /** Latest pool-API snapshot (null = not fetched / not the official pool). */
    private var poolTiles: AtomicReference<String?> = AtomicReference(null)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        prefs = getSharedPreferences("miner", MODE_PRIVATE)

        val pad = (resources.displayMetrics.density * 16).toInt()
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(pad, pad, pad, pad)
            gravity = Gravity.CENTER_HORIZONTAL
        }
        status = TextView(this).apply { text = selfTestLine() }
        root.addView(status)
        dashboard = TextView(this).apply { text = "dashboard: start the service for live tiles" }
        root.addView(dashboard)

        val fieldPad = (resources.displayMetrics.density * 4).toInt()
        fun label(text: String): TextView = TextView(this).apply {
            this.text = text
            setPadding(fieldPad, fieldPad, fieldPad, 0)
        }
        fun input(hint: String, value: String, numeric: Boolean = false): EditText =
            EditText(this).apply {
                this.hint = hint
                inputType = if (numeric) InputType.TYPE_CLASS_NUMBER else InputType.TYPE_CLASS_TEXT
                setText(value)
                setSingleLine()
            }

        poolInput = input("pool (host:port)", prefs.getString(MiningService.KEY_POOL, MiningService.DEFAULT_POOL)!!)
        workerInput = input("worker name", prefs.getString(MiningService.KEY_WORKER, MiningService.DEFAULT_WORKER)!!)
        root.addView(label("Pool"))
        root.addView(poolInput)
        root.addView(label("Worker"))
        root.addView(workerInput)

        root.addView(
            Button(this).apply {
                text = "Start mining service"
                setOnClickListener { saveAndStart() }
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
        root.addView(
            Button(this).apply {
                text = "Advanced thresholds"
                setOnClickListener {
                    advanced.visibility = if (advanced.visibility == View.GONE) View.VISIBLE else View.GONE
                }
            }
        )

        // Doc 07 §7.2 thresholds; empty = keep the default (the service
        // passes non-positive values through and the core falls back).
        advanced = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            visibility = View.GONE
        }
        val pauseDeci = input("pause °C (default 43)", intPref(MiningService.KEY_PAUSE_DECI, 430))
        val resumeDeci = input("resume °C (default 38)", intPref(MiningService.KEY_RESUME_DECI, 380))
        val bandDeci = input("derate band °C (default 5)", intPref(MiningService.KEY_BAND_DECI, 50))
        val battFloor = input("battery floor % (default 25)", intPref(MiningService.KEY_BATT_FLOOR, 25), numeric = true)
        val overnightStart = input("overnight start hour (default 22)", intPref(MiningService.KEY_OVERNIGHT_START, 22), numeric = true)
        val overnightEnd = input("overnight end hour (default 6)", intPref(MiningService.KEY_OVERNIGHT_END, 6), numeric = true)
        val maxThreads = input("max threads (default: all cores)", intPref(MiningService.KEY_MAX_THREADS, Runtime.getRuntime().availableProcessors()), numeric = true)
        val chargeOnly = CheckBox(this).apply {
            text = "Charge-only (mine on battery just overnight)"
            isChecked = prefs.getBoolean(MiningService.KEY_CHARGE_ONLY, true)
        }
        advanced.addView(label("Pause line (°C, one decimal = 43.0)"))
        advanced.addView(pauseDeci)
        advanced.addView(label("Resume line (°C)"))
        advanced.addView(resumeDeci)
        advanced.addView(label("Derate band (°C under pause)"))
        advanced.addView(bandDeci)
        advanced.addView(label("Battery floor (%)"))
        advanced.addView(battFloor)
        advanced.addView(label("Overnight window (hours, local)"))
        advanced.addView(overnightStart)
        advanced.addView(overnightEnd)
        advanced.addView(label("Max threads"))
        advanced.addView(maxThreads)
        advanced.addView(chargeOnly)
        advanced.addView(
            Button(this).apply {
                text = "Save thresholds (applies on next start)"
                setOnClickListener {
                    val e = prefs.edit()
                    fun putInt(key: String, field: EditText, def: Int) {
                        val v = field.text.toString().trim().toIntOrNull()
                        e.putInt(key, if (v == null || v <= 0) def else v)
                    }
                    putInt(MiningService.KEY_PAUSE_DECI, pauseDeci, 430)
                    putInt(MiningService.KEY_RESUME_DECI, resumeDeci, 380)
                    putInt(MiningService.KEY_BAND_DECI, bandDeci, 50)
                    putInt(MiningService.KEY_BATT_FLOOR, battFloor, 25)
                    putInt(MiningService.KEY_OVERNIGHT_START, overnightStart, 22)
                    putInt(MiningService.KEY_OVERNIGHT_END, overnightEnd, 6)
                    putInt(MiningService.KEY_MAX_THREADS, maxThreads, Runtime.getRuntime().availableProcessors())
                    e.putBoolean(MiningService.KEY_CHARGE_ONLY, chargeOnly.isChecked)
                    e.apply()
                    Toast.makeText(this@MainActivity, "thresholds saved", Toast.LENGTH_SHORT).show()
                }
            }
        )
        root.addView(advanced)

        // Battery honesty (doc 07 §7.4): state the drain, default to
        // charge-only, never promise free energy.
        root.addView(
            TextView(this).apply {
                text = "Battery honesty: mining draws roughly 3–5 W. A typical " +
                    "~15 Wh phone battery empties in about 3 hours from full. " +
                    "Charge-only is the default: on battery, the scheduler mines " +
                    "only 22:00–06:00 and only above the 25% floor."
                textSize = 12f
                setPadding(fieldPad, pad, fieldPad, 0)
            }
        )
        setContentView(root)
    }

    private fun intPref(key: String, def: Int): String =
        (prefs.getInt(key, def)).toString()

    private fun saveAndStart() {
        prefs.edit()
            .putString(MiningService.KEY_POOL, poolInput.text.toString().trim())
            .putString(MiningService.KEY_WORKER, workerInput.text.toString().trim())
            .apply()
        startForegroundService(Intent(this, MiningService::class.java))
        Toast.makeText(this, "mining — see the notification", Toast.LENGTH_SHORT).show()
    }

    override fun onResume() {
        super.onResume()
        ui.post(dashboardTick)
        startPoolFetcher()
    }

    override fun onPause() {
        super.onPause()
        ui.remove(dashboardTick)
        stopPoolFetcher()
    }

    /** One fetcher thread per activity lifetime (no per-resume leaks). */
    @Volatile private var fetcher: Thread? = null

    private fun startPoolFetcher() {
        if (fetcher?.isAlive == true) return
        fetcher = Thread(::fetchPoolTilesForever, "pool-tiles").also { it.start() }
    }

    private fun stopPoolFetcher() {
        fetcher?.interrupt()
        fetcher = null
    }

    /** 1 Hz local tiles from the mining loop + battery sensors. */
    private val dashboardTick: Runnable = object : Runnable {
        override fun run() {
            val (tempDeci, battPct, charging) = batteryState()
            val temp = if (tempDeci >= 0) "${tempDeci / 10.0}°C" else "n/a"
            val power = if (charging) "charging" else "on battery"
            var line = "battery: $temp · $battPct% · $power"
            if (NativeCore.nativePoolRunning()) {
                val p = NativeCore.nativePoolStats().split(" ", limit = 6)
                if (p.size >= 6) {
                    val accepted = p[1].toLongOrNull() ?: 0L
                    val rejected = p[2].toLongOrNull() ?: 0L
                    val connected = p[3] == "1"
                    val hps = (p[4].toLongOrNull() ?: 0L) / 10.0
                    line = if (connected) {
                        "mining: ${"%.1f".format(hps)} H/s · $accepted shares · $rejected rejected"
                    } else {
                        "mining (reconnecting): $accepted shares"
                    } + "\n" + line
                }
            }
            poolTiles.get()?.let { line += "\n$it" }
            dashboard.text = line
            ui.postDelayed(this, 1_000)
        }
    }

    /**
     * Pool tiles from the official pool's public API every 30 s (a plain
     * HttpURLConnection on a dedicated thread — zero app dependencies).
     * Only the official pool host has a known public stats URL; other
     * pools keep purely local tiles.
     */
    private fun fetchPoolTilesForever() {
        try {
            while (!isFinishing) {
                val worker = prefs.getString(MiningService.KEY_WORKER, MiningService.DEFAULT_WORKER)
                val isOfficial = (prefs.getString(MiningService.KEY_POOL, MiningService.DEFAULT_POOL)
                    ?: "").contains("luckylode.cc")
                val fetched = if (isOfficial) fetchPoolTiles(worker ?: "") else null
                poolTiles.set(fetched)
                Thread.sleep(30_000)
            }
        } catch (e: InterruptedException) {
            // activity paused/destroyed — exit quietly
        }
    }

    private fun fetchPoolTiles(worker: String): String? {
        return try {
            val conn = URL(MiningService.POOL_STATS_URL).openConnection() as HttpURLConnection
            conn.connectTimeout = 10_000
            conn.readTimeout = 10_000
            val body = conn.inputStream.use { ins ->
                val buf = ByteArrayOutputStream()
                ins.copyTo(buf)
                buf.toString("UTF-8")
            }
            conn.disconnect()
            val d = JSONObject(body)
            val sb = StringBuilder()
            val pool = d.optJSONObject("pool")
            val jackpot = d.optJSONObject("jackpot")
            if (pool != null) {
                val me = (0 until d.optJSONArray("workers").length())
                    .map { d.optJSONArray("workers").optJSONObject(it) }
                    .firstOrNull { it?.optString("name") == worker }
                if (me != null) {
                    val streak = me.optInt("streak", 0)
                    val best = me.optInt("best_streak", 0)
                    sb.append("pool-side: ${me.optLong("shares")} shares · ${me.optLong("rejects")} rejected")
                    if (streak > 0) {
                        sb.append(" · streak ${streak}d")
                        if (best > streak) sb.append(" (best $best)")
                        sb.append("\n")
                    } else {
                        sb.append(" · no active streak\n")
                    }
                }
                sb.append("pool: ${pool.optInt("workers")} workers · ${pool.optLong("shares")} shares\n")
            }
            if (jackpot != null) {
                // Chain amounts are 8-decimal integers.
                sb.append(
                    "jackpot reserve: ${jackpot.optLong("reserve") / 100_000_000} coins · " +
                        "pot: ${jackpot.optLong("pot") / 100_000_000}\n"
                )
            }
            val blocks = d.optJSONArray("last_blocks")
            if (blocks != null && blocks.length() > 0) {
                val b = blocks.optJSONObject(0)
                val finder = b?.optString("finder_name", b.optString("finder"))
                sb.append("last block: #${b?.optLong("height")} by $finder")
            }
            sb.toString().trim().ifEmpty { null }
        } catch (e: Exception) {
            Log.w(TAG, "pool tiles fetch failed: ${e.message}")
            null
        }
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
            "core self-test: PASS (frozen scheduler vectors)"
        } else {
            "core self-test: FAIL — this build must not mine"
        }
    }

    private fun batteryState(): Triple<Int, Int, Boolean> {
        val bm = getSystemService(BATTERY_SERVICE) as android.os.BatteryManager
        val intent = registerReceiver(null, android.content.IntentFilter(Intent.ACTION_BATTERY_CHANGED))
        val temp = intent?.getIntExtra(android.os.BatteryManager.EXTRA_TEMPERATURE, -1) ?: -1
        val level = intent?.getIntExtra(android.os.BatteryManager.EXTRA_LEVEL, -1) ?: -1
        val scale = intent?.getIntExtra(android.os.BatteryManager.EXTRA_SCALE, -1) ?: -1
        val pct = if (level >= 0 && scale > 0) level * 100 / scale else
            bm.getIntProperty(android.os.BatteryManager.BATTERY_PROPERTY_CAPACITY)
        return Triple(temp, pct, bm.isCharging)
    }
}

/** UI-thread handler helper (plain android.os, zero dependencies). */
private class HandlerHelper {
    // The static main looper — valid at field-init time (an Activity's
    // own looper is not attached until onCreate).
    private val handler = android.os.Handler(android.os.Looper.getMainLooper())
    fun post(r: Runnable) = handler.post(r)
    fun postDelayed(r: Runnable, ms: Long) = handler.postDelayed(r, ms)
    fun remove(r: Runnable) = handler.removeCallbacks(r)
}
