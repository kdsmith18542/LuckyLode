package cc.luckylode.miner

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import android.content.IntentFilter
import android.content.SharedPreferences
import android.os.BatteryManager
import android.os.Handler
import android.os.HandlerThread
import android.os.IBinder
import java.util.Calendar

/**
 * The mining foreground service (doc 07 §7.4): Android kills background
 * workers, so the scheduler ticker lives here behind the persistent
 * notification, honest about why the icon stays.
 *
 * The scheduler is the actuator's gate: its decision starts/stops the
 * pool miner with the decided thread count (a count change restarts
 * the miner; the frozen lm-sched policy stays the only thermal/battery
 * authority). The notification reports real shares from the mining
 * loop — never a fake hashrate. Scheduler thresholds come from the
 * app's advanced screen (doc 07 §7.2 defaults when untouched).
 */
class MiningService : Service() {
    private var handle: Long = 0L
    private lateinit var thread: HandlerThread
    private lateinit var ticker: Handler
    private val channelId = "mining"
    private lateinit var prefs: SharedPreferences

    /** Thread count the running miner was started with (-1 = not mining). */
    private var minedThreads = -1

    override fun onCreate() {
        super.onCreate()
        prefs = getSharedPreferences("miner", MODE_PRIVATE)
        val nm = getSystemService(NOTIFICATION_SERVICE) as NotificationManager
        nm.createNotificationChannel(
            NotificationChannel(
                channelId,
                "Mining scheduler",
                NotificationManager.IMPORTANCE_LOW,
            )
        )
        // Doc 07 §7.2 defaults; the advanced screen persists overrides
        // (non-positive values fall back in-core).
        handle = NativeCore.nativeSchedulerCreate(
            prefs.getInt(KEY_PAUSE_DECI, 430),
            prefs.getInt(KEY_RESUME_DECI, 380),
            prefs.getInt(KEY_BAND_DECI, 50),
            prefs.getInt(KEY_BATT_FLOOR, 25),
            prefs.getBoolean(KEY_CHARGE_ONLY, true),
            prefs.getInt(KEY_OVERNIGHT_START, 22),
            prefs.getInt(KEY_OVERNIGHT_END, 6),
            prefs.getInt(KEY_MAX_THREADS, Runtime.getRuntime().availableProcessors()),
        )
        startForeground(NOTIF_ID, buildNotification("scheduler starting…"))
        thread = HandlerThread("scheduler").also { it.start() }
        ticker = Handler(thread.looper)
        ticker.post(tick)
    }

    private val tick: Runnable = object : Runnable {
        override fun run() {
            if (handle == 0L) return
            val (tempDeci, battPct, charging) = batteryState()
            val hour = Calendar.getInstance().get(Calendar.HOUR_OF_DAY)
            val d = NativeCore.decode(
                NativeCore.nativeSchedulerDecide(handle, tempDeci, battPct, charging, hour)
            )
            val running = NativeCore.nativePoolRunning()
            when {
                d.mine && !running -> if (startMiner(d.threads)) minedThreads = d.threads
                !d.mine && running -> {
                    NativeCore.nativePoolStop()
                    minedThreads = -1
                }
                d.mine && running && d.threads != minedThreads -> {
                    // A derate (thermal band) changes the count mid-session:
                    // restart the miner at the new count.
                    NativeCore.nativePoolStop()
                    if (startMiner(d.threads)) minedThreads = d.threads
                }
            }
            val text = if (d.mine && NativeCore.nativePoolRunning()) {
                miningLine()
            } else {
                pauseReason(d, tempDeci)
            }
            val nm = getSystemService(NOTIFICATION_SERVICE) as NotificationManager
            nm.notify(NOTIF_ID, buildNotification(text))
            ticker.postDelayed(this, 1_000)
        }
    }

    private fun startMiner(threads: Int): Boolean {
        val v2 = prefs.getBoolean(KEY_V2, false)
        var pool = prefs.getString(KEY_POOL, DEFAULT_POOL) ?: DEFAULT_POOL
        // The official pool serves V2 on its own port; when V2 is on and
        // the pool field still points at the V1 default, switch to the V2
        // endpoint (the field itself stays untouched so toggling back is
        // trivial).
        if (v2 && pool == DEFAULT_POOL) pool = DEFAULT_POOL_V2
        val authority = prefs.getString(KEY_V2_AUTHORITY, DEFAULT_V2_AUTHORITY)
            ?.trim().orEmpty()
        return NativeCore.nativePoolStart(
            pool,
            prefs.getString(KEY_WORKER, DEFAULT_WORKER) ?: DEFAULT_WORKER,
            "",
            threads,
            0,
            v2,
            authority,
        )
    }

