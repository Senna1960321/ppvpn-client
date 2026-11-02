package com.peakpassvpn.ppvpn

import android.app.Activity
import android.content.Intent
import android.content.pm.ApplicationInfo
import android.content.pm.PackageInfo
import android.content.pm.PackageManager
import android.content.pm.Signature
import android.content.pm.SigningInfo
import android.os.Build
import android.os.Looper
import android.provider.Settings
import io.flutter.plugin.common.FlutterException
import java.io.ByteArrayInputStream
import java.io.File
import java.io.InputStream
import java.net.URL
import java.net.URLConnection
import java.net.URLStreamHandler
import java.security.MessageDigest
import java.security.cert.Certificate
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CountDownLatch
import java.util.concurrent.ExecutionException
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import javax.net.ssl.HttpsURLConnection
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import org.robolectric.shadow.api.Shadow

private object Network {
    data class Reply(val bytes: ByteArray, val status: Int = 200, val length: Long = bytes.size.toLong(),
                     val input: (() -> InputStream)? = null)
    val replies = ConcurrentHashMap<String, Reply>()
    val opened = java.util.concurrent.CopyOnWriteArrayList<Connection>()
    fun reset() {
        // Robolectric isolates each SDK in a classloader, while java.net.URL
        // is JVM-global. Replace the test transport between those sandboxes.
        URL::class.java.getDeclaredField("factory").apply { isAccessible = true }.set(null, null)
        val handlers = URL::class.java.getDeclaredField("handlers").apply { isAccessible = true }
            .get(null) as MutableMap<*, *>
        handlers.clear()
        URL.setURLStreamHandlerFactory { protocol ->
            if (protocol != "https") null else object : URLStreamHandler() {
                override fun openConnection(url: URL): URLConnection = Connection(url).also { opened.add(it) }
            }
        }
        replies.clear()
        opened.clear()
    }
    class Connection(url: URL) : HttpsURLConnection(url) {
        var disconnected = false
        private val reply get() = checkNotNull(replies[url.toString()]) { "Unexpected URL $url" }
        override fun connect() { connected = true }
        override fun disconnect() { disconnected = true }
        override fun usingProxy() = false
        override fun getResponseCode() = reply.status
        override fun getContentLengthLong() = reply.length
        override fun getInputStream(): InputStream = reply.input?.invoke() ?: ByteArrayInputStream(reply.bytes)
        override fun getCipherSuite() = "fixture"
        override fun getLocalCertificates(): Array<Certificate>? = null
        override fun getServerCertificates(): Array<Certificate> = emptyArray()
    }
}

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [26, 35])
class UpdatePluginTest {
    private lateinit var activity: Activity
    private lateinit var channel: ChannelHarness
    private lateinit var plugin: UpdatePlugin
    private val bytes = "fixture APK bytes".toByteArray()
    private val url = "https://updates.example.test/root\$literal/android/stable/0.4.1/101/new.apk"
    private val directory get() = File(activity.cacheDir, "updates")

    @Before fun setup() {
        Network.reset()
        // Robolectric gives each test a new cacheDir; FileProvider's process
        // cache must not keep the previous test's otherwise-correct root.
        val providerCache = androidx.core.content.FileProvider::class.java.getDeclaredField("sCache")
            .apply { isAccessible = true }.get(null) as MutableMap<*, *>
        providerCache.clear()
        activity = Robolectric.buildActivity(Activity::class.java).setup().get()
        channel = ChannelHarness()
        shadowOf(activity.packageManager).installPackage(info(100))
        shadowOf(activity.packageManager).setCanRequestPackageInstalls(true)
        plugin = UpdatePlugin(activity, channel)
    }
    @After fun teardown() { if (::plugin.isInitialized) plugin.close() }

    @Suppress("DEPRECATION")
    private fun info(code: Long, name: String = activity.packageName, version: String = "0.4.1",
                     signer: String = "01020304"): PackageInfo = PackageInfo().apply {
        packageName = name
        applicationInfo = ApplicationInfo(activity.applicationInfo).apply { packageName = name }
        val manifest = activity.packageManager.getPackageInfo(activity.packageName,
            PackageManager.GET_PERMISSIONS or PackageManager.GET_PROVIDERS or PackageManager.GET_META_DATA)
        requestedPermissions = manifest.requestedPermissions
        providers = manifest.providers
        versionName = version
        if (Build.VERSION.SDK_INT >= 28) {
            setLongVersionCode(code)
            signingInfo = Shadow.newInstanceOf(SigningInfo::class.java).also {
                shadowOf(it).setSignatures(arrayOf(Signature(signer)))
            }
        } else {
            versionCode = code.toInt()
            signatures = arrayOf(Signature(signer))
        }
    }

