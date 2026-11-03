package com.peakpassvpn.ppvpn

import android.content.ActivityNotFoundException
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodChannel

// The app's one activity (#448), in the Play package (ci-build.sh). Its
// channel tells main.dart the app's own directory: the client's data and
// logs go under it, and the tunnel's socket lives there; and it opens links
// for lib/links.dart (#473), and closes the sign-in's Custom Tab once signed
// in (#535). The system VPN's channels are VpnPlugin's.
class MainActivity : FlutterActivity() {
    private var vpn: VpnPlugin? = null
    private var push: PushPlugin? = null

    // A Custom Tab opened by open() is over this activity, in its task.
    private var tabOpen = false

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        MethodChannel(flutterEngine.dartExecutor.binaryMessenger, "com.peakpassvpn.ppvpn/app")
            .setMethodCallHandler { call, result ->
                when (call.method) {
                    "filesDir" -> result.success(filesDir.absolutePath)
                    "openUrl" -> result.success(open(call.arguments as? String))
                    "closeBrowser" -> {
                        closeTab()
                        result.success(null)
                    }
                    else -> result.notImplemented()
                }
            }
        vpn = VpnPlugin(this, flutterEngine.dartExecutor.binaryMessenger)
        push = PushPlugin(this, flutterEngine.dartExecutor.binaryMessenger).also { it.onIntent(intent) }
    }

    // A Custom Tab where the browser offers them, else the browser: the
    // Custom Tabs protocol is this extra on the view intent, so no library.
    private fun open(url: String?): Boolean {
        val uri = url?.let(Uri::parse) ?: return false
        val intent = Intent(Intent.ACTION_VIEW, uri)
        if (uri.scheme == "http" || uri.scheme == "https") {
            intent.putExtras(Bundle().apply { putBinder(CUSTOM_TABS_SESSION, null) })
        }
        return try {
            startActivity(intent)
            tabOpen = uri.scheme == "http" || uri.scheme == "https"
            true
        } catch (e: ActivityNotFoundException) {
            false
        }
    }

    // The app cannot close a Custom Tab, but the tab runs in this task above
    // this activity: starting this activity again with CLEAR_TOP finishes
    // what is above it (onNewIntent, no new instance). Only while a tab is
    // up, so a sign-in finishing in the background does not bring the app
    // forward.
    private fun closeTab() {
        if (!tabOpen) return
        tabOpen = false
        startActivity(
            Intent(this, MainActivity::class.java)
                .addFlags(Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP),
        )
    }

    // Back here (the tab closed by the user, or covered by another app):
    // nothing of ours is above this activity any more.
    override fun onResume() {
        super.onResume()
        tabOpen = false
        push?.refresh()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        push?.onIntent(intent)
    }

    override fun cleanUpFlutterEngine(flutterEngine: FlutterEngine) {
        push?.close()
        push = null
        vpn?.close()
        vpn = null
        super.cleanUpFlutterEngine(flutterEngine)
    }

    private companion object {
        const val CUSTOM_TABS_SESSION = "android.support.customtabs.extra.SESSION"

        // The bridge, before Dart opens it by the same name (main.dart),
        // which then gets this copy: loaded here, its JNI_OnLoad finds the
        // app's classes (rust/src/android.rs, the saved sign-in's key).
        init {
            System.loadLibrary("ppvpn_flutter")
        }
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        if (vpn?.onActivityResult(requestCode, resultCode) != true) {
            super.onActivityResult(requestCode, resultCode, data)
        }
    }
}
