package org.fruitiex.oriel

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.PowerManager
import android.provider.Settings
import android.util.Log

/**
 * Another device asks to see this phone (a push from the hub).
 *
 * Unattended start needs two grants, made once over adb (see docs/hub.md):
 * PROJECT_MEDIA, so Android approves the screen share without asking, and
 * "display over other apps", so the app may open from the background. With
 * both, the app starts sharing and steps aside; otherwise a notification
 * asks whoever holds the phone.
 */
object Wake {
    fun request(context: Context, from: String) {
        if (Settings.canDrawOverlays(context)) {
            Log.i("oriel", "wake-up from $from: starting to share")
            turnScreenOn(context)
            context.startActivity(
                Intent(context, OrielActivity::class.java)
                    .setAction(OrielActivity.ACTION_WAKE)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            )
        } else {
            notify(context, from)
        }
    }

    /** A dark screen sends no frames; light it (the lock screen stays). */
    @Suppress("DEPRECATION")
    private fun turnScreenOn(context: Context) {
        val pm = context.getSystemService(PowerManager::class.java)
        if (pm.isInteractive) return
        pm.newWakeLock(
            PowerManager.SCREEN_BRIGHT_WAKE_LOCK or PowerManager.ACQUIRE_CAUSES_WAKEUP,
            "oriel:wake",
        ).acquire(5_000)
    }

    private fun notify(context: Context, from: String) {
        val nm = context.getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL, "Requests to share", NotificationManager.IMPORTANCE_HIGH),
        )
        val open = PendingIntent.getActivity(
            context, 0,
            Intent(context, OrielActivity::class.java)
                .setAction(OrielActivity.ACTION_SHARE)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val n = android.app.Notification.Builder(context, CHANNEL)
            .setContentTitle("$from wants to see this phone")
            .setContentText("Tap to share the screen with oriel")
            .setSmallIcon(context.applicationInfo.icon)
            .setContentIntent(open)
            .setAutoCancel(true)
            .build()
        nm.notify(2, n)
    }

    private const val CHANNEL = "wake"
}

/**
 * The same wake-up without a push, for testing:
 * `adb shell am broadcast -a org.fruitiex.oriel.WAKE -n org.fruitiex.oriel/.WakeReceiver`.
 * Only the shell and the system hold the DUMP permission it requires.
 */
class WakeReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        Wake.request(context, intent.getStringExtra("from") ?: "adb")
    }
}