    private fun metadata(): JSONObject = JSONObject().put("schema", 1).put("distribution", "apk")
        .put("package", activity.packageName).put("version", "0.4.1").put("version_code", 101)
        .put("url", url).put("length", bytes.size).put("sha256", hex(bytes))

    private fun feed(json: JSONObject = metadata(), status: Int = 200) {
        Network.replies[BuildConfig.PPVPN_UPDATE_FEED] = Network.Reply(json.toString().toByteArray(), status)
    }
    private fun archive(archive: PackageInfo? = info(101), body: ByteArray = bytes, length: Long = body.size.toLong()) {
        Network.replies[url] = Network.Reply(body, length = length, input = {
            if (archive != null) {
                val file = checkNotNull(directory.listFiles()?.singleOrNull())
                shadowOf(activity.packageManager).setPackageArchiveInfo(file.absolutePath, archive)
            }
            ByteArrayInputStream(body)
        })
    }
    private fun check() = channel.await(channel.call("check"))
    private fun install() = channel.await(channel.call("install", mapOf("url" to "https://attacker.invalid/app.apk")))
    private fun refusal(operation: () -> Any?): FlutterException {
        try { operation(); fail("Expected update refusal") }
        catch (error: ExecutionException) { return error.cause as FlutterException }
        throw AssertionError("unreachable")
    }
    private fun hex(body: ByteArray) = MessageDigest.getInstance("SHA-256").digest(body)
        .joinToString("") { "%02x".format(it.toInt() and 255) }

    @Test fun generatedApkFlavorValidatesFeedOriginAndMetadataBoundaries() {
        assertEquals("apk", BuildConfig.FLAVOR)
        assertEquals("https://updates.example.test/root\$literal/android/stable/latest.json", BuildConfig.PPVPN_UPDATE_FEED)
        assertEquals(mapOf("available" to true), channel.await(channel.call("state")))
        val permissions = activity.packageManager.getPackageInfo(activity.packageName, PackageManager.GET_PERMISSIONS)
        assertTrue(permissions.requestedPermissions.orEmpty().contains("android.permission.REQUEST_INSTALL_PACKAGES"))
        feed()
        assertEquals(mapOf("version" to "0.4.1", "versionCode" to 101L), check())
        for (code in listOf(99, 100)) {
            feed(metadata().put("version_code", code))
            assertNull(check())
        }
        val malformed = listOf("schema" to 2, "distribution" to "play", "package" to "other.app",
            "version" to "garbage", "version_code" to 0, "version_code" to 2_100_000_001L,
            "length" to 0, "length" to 536_870_913L, "sha256" to "00",
            "url" to "http://updates.example.test/app.apk", "url" to "https://attacker.invalid/app.apk",
            "url" to "https://user@updates.example.test/app.apk", "url" to "https://updates.example.test:444/app.apk")
        for ((key, value) in malformed) {
            feed(metadata().put(key, value))
            assertEquals("update", refusal { check() }.code)
        }
        feed(status = 302)
        assertEquals("update", refusal { check() }.code)
        Network.replies[BuildConfig.PPVPN_UPDATE_FEED] = Network.Reply(ByteArray(65_537) { 32 })
        assertEquals("update", refusal { check() }.code)
        assertTrue(Network.opened.all { !it.instanceFollowRedirects && it.connectTimeout == 20_000
            && it.readTimeout == 30_000 && it.getRequestProperty("Accept-Encoding") == "identity" })
        assertTrue(Network.opened.all { it.disconnected })
    }

    @Test fun onlyVerifiedNewerSameSignerApkReachesTheInstaller() {
        feed()
        check()
        val bad = listOf<Pair<PackageInfo?, ByteArray>>(
            info(101, name = "other.app") to bytes, info(102) to bytes,
            info(101, version = "0.4.2") to bytes, info(101, signer = "aabbccdd") to bytes,
            null to bytes, info(101) to ByteArray(bytes.size) { 0 })
        for ((candidate, body) in bad) {
            archive(candidate, body)
            assertEquals("update", refusal { install() }.code)
            assertTrue(directory.listFiles().orEmpty().isEmpty())
            assertNull(shadowOf(activity).nextStartedActivity)
        }
        archive()
        assertEquals("installer", install())
        val intent = checkNotNull(shadowOf(activity).nextStartedActivity)
        assertEquals(Intent.ACTION_VIEW, intent.action)
        assertEquals("application/vnd.android.package-archive", intent.type)
        assertEquals("${activity.packageName}.updates", intent.data?.authority)
        assertEquals("content", intent.data?.scheme)
        assertTrue(intent.flags and Intent.FLAG_GRANT_READ_URI_PERMISSION != 0)
        assertArrayEquals(bytes, checkNotNull(directory.listFiles()?.singleOrNull()).readBytes())
        assertTrue(Network.opened.none { it.url.host == "attacker.invalid" })
    }

