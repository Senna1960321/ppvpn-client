package com.peakpassvpn.ppvpn

import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage
import org.json.JSONObject

open class PushMessagingService : FirebaseMessagingService() {
    override fun onNewToken(token: String) {
        // onNewToken also runs with no activity/Flutter engine. Persist for
        // the next startup; a live PushPlugin observes the same preferences.
        getSharedPreferences(PushPlugin.PREFS, MODE_PRIVATE).edit().putString(PushPlugin.TOKEN, token).apply()
        // A live core may outlast its activity and preference listener. Only
        // this fresh event signals recovery, never an old WorkManager retry.
        onTokenRecovered()
        PushTokenWorker.enqueue(this)
    }

    protected open fun onTokenRecovered() { NativePushToken.recovered() }

    override fun onMessageReceived(message: RemoteMessage) {
        val data = message.data
        val extra = try { JSONObject(data["extra"] ?: "{}") } catch (_: Exception) { JSONObject() }
        val id = data["message_id"] ?: extra.opt("message_id")?.takeUnless { it == JSONObject.NULL }?.toString()
        PushNotifications.show(this, message.messageId ?: "message", id,
            message.notification?.title ?: data["title"] ?: "PPVPN",
            message.notification?.body ?: data["body"].orEmpty())
    }
}
