package com.peakpassvpn.ppvpn

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.content.SharedPreferences
import android.content.pm.PackageManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import com.google.android.gms.common.ConnectionResult
import com.google.android.gms.common.GoogleApiAvailabilityLight
import com.google.firebase.FirebaseApp
import com.google.firebase.messaging.FirebaseMessaging
import io.flutter.plugin.common.BinaryMessenger
import io.flutter.plugin.common.MethodChannel
import org.json.JSONObject

/** The OS token and tap are buffered until Dart installs its handler. */
class PushPlugin(private val activity: Activity, messenger: BinaryMessenger,
    private val fetchToken: ((String?) -> Unit) -> Unit = { done -> firebaseToken(activity, done) }) :
    SharedPreferences.OnSharedPreferenceChangeListener {
    private val channel = MethodChannel(messenger, "com.peakpassvpn.ppvpn/push")
    private val prefs = activity.getSharedPreferences(PREFS, Activity.MODE_PRIVATE)
    private val handler = Handler(Looper.getMainLooper())
    private var ready = false
    private var closed = false
    private var requesting = false
    private var pending: Map<String, String>? = null
    private var attempt = 0
    private var timeout: Runnable? = null

    init {
        PushNotifications.channels(activity)
        prefs.registerOnSharedPreferenceChangeListener(this)
        channel.setMethodCallHandler { call, result ->
            when (call.method) {
                "start" -> {
                    ready = true
                    prefs.getString(TOKEN, null)?.let(::token)
                    pending?.let { channel.invokeMethod("opened", it) }
                    pending = null
                    refresh()
                    result.success(null)
                }
                "requestPermission" -> {
                    requestPermission()
                    result.success(null)
                }
                else -> result.notImplemented()
            }
        }
    }

    fun refresh() {
        if (!ready || closed || requesting) return
        requesting = true
        val request = ++attempt
        val timedOut = Runnable {
            if (request == attempt && !closed) { requesting = false; setFallback(true) }
        }
        timeout = timedOut
        handler.postDelayed(timedOut, 30_000)
        fetchToken { value ->
            if (closed || request != attempt) return@fetchToken
            requesting = false
            handler.removeCallbacks(timedOut)
            if (value.isNullOrBlank()) setFallback(true)
            else {
                prefs.edit().putString(TOKEN, value).apply()
                token(value)
                setFallback(false)
            }
        }
    }

    private fun setFallback(enabled: Boolean) {
        if (ready && !closed) channel.invokeMethod("fallback", mapOf("enabled" to enabled))
    }

    private fun token(value: String) {
        if (ready && !closed && value.isNotBlank()) {
            channel.invokeMethod("token", mapOf("token" to value, "bundle_id" to activity.packageName))
        }
    }

    override fun onSharedPreferenceChanged(preferences: SharedPreferences, key: String?) {
        if (key == TOKEN) preferences.getString(TOKEN, null)?.let {
            ++attempt
            requesting = false
            timeout?.let(handler::removeCallbacks)
            token(it)
            setFallback(false)
            PushTokenWorker.enqueue(activity)
        }
    }

    /** Both FCM's background launch extras and our own foreground notifications. */
    fun onIntent(intent: Intent?) {
        if (intent == null || (intent.action != PushNotifications.OPEN && !intent.hasExtra("google.message_id"))) return
        val extra = try { JSONObject(intent.getStringExtra("extra") ?: "{}") } catch (_: Exception) { JSONObject() }
        val id = intent.extras?.get("message_id")?.toString()
            ?: extra.opt("message_id")?.takeUnless { it == JSONObject.NULL }?.toString()
        val payload = id?.let { mapOf("message_id" to it) } ?: emptyMap()
        // Activity recreation must not replay the same click.
        listOf("message_id", "extra", "google.message_id").forEach(intent::removeExtra)
        if (intent.action == PushNotifications.OPEN) intent.action = null
        if (ready) channel.invokeMethod("opened", payload) else pending = payload
    }

    private fun requestPermission() {
        if (Build.VERSION.SDK_INT < 33 ||
            activity.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED ||
            prefs.getBoolean(ASKED, false)) return
        prefs.edit().putBoolean(ASKED, true).apply()
        activity.requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 0x5055)
    }

    fun close() {
        closed = true
        handler.removeCallbacksAndMessages(null)
        prefs.unregisterOnSharedPreferenceChangeListener(this)
        channel.setMethodCallHandler(null)
    }

    companion object {
        private fun firebaseToken(activity: Activity, done: (String?) -> Unit) {
            if (GoogleApiAvailabilityLight.getInstance().isGooglePlayServicesAvailable(activity) != ConnectionResult.SUCCESS ||
                FirebaseApp.initializeApp(activity) == null) { done(null); return }
            FirebaseMessaging.getInstance().token.addOnCompleteListener { task ->
                done(if (task.isSuccessful) task.result else null)
            }
        }
        const val PREFS = "ppvpn-push"
        const val TOKEN = "fcm-token"
        private const val ASKED = "notification-permission-asked"
    }
}