    @Test fun permissionAndDownloadFailuresRemainRetryable() {
        feed()
        check()
        shadowOf(activity.packageManager).setCanRequestPackageInstalls(false)
        assertEquals("permission", install())
        val permission = checkNotNull(shadowOf(activity).nextStartedActivity)
        assertEquals(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, permission.action)
        assertEquals("package:${activity.packageName}", permission.data.toString())
        assertFalse(directory.exists())
        shadowOf(activity.packageManager).setCanRequestPackageInstalls(true)
        for ((body, declared) in listOf(bytes to 999L, (bytes + byteArrayOf(0)) to -1L,
                                       bytes.dropLast(1).toByteArray() to -1L)) {
            archive(body = body, length = declared)
            assertEquals("update", refusal { install() }.code)
            assertTrue(directory.listFiles().orEmpty().isEmpty())
        }
        Network.replies[url] = Network.Reply(bytes, status = 403)
        assertEquals("update", refusal { install() }.code)
        archive(length = -1)
        assertEquals("installer", install())
    }

    @Test fun duplicateRequestsAndClosedActivityCannotLaunchInstaller() {
        assertEquals("update", refusal { install() }.code)
        feed()
        check()
        val entered = CountDownLatch(1)
        val release = CountDownLatch(1)
        Network.replies[url] = Network.Reply(bytes, input = {
            entered.countDown()
            release.await(5, TimeUnit.SECONDS)
            ByteArrayInputStream(bytes)
        })
        val pending = channel.call("install")
        assertTrue(entered.await(5, TimeUnit.SECONDS))
        assertEquals("busy", refusal { channel.await(channel.call("check")) }.code)
        plugin.close()
        release.countDown()
        assertNull(channel.handler)
        val workerField = plugin.javaClass.getDeclaredField("worker").apply { isAccessible = true }
        val worker = workerField.get(plugin) as java.util.concurrent.ExecutorService
        assertTrue(worker.awaitTermination(5, TimeUnit.SECONDS))
        shadowOf(Looper.getMainLooper()).idle()
        assertNull(shadowOf(activity).nextStartedActivity)
        assertTrue(directory.listFiles().orEmpty().isEmpty())
        assertFalse(pending.isDone)

        // A real network read need not honor Thread.interrupt. Once that
        // in-flight read returns, closing the Activity must stop feed reads
        // too, rather than retaining it for an arbitrarily slow response.
        channel = ChannelHarness()
        plugin = UpdatePlugin(activity, channel)
        val reading = CountDownLatch(1)
        val unblock = CountDownLatch(1)
        val reads = AtomicInteger()
        val body = ByteArrayInputStream(metadata().toString().toByteArray())
        Network.replies[BuildConfig.PPVPN_UPDATE_FEED] = Network.Reply(byteArrayOf(), input = {
            object : InputStream() {
                override fun read(): Int = body.read()
                override fun read(buffer: ByteArray, offset: Int, length: Int): Int {
                    if (reads.incrementAndGet() == 1) {
                        reading.countDown()
                        while (unblock.count > 0) {
                            try { unblock.await(5, TimeUnit.SECONDS) }
                            catch (_: InterruptedException) { /* like a socket read */ }
                        }
                    }
                    return body.read(buffer, offset, 1)
                }
            }
        })
        val checking = channel.call("check")
        assertTrue(reading.await(5, TimeUnit.SECONDS))
        plugin.close()
        unblock.countDown()
        val checkingWorker = workerField.get(plugin) as java.util.concurrent.ExecutorService
        assertTrue(checkingWorker.awaitTermination(5, TimeUnit.SECONDS))
        shadowOf(Looper.getMainLooper()).idle()
        assertEquals("Closed updater continued reading its feed", 1, reads.get())
        assertFalse(checking.isDone)
    }
}
