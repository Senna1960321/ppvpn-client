package com.peakpassvpn.ppvpn

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri

/** Shared by FCM (the app process) and the fallback agent (:tunnel). */
object PushNotifications {
    const val OPEN = "com.peakpassvpn.ppvpn.OPEN_MESSAGE"
    const val MESSAGES = "ppvpn-messages"
    const val VPN = "ppvpn-vpn"
    const val VPN_ID = 1

    fun channels(context: Context) {
        context.getSystemService(NotificationManager::class.java).createNotificationChannels(listOf(
            NotificationChannel(MESSAGES, "Messages", NotificationManager.IMPORTANCE_DEFAULT),
            NotificationChannel(VPN, "VPN connection", NotificationManager.IMPORTANCE_LOW),
        ))
    }

    fun show(context: Context, key: String, messageId: String?, title: String, body: String): Boolean {
        channels(context)
        val manager = context.getSystemService(NotificationManager::class.java)
        if (!manager.areNotificationsEnabled() || manager.getNotificationChannel(MESSAGES)?.importance == NotificationManager.IMPORTANCE_NONE) return false
        val tag = "ppvpn-message-${messageId ?: key}"
        val intent = Intent(context, MainActivity::class.java)
            .setAction(OPEN).setData(Uri.parse("ppvpn://notification/${Uri.encode(tag)}"))
            .addFlags(Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP)
        messageId?.let { intent.putExtra("message_id", it) }
        val click = PendingIntent.getActivity(context, 0, intent, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
        val notification = Notification.Builder(context, MESSAGES)
            .setSmallIcon(R.drawable.ic_notification).setContentTitle(title).setContentText(body)
            .setStyle(Notification.BigTextStyle().bigText(body)).setContentIntent(click)
            .setAutoCancel(true).setOnlyAlertOnce(true).build()
        return try { manager.notify(tag, 2, notification); true } catch (_: SecurityException) { false }
    }

    fun vpn(context: Context): Notification {
        channels(context)
        val click = PendingIntent.getActivity(context, 0, Intent(context, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE)
        return Notification.Builder(context, VPN).setSmallIcon(R.drawable.ic_notification)
            .setContentTitle("PPVPN").setContentText("VPN is running")
            .setContentIntent(click).setOngoing(true).setCategory(Notification.CATEGORY_SERVICE).build()
    }
}
