package com.peakpassvpn.ppvpn

import android.app.Activity
import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.os.Looper
import androidx.work.Configuration
import androidx.work.ListenableWorker
import androidx.work.WorkerFactory
import androidx.work.WorkerParameters
import androidx.work.WorkManager
import androidx.work.testing.TestListenableWorkerBuilder
import androidx.work.testing.WorkManagerTestInitHelper
import io.flutter.plugin.common.BinaryMessenger
import io.flutter.plugin.common.MethodCall
import io.flutter.plugin.common.StandardMethodCodec
import java.io.File
import java.nio.ByteBuffer
import java.time.Duration
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config

class RecordingPushService : PushMessagingService() {
    var recoveries = 0
    override fun onTokenRecovered() { recoveries++ }
}

class PushChannel : BinaryMessenger {
    val events = mutableListOf<MethodCall>()
    private val handlers = mutableMapOf<String, BinaryMessenger.BinaryMessageHandler>()
    override fun send(name: String, message: ByteBuffer?) = send(name, message, null)
    override fun send(name: String, message: ByteBuffer?, callback: BinaryMessenger.BinaryReply?) {
        if (name == "com.peakpassvpn.ppvpn/push" && message != null) {
            events.add(StandardMethodCodec.INSTANCE.decodeMethodCall(message.duplicate().apply { flip() }))
        }
        callback?.reply(null)
    }
    override fun setMessageHandler(name: String, handler: BinaryMessenger.BinaryMessageHandler?) {
        if (handler == null) handlers.remove(name) else handlers[name] = handler
    }
    fun call(name: String) {
        val bytes = StandardMethodCodec.INSTANCE.encodeMethodCall(MethodCall(name, null)).apply { flip() }
        checkNotNull(handlers["com.peakpassvpn.ppvpn/push"]).onMessage(bytes) { reply ->
            if (reply != null) StandardMethodCodec.INSTANCE.decodeEnvelope(reply.apply { flip() })
        }
    }
    fun values(method: String) = events.filter { it.method == method }.map { it.arguments as Map<*, *> }
}

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [26, 35])
class PushTest {
    private lateinit var activity: Activity
    private lateinit var channel: PushChannel
    private var plugin: PushPlugin? = null
    private val prefs get() = activity.getSharedPreferences(PushPlugin.PREFS, Context.MODE_PRIVATE)

    @Before fun setup() {
        activity = Robolectric.buildActivity(Activity::class.java).setup().get()
        prefs.edit().clear().commit()
        channel = PushChannel()
        // Jobs are recorded, but never run native registration in the test VM.
        WorkManagerTestInitHelper.initializeTestWorkManager(activity, Configuration.Builder().build())
    }
    @After fun cleanup() { plugin?.close() }
    private fun host(fetch: ((String?) -> Unit) -> Unit = {}) = PushPlugin(activity, channel, fetch).also { plugin = it }

    @Test fun tokenRotationWithoutActivityQueuesEveryUpdate() {
        val service = Robolectric.buildService(RecordingPushService::class.java).create().get()
        val manager = WorkManager.getInstance(service)
        service.onNewToken("first")
        val first = manager.getWorkInfosForUniqueWork("ppvpn-fcm-registration").get()
        assertEquals(1, first.size)
        service.onNewToken("rotated")
        val second = manager.getWorkInfosForUniqueWork("ppvpn-fcm-registration").get()
        assertEquals(2, second.size)
        assertTrue(second.any { it.id == first.single().id })
        assertEquals("rotated", prefs.getString(PushPlugin.TOKEN, null))
        assertEquals(2, service.recoveries)
    }

    @Test fun coldStartTapAndPersistedTokenWaitForDart() {
        prefs.edit().putString(PushPlugin.TOKEN, "cached-token").commit()
        val host = host()
        val click = Intent().setAction(PushNotifications.OPEN)
            .putExtra("extra", """{"message_id":42,"deep_link":"file:///ignored"}""")
        host.onIntent(click)
        assertTrue(channel.events.isEmpty())
        channel.call("start")
        assertEquals("cached-token", channel.values("token").single()["token"])
        assertEquals(mapOf("message_id" to "42"), channel.values("opened").single())
        host.onIntent(click)
        assertEquals(1, channel.values("opened").size)
    }

    @Test fun actualMissingGooglePlayServicesSelectsFallback() {
        plugin = PushPlugin(activity, channel)
        channel.call("start")
        assertEquals(true, channel.values("fallback").last()["enabled"])
    }