    /** Real counters from the mining loop (never a fabricated rate). */
    private fun miningLine(): String {
        val parts = NativeCore.nativePoolStats().split(" ", limit = 6)
        if (parts.size < 6) return "mining…"
        val accepted = parts[1].toLongOrNull() ?: 0L
        val connected = parts[3] == "1"
        val hps = (parts[4].toLongOrNull() ?: 0L) / 10.0
        return if (connected) {
            "mining: $accepted shares · ${"%.1f".format(hps)} H/s"
        } else {
            parts[5]
        }
    }

    private fun pauseReason(d: NativeCore.Decision, tempDeci: Int): String = when (d.reason) {
        NativeCore.REASON_OVERHEAT -> "paused: hot (${tempDeci / 10.0}°C)"
        NativeCore.REASON_LOW_BATTERY -> "paused: battery below floor"
        NativeCore.REASON_NO_POWER -> "paused: charge-only outside 22:00–06:00"
        else -> "paused"
    }

    /** Battery state via the sticky broadcast: temperature is already deci-°C. */
    private fun batteryState(): Triple<Int, Int, Boolean> {
        val intent = registerReceiver(null, IntentFilter(Intent.ACTION_BATTERY_CHANGED))
        val temp = intent?.getIntExtra(BatteryManager.EXTRA_TEMPERATURE, -1) ?: -1
        val pct = intent?.let {
            val level = it.getIntExtra(BatteryManager.EXTRA_LEVEL, -1)
            val scale = it.getIntExtra(BatteryManager.EXTRA_SCALE, -1)
            if (level >= 0 && scale > 0) level * 100 / scale else 50
        } ?: 50
        val plugged = intent?.getIntExtra(BatteryManager.EXTRA_PLUGGED, -1) ?: -1
        return Triple(temp, pct, plugged > 0)
    }

    private fun buildNotification(text: String): Notification =
        Notification.Builder(this, channelId)
            .setSmallIcon(android.R.drawable.stat_sys_download)
            .setContentTitle("Lucky Lode Miner")
            .setContentText(text)
            .setOngoing(true)
            .build()

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int = START_STICKY

    override fun onDestroy() {
        ticker.removeCallbacks(tick)
        thread.quitSafely()
        NativeCore.nativePoolStop()
        if (handle != 0L) {
            NativeCore.nativeSchedulerDestroy(handle)
            handle = 0L
        }
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    companion object {
        private const val NOTIF_ID = 1
        const val KEY_POOL = "pool"
        const val KEY_WORKER = "worker"

        /** Doc 07 §7.2 thresholds (advanced screen overrides). */
        const val KEY_PAUSE_DECI = "pauseDeci"
        const val KEY_RESUME_DECI = "resumeDeci"
        const val KEY_BAND_DECI = "bandDeci"
        const val KEY_BATT_FLOOR = "battFloor"
        const val KEY_CHARGE_ONLY = "chargeOnly"
        const val KEY_OVERNIGHT_START = "overnightStart"
        const val KEY_OVERNIGHT_END = "overnightEnd"
        const val KEY_MAX_THREADS = "maxThreads"

        /** Opt-in device attestation (doc 03 §3.6; self-labeled on testnet). */
        const val KEY_ATTEST = "attest"

        /** Stratum V2 (Noise) wire + the pool's authority URL key pin. */
        const val KEY_V2 = "v2"
        const val KEY_V2_AUTHORITY = "v2Authority"

        /** Public testnet pool (internal/testnet-runbook: ufw-open direct stratum). */
        const val DEFAULT_POOL = "luckylode.cc:3346"
        const val DEFAULT_WORKER = "android"

        /** The official pool's Stratum V2 endpoint (Noise NX + Lucky jobs). */
        const val DEFAULT_POOL_V2 = "luckylode.cc:3347"

        /** The official pool's published V2 authority URL key (responder pin). */
        const val DEFAULT_V2_AUTHORITY = "9bzFz1Zf5AeQz51XqEcokUtip8P2Ci5DCeBRjoRpnLqnUWC74nn"

        /** The official pool's public dashboard API (nginx-proxied). */
        const val POOL_STATS_URL = "https://luckylode.cc/pool/api/stats"

        /** Device-attestation verdict endpoint (verdict-only, no identity). */
        const val POOL_ATTEST_URL = "https://luckylode.cc/pool/api/attest"
    }
}
