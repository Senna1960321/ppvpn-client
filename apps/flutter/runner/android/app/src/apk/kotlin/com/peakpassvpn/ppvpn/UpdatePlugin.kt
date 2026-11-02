package com.peakpassvpn.ppvpn

import android.app.Activity
import android.content.Intent
import android.content.pm.PackageInfo
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.provider.Settings
import androidx.core.content.FileProvider
import io.flutter.plugin.common.BinaryMessenger
import io.flutter.plugin.common.MethodChannel
import org.json.JSONObject
import java.io.File
import java.net.URL
import java.security.MessageDigest
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean
import javax.net.ssl.HttpsURLConnection

/** Website APK updates. The app's private cache owns all downloaded bytes;
 * only the verified candidate is shared with the system installer. */
class UpdatePlugin(private val activity: Activity, messenger: BinaryMessenger) : AutoCloseable {
    private val channel = MethodChannel(messenger, "ppvpn/update")
    private val worker = Executors.newSingleThreadExecutor()
    private val busy = AtomicBoolean(false)
    private val closed = AtomicBoolean(false)
    private var candidate: Candidate? = null // worker thread only
    private val directory = File(activity.cacheDir, "updates")
    private val feed = BuildConfig.PPVPN_UPDATE_FEED

    private data class Candidate(val version: String, val code: Long, val url: URL,
        val length: Long, val sha256: String)