    @Test fun timeoutAndTokenRecoveryChangeChannelsWithoutReplayingStaleFailures() {
        val callbacks = mutableListOf<(String?) -> Unit>()
        val host = host { callbacks.add(it) }
        channel.call("start")
        shadowOf(Looper.getMainLooper()).idleFor(Duration.ofSeconds(31))
        assertEquals(true, channel.values("fallback").last()["enabled"])
        host.refresh()
        callbacks[1]("recovered-token")
        shadowOf(Looper.getMainLooper()).idle()
        callbacks[0](null)
        assertEquals(false, channel.values("fallback").last()["enabled"])
        assertEquals("recovered-token", channel.values("token").last()["token"])
    }

    @Test fun backgroundFcmClickIsParsedAndConsumedOnce() {
        val host = host()
        channel.call("start")
        val intent = Intent(Intent.ACTION_MAIN).putExtra("google.message_id", "fcm-id")
            .putExtra("message_id", "99").putExtra("deep_link", "https://ignored.test")
        host.onIntent(intent)
        host.onIntent(intent)
        assertEquals(listOf(mapOf("message_id" to "99")), channel.values("opened"))
    }

    @Test fun notificationsUseDistinctImmutablePendingIntentsAndRespectDisabledChannel() {
        val manager = activity.getSystemService(NotificationManager::class.java)
        assertTrue(PushNotifications.show(activity, "a", "41", "title", "body"))
        assertTrue(PushNotifications.show(activity, "b", "42", "title", "body"))
        val notifications = shadowOf(manager).allNotifications
        assertEquals(2, notifications.size)
        val intents = notifications.map { shadowOf(it.contentIntent).savedIntent }
        assertEquals(setOf("41", "42"), intents.map { it.getStringExtra("message_id") }.toSet())
        assertTrue(intents.all { it.component?.className == MainActivity::class.java.name && it.action == PushNotifications.OPEN })
        assertNotEquals(intents[0].data, intents[1].data)
        shadowOf(manager).setNotificationsEnabled(false)
        assertFalse(PushNotifications.show(activity, "c", "43", "title", "body"))
    }

    @Test fun fallbackUsesCoreDataDirectoryAndRejectsOldLeaseOrStoppedService() {
        class Agent : PushAgentHost {
            var starts = 0
            var stops = 0
            var directory: String? = null
            lateinit var listener: NativePushAgent.Listener
            override fun start(dataDir: String, listener: NativePushAgent.Listener) {
                starts++; directory = dataDir; this.listener = listener
            }
            override fun stop() { stops++ }
        }
        val agent = Agent()
        val fallback = PushFallback(activity, { it() }, agent)
        val data = File(activity.filesDir, "data").apply { mkdirs() }
        val registration = File(data, "push-agent.json")
        val lease = File(data, "android-push-fallback")
        fallback.start()
        assertEquals(0, agent.starts)
        registration.writeText("account-a")
        lease.writeText("enabled")
        fallback.refresh()
        assertEquals(data.path, agent.directory)
        assertEquals(1, agent.starts)
        val old = agent.listener
        registration.writeText("account-b")
        assertFalse(old.show("1", "41", "old", "old"))
        fallback.refresh()
        assertEquals(2, agent.starts)
        lease.delete()
        assertFalse(agent.listener.show("2", "42", "old", "old"))
        fallback.refresh()
        lease.writeText("enabled")
        fallback.refresh()
        val latest = agent.listener
        fallback.close()
        assertFalse(latest.show("3", "43", "stopped", "stopped"))
        fallback.refresh()
        assertEquals(3, agent.starts)
    }

    @Test fun tokenWorkerNeedsNoActivityAndRetriesRotationDuringRegistration() {
        prefs.edit().putString(PushPlugin.TOKEN, "first").commit()
        val factory = object : WorkerFactory() {
            override fun createWorker(context: Context, name: String, params: WorkerParameters): ListenableWorker =
                PushTokenWorker(context, params) { dir, token, bundle ->
                    assertEquals(activity.filesDir.path, dir)
                    assertEquals(activity.packageName, bundle)
                    assertEquals("first", token)
                    prefs.edit().putString(PushPlugin.TOKEN, "rotated").commit()
                    true
                }
        }
        val worker = TestListenableWorkerBuilder<PushTokenWorker>(activity).setWorkerFactory(factory).build()
        assertEquals(ListenableWorker.Result.retry(), worker.doWork())
    }
}
