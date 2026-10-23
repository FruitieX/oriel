package org.fruitiex.oriel

import android.app.Application
import android.content.Context
import android.util.Log
import com.google.firebase.FirebaseApp
import com.google.firebase.FirebaseOptions
import com.google.firebase.messaging.FirebaseMessaging
import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage

/**
 * Push wake-ups from the hub (Firebase Cloud Messaging): another device
 * asks to see this phone (see `Wake`).
 *
 * Firebase is set up from the build's google-services.json values
 * (BuildConfig); without them push is off.
 */
object Push {
    val enabled get() = BuildConfig.FIREBASE_APP_ID.isNotEmpty()

    fun init(context: Context) {
        if (!enabled || FirebaseApp.getApps(context).isNotEmpty()) return
        FirebaseApp.initializeApp(
            context,
            FirebaseOptions.Builder()
                .setApplicationId(BuildConfig.FIREBASE_APP_ID)
                .setApiKey(BuildConfig.FIREBASE_API_KEY)
                .setProjectId(BuildConfig.FIREBASE_PROJECT_ID)
                .setGcmSenderId(BuildConfig.FIREBASE_SENDER_ID)
                .build(),
        )
    }

    /** Hand the current token to Rust, which tells the hub. */
    fun register(context: Context) {
        if (!enabled) return
        init(context)
        FirebaseMessaging.getInstance().token.addOnCompleteListener { task ->
            if (task.isSuccessful) {
                task.result?.let { OrielActivity.nativePushToken(it) }
            } else {
                Log.w("oriel", "no push token", task.exception)
            }
        }
    }
}

class OrielApp : Application() {
    override fun onCreate() {
        super.onCreate()
        Push.init(this)
    }
}

class PushService : FirebaseMessagingService() {
    override fun onNewToken(token: String) {
        // Rust only runs with the app open; it asks for the token then.
        Log.i("oriel", "new push token")
    }

    override fun onMessageReceived(message: RemoteMessage) {
        if (message.data["type"] != "wake") return
        Wake.request(this, message.data["from"].orEmpty().ifEmpty { "A device" })
    }
}