    init {
        channel.setMethodCallHandler { call, result ->
            when (call.method) {
                "state" -> result.success(mapOf("available" to feed.isNotEmpty()))
                "check" -> run(result) {
                    val latest = check()
                    candidate = latest
                    latest?.let { mapOf("version" to it.version, "versionCode" to it.code) }
                }
                "install" -> {
                    if (feed.isEmpty()) result.error("unavailable", "No APK update feed", null)
                    else if (!activity.packageManager.canRequestPackageInstalls()) {
                        try {
                            activity.startActivity(Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
                                Uri.parse("package:${activity.packageName}")))
                            result.success("permission")
                        } catch (error: Exception) { result.error("permission", error.message, null) }
                    } else run(result) {
                        val latest = candidate ?: error("Check for an update first")
                        directory.mkdirs()
                        // No arbitrary paths or URLs arrive over the method channel.
                        directory.listFiles()?.forEach { it.delete() }
                        val file = File.createTempFile("update-", ".apk", directory)
                        try {
                            download(latest, file)
                            verify(latest, file)
                            file
                        } catch (error: Exception) { file.delete(); throw error }
                    }
                }
                else -> result.notImplemented()
            }
        }
    }

    private fun run(result: MethodChannel.Result, operation: () -> Any?) {
        if (closed.get() || !busy.compareAndSet(false, true)) {
            result.error("busy", "An update operation is already running", null)
            return
        }
        worker.execute {
            val outcome = runCatching(operation)
            activity.runOnUiThread {
                busy.set(false)
                if (closed.get()) return@runOnUiThread
                outcome.fold(onSuccess = { value ->
                    try {
                        if (value is File) {
                            val uri = FileProvider.getUriForFile(activity, "${activity.packageName}.updates", value)
                            activity.startActivity(Intent(Intent.ACTION_VIEW)
                                .setDataAndType(uri, "application/vnd.android.package-archive")
                                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION))
                            result.success("installer")
                        } else result.success(value)
                    } catch (error: Exception) { result.error("install", error.message, null) }
                }, onFailure = { result.error("update", it.message, null) })
            }
        }
    }

    private fun connection(url: URL): HttpsURLConnection {
        val origin = URL(feed)
        require(url.protocol == "https" && url.host == origin.host && url.port == origin.port
            && url.userInfo == null) { "Update URL must use the configured HTTPS origin" }
        return (url.openConnection() as HttpsURLConnection).apply {
            connectTimeout = 20_000
            readTimeout = 30_000
            instanceFollowRedirects = false
            setRequestProperty("Accept-Encoding", "identity")
            setRequestProperty("User-Agent", "PPVPN/${BuildConfig.VERSION_NAME} (android-apk)")
        }
    }

    private fun check(): Candidate? {
        require(feed.isNotEmpty()) { "No APK update feed" }
        val connection = connection(URL(feed))
        val json = try {
            require(connection.responseCode == 200) { "Update feed HTTP ${connection.responseCode}" }
            val bytes = connection.inputStream.use { input ->
                val out = java.io.ByteArrayOutputStream()
                val buffer = ByteArray(4096)
                val deadline = System.nanoTime() + 30_000_000_000L
                while (true) {
                    if (closed.get()) error("Updater closed")
                    require(System.nanoTime() < deadline) { "Update feed timed out" }
                    val count = input.read(buffer)
                    if (count < 0) break
                    require(out.size() + count <= 64 * 1024) { "Update feed too large" }
                    out.write(buffer, 0, count)
                }
                out.toByteArray()
            }
            JSONObject(String(bytes, Charsets.UTF_8))
        } finally { connection.disconnect() }
        require(json.getInt("schema") == 1 && json.getString("distribution") == "apk"
            && json.getString("package") == activity.packageName) { "Wrong APK update feed" }
        val next = Candidate(json.getString("version"), json.getLong("version_code"),
            URL(json.getString("url")), json.getLong("length"), json.getString("sha256"))
        require(next.version.matches(Regex("[0-9]+\\.[0-9]+\\.[0-9]+"))
            && next.code in 1..2_100_000_000L && next.length in 1..MAX_APK_BYTES
            && next.sha256.matches(Regex("[0-9a-f]{64}"))) { "Invalid APK metadata" }
        // Validate its URL even when no download is needed.
        connection(next.url).disconnect()
        return if (next.code > versionCode(installed())) next else null
    }

    private fun download(next: Candidate, file: File) {
        val connection = connection(next.url)
        try {
            require(connection.responseCode == 200) { "APK HTTP ${connection.responseCode}" }
            if (connection.contentLengthLong >= 0) {
                require(connection.contentLengthLong == next.length) { "APK length mismatch" }
            }
            val digest = MessageDigest.getInstance("SHA-256")
            var length = 0L
            val deadline = System.nanoTime() + 600_000_000_000L
            connection.inputStream.use { input -> file.outputStream().use { output ->
                val buffer = ByteArray(64 * 1024)
                while (true) {
                    if (closed.get()) error("Updater closed")
                    require(System.nanoTime() < deadline) { "APK download timed out" }
                    val count = input.read(buffer)
                    if (count < 0) break
                    length += count
                    require(length <= next.length) { "APK exceeds declared length" }
                    digest.update(buffer, 0, count)
                    output.write(buffer, 0, count)
                }
                output.fd.sync()
            } }
            require(length == next.length && hex(digest.digest()) == next.sha256) { "APK checksum mismatch" }
        } finally { connection.disconnect() }
    }

    @Suppress("DEPRECATION")
    private fun flags(): Int = if (Build.VERSION.SDK_INT >= 28) PackageManager.GET_SIGNING_CERTIFICATES
        else PackageManager.GET_SIGNATURES

    @Suppress("DEPRECATION")
    private fun installed(): PackageInfo = activity.packageManager.getPackageInfo(activity.packageName, flags())

    @Suppress("DEPRECATION")
    private fun versionCode(info: PackageInfo): Long = if (Build.VERSION.SDK_INT >= 28) info.longVersionCode
        else info.versionCode.toLong()

    @Suppress("DEPRECATION")
    private fun signers(info: PackageInfo): Set<String> {
        val signatures = if (Build.VERSION.SDK_INT >= 28) info.signingInfo?.apkContentsSigners else info.signatures
        return signatures?.map { hex(MessageDigest.getInstance("SHA-256").digest(it.toByteArray())) }?.toSet()
            ?: emptySet()
    }

    @Suppress("DEPRECATION")
    private fun verify(next: Candidate, file: File) {
        val archive = activity.packageManager.getPackageArchiveInfo(file.absolutePath, flags())
            ?: error("Invalid APK")
        val current = installed()
        require(archive.packageName == activity.packageName && archive.versionName == next.version
            && versionCode(archive) == next.code && next.code > versionCode(current)) { "APK identity or version mismatch" }
        val expected = signers(current)
        require(expected.isNotEmpty() && signers(archive) == expected) { "APK signer differs from this installation" }
        // Android's installer verifies the APK signature again before it
        // replaces this package. Key rotation needs an explicit migration.
    }

    override fun close() {
        closed.set(true)
        channel.setMethodCallHandler(null)
        worker.shutdownNow()
    }

    private companion object {
        const val MAX_APK_BYTES = 512L * 1024 * 1024
        fun hex(bytes: ByteArray) = bytes.joinToString("") { "%02x".format(it.toInt() and 255) }
    }
}
