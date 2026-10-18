package cc.luckylode.miner

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import android.content.IntentFilter
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
 * Stage note: this milestone runs the SCHEDULER live (real battery
 * temperature, the frozen policy deciding every tick). The mining
 * threads themselves land with the pool-client runtime milestone —
 * the notification says exactly that rather than faking hashrate.
 */
class MiningService : Service() {
    private var handle: Long = 0L
    private lateinit var thread: HandlerThread
    private lateinit var ticker: Handler
    private val channelId = "mining"

    override fun onCreate() {
        super.onCreate()
        val nm = getSystemService(NOTIFICATION_SERVICE) as NotificationManager
        nm.createNotificationChannel(
            NotificationChannel(
                channelId,
                "Mining scheduler",
                NotificationManager.IMPORTANCE_LOW,
            )
        )
        // Doc 07 §7.2 defaults; any non-positive value falls back in-core.
        handle = NativeCore.nativeSchedulerCreate(
            /* pauseDeci = */ 430,
            /* resumeDeci = */ 380,
            /* bandDeci = */ 50,
            /* battFloor = */ 25,
            /* chargeOnly = */ true,
            /* overnightStart = */ 22,
            /* overnightEnd = */ 6,
            /* maxThreads = */ Runtime.getRuntime().availableProcessors(),
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
            val reason = when (d.reason) {
                NativeCore.REASON_OVERHEAT -> "paused: hot (${tempDeci / 10.0}°C)"
                NativeCore.REASON_LOW_BATTERY -> "paused: battery below floor"
                NativeCore.REASON_NO_POWER -> "paused: charge-only outside 22:00–06:00"
                else -> if (d.mine) {
                    "scheduler ready: ${d.threads} threads${if (d.overnight) " · overnight" else ""}"
                } else {
                    "paused"
                }
            }
            val nm = getSystemService(NOTIFICATION_SERVICE) as NotificationManager
            nm.notify(NOTIF_ID, buildNotification(reason))
            ticker.postDelayed(this, 1_000)
        }
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
        if (handle != 0L) {
            NativeCore.nativeSchedulerDestroy(handle)
            handle = 0L
        }
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    companion object {
        private const val NOTIF_ID = 1
    }
}
