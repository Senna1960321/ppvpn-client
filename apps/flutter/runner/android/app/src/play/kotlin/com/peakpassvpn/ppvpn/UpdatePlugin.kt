package com.peakpassvpn.ppvpn

import android.app.Activity
import io.flutter.plugin.common.BinaryMessenger
import io.flutter.plugin.common.MethodChannel

// Play builds contain no downloader, installer or install permission.
class UpdatePlugin(activity: Activity, messenger: BinaryMessenger) : AutoCloseable {
    private val channel = MethodChannel(messenger, "ppvpn/update")
    init {
        channel.setMethodCallHandler { call, result ->
            if (call.method == "state") result.success(mapOf("available" to false))
            else result.notImplemented()
        }
    }
    override fun close() { channel.setMethodCallHandler(null) }
}
