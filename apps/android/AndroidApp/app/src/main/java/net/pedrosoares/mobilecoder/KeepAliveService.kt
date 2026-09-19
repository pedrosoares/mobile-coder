package net.pedrosoares.mobilecoder

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.IBinder
import android.util.Log

/**
 * The reason the sandbox keeps running when the app is not on screen.
 *
 * Android freezes a cached app's processes - the whole cgroup, so proot and
 * everything under it goes too. A dev server started in here would accept a
 * connection and then never answer it: the browser sits waiting until the app
 * is back in front. A build stops mid-compile. A background job stops counting.
 *
 * A foreground service is the one thing that changes that: while it runs the
 * process is not cached, so it is not frozen. The cost is a notification the
 * user can see, which is the right trade - something *is* running on their
 * phone, and they should be able to tell, and stop it.
 *
 * It runs only while there is work: a turn, a background job, or the user
 * asking for it explicitly (see the settings switch). When the work ends, the
 * service stops and the phone gets to freeze the app like any other.
 */
class KeepAliveService : Service() {

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            // From the notification: the user wants the phone back.
            //
            // This turns the switch off as well as stopping the service. Left
            // on, the poll would see it a quarter of a second later and start
            // the service again - a Stop button that does nothing is worse than
            // no Stop button.
            Log.i(TAG, "keep-alive stopped from the notification")
            getSharedPreferences(MainActivity.ENDPOINT_PREFS, Context.MODE_PRIVATE)
                .edit()
                .putBoolean(MainActivity.PREF_KEEP_AWAKE, false)
                .apply()
            KeepAlive.userStopped = true
            stopSelf()
            return START_NOT_STICKY
        }
        val summary = intent?.getStringExtra(EXTRA_SUMMARY) ?: "Working"
        startForeground(NOTIFICATION_ID, notification(summary))
        // Not sticky: if the system kills us, whatever we were keeping alive is
        // gone too, and restarting an empty service would be a lie.
        return START_NOT_STICKY
    }

    private fun notification(summary: String): Notification {
        val manager = getSystemService(NotificationManager::class.java)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            manager?.createNotificationChannel(
                NotificationChannel(CHANNEL, "Running work", NotificationManager.IMPORTANCE_LOW)
                    .apply {
                        description = "Shown while the sandbox keeps running in the background."
                        setShowBadge(false)
                    },
            )
        }

        val open = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val stop = PendingIntent.getService(
            this,
            1,
            Intent(this, KeepAliveService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE,
        )

        return Notification.Builder(this, CHANNEL)
            .setContentTitle("mobile-coder")
            .setContentText(summary)
            .setSmallIcon(android.R.drawable.stat_notify_sync)
            .setContentIntent(open)
            .setOngoing(true)
            .addAction(Notification.Action.Builder(null, "Stop", stop).build())
            .build()
    }

    companion object {
        private const val TAG = "mobile-coder"
        private const val CHANNEL = "keep-alive"
        private const val NOTIFICATION_ID = 1
        const val ACTION_STOP = "net.pedrosoares.mobilecoder.STOP_KEEP_ALIVE"
        const val EXTRA_SUMMARY = "summary"
    }
}

/**
 * Starts and stops [KeepAliveService] as work comes and goes.
 *
 * Polled from the activity rather than pushed from Rust: starting a foreground
 * service is only allowed from the foreground, and the poll already runs there.
 */
object KeepAlive {
    private const val TAG = "mobile-coder"

    /** True after the user presses Stop, until work stops and starts again. */
    @Volatile
    var userStopped = false

    private var running = false
    private var lastSummary = ""

    /**
     * Bring the service in line with what is running.
     *
     * [summary] is what the notification says; a change to it updates the
     * notification in place rather than restarting anything.
     */
    fun sync(context: Context, wanted: Boolean, summary: String) {
        val wanted = wanted && !userStopped
        if (!wanted) {
            if (running) {
                context.stopService(Intent(context, KeepAliveService::class.java))
                running = false
                lastSummary = ""
                Log.i(TAG, "keep-alive off")
            }
            return
        }
        if (running && summary == lastSummary) return

        val intent = Intent(context, KeepAliveService::class.java)
            .putExtra(KeepAliveService.EXTRA_SUMMARY, summary)
        context.startForegroundService(intent)
        if (!running) Log.i(TAG, "keep-alive on: $summary")
        running = true
        lastSummary = summary
    }

    /**
     * Called when there is no work left, so the next job is kept alive again.
     *
     * Only the automatic half is re-armed here. The switch, if the user turned
     * it off from the notification, stays off: that was a decision, not a
     * reaction to one job.
     */
    fun workEnded() {
        userStopped = false
    }
}
