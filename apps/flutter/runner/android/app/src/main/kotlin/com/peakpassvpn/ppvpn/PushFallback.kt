package com.peakpassvpn.ppvpn

import android.content.Context
import android.os.FileObserver
import java.io.File

/** Owned only by the running VPN foreground service. Calls are serialized
 * on its worker. FileObserver merely queues work there. */
class PushFallback(private val context: Context, private val enqueue: (() -> Unit) -> Unit, private val agent: PushAgentHost = JniPushAgent) {
    private val dataDir = File(context.filesDir, "data").apply { mkdirs() }
    private val registration = File(dataDir, "push-agent.json")
    private val lease = File(dataDir, "android-push-fallback")
    private var current: String? = null
    @Volatile private var closed = false
    @Suppress("DEPRECATION")
    private val observer = object : FileObserver(dataDir.path, CLOSE_WRITE or MOVED_TO or DELETE or MOVED_FROM) {
        override fun onEvent(event: Int, path: String?) {
            if (path == registration.name || path == lease.name) enqueue(::refresh)
        }
    }

    fun start() { observer.startWatching(); refresh() }

    internal fun refresh() {
        if (closed) return
        val next = if (lease.isFile) runCatching { registration.readText() }.getOrNull() else null
        if (next == current) return
        agent.stop()
        current = next
        if (next != null) agent.start(dataDir.path, object : NativePushAgent.Listener {
            override fun show(key: String, messageId: String, title: String, body: String): Boolean {
                // Sign-out / account switch / FCM recovery can happen while a
                // long poll is finishing. Never display its stale response.
                if (closed || !lease.isFile || runCatching { registration.readText() }.getOrNull() != next) return false
                return PushNotifications.show(context, key, messageId.ifEmpty { null }, title, body)
            }
        })
    }

    fun close() {
        closed = true
        observer.stopWatching()
        agent.stop()
        current = null
    }
}

object NativePushAgent {
    init { System.loadLibrary("ppvpn_flutter") }
    interface Listener { fun show(key: String, messageId: String, title: String, body: String): Boolean }
    @JvmStatic external fun start(dataDir: String, listener: Listener)
    @JvmStatic external fun stop()
}

interface PushAgentHost {
    fun start(dataDir: String, listener: NativePushAgent.Listener)
    fun stop()
}
private object JniPushAgent : PushAgentHost {
    override fun start(dataDir: String, listener: NativePushAgent.Listener) = NativePushAgent.start(dataDir, listener)
    override fun stop() = NativePushAgent.stop()
}
