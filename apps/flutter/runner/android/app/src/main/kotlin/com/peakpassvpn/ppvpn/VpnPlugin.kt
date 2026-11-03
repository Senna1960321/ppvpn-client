package com.peakpassvpn.ppvpn

import android.app.Activity
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.net.VpnService
import android.os.Build
import io.flutter.plugin.common.BinaryMessenger
import io.flutter.plugin.common.EventChannel
import io.flutter.plugin.common.MethodChannel

// The system VPN for lib/vpn.dart (#448, the client's tunnel mode):
//
// - MethodChannel com.peakpassvpn.ppvpn/vpn: prepare() -> Boolean (true once
//   the user has allowed the VPN; the system asks the first time), start()
//   and stop() of TunnelService.
// - EventChannel com.peakpassvpn.ppvpn/vpn-state: TunnelService's state,
//   off / connecting / on / disconnecting. TunnelService runs in its own
//   process and broadcasts it within the package; a new listener gets off,
//   then the service's state if it is running.
class VpnPlugin(private val activity: Activity, messenger: BinaryMessenger) :
    EventChannel.StreamHandler {
    private val methods = MethodChannel(messenger, "com.peakpassvpn.ppvpn/vpn")
    private val events = EventChannel(messenger, "com.peakpassvpn.ppvpn/vpn-state")
    private var pending: MethodChannel.Result? = null
    private var receiver: BroadcastReceiver? = null

    init {
        methods.setMethodCallHandler { call, result ->
            when (call.method) {
                "prepare" -> prepare(result)
                "start" -> {
                    activity.startForegroundService(TunnelService.intent(activity, TunnelService.ACTION_START))
                    result.success(null)
                }
                "stop" -> {
                    activity.startService(TunnelService.intent(activity, TunnelService.ACTION_STOP))
                    result.success(null)
                }
                else -> result.notImplemented()
            }
        }
        events.setStreamHandler(this)
    }

    private fun prepare(result: MethodChannel.Result) {
        val consent = VpnService.prepare(activity)
        if (consent == null) {
            result.success(true)
            return
        }
        if (pending != null) {
            result.error("busy", "the VPN permission is already being asked for", null)
            return
        }
        pending = result
        @Suppress("DEPRECATION")
        activity.startActivityForResult(consent, REQUEST_PREPARE)
    }

    /** The system's VPN consent dialog closed; false: not ours. */
    fun onActivityResult(requestCode: Int, resultCode: Int): Boolean {
        if (requestCode != REQUEST_PREPARE) return false
        pending?.success(resultCode == Activity.RESULT_OK)
        pending = null
        return true
    }

    override fun onListen(arguments: Any?, sink: EventChannel.EventSink) {
        unregister()
        val next = object : BroadcastReceiver() {
            override fun onReceive(context: Context, intent: Intent) {
                intent.getStringExtra(TunnelService.EXTRA_STATE)?.let(sink::success)
            }
        }
        val filter = IntentFilter(TunnelService.ACTION_STATE)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            activity.registerReceiver(next, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            activity.registerReceiver(next, filter)
        }
        receiver = next
        sink.success(TunnelService.STATE_OFF)
        activity.sendBroadcast(Intent(TunnelService.ACTION_QUERY).setPackage(activity.packageName))
    }

    override fun onCancel(arguments: Any?) = unregister()

    private fun unregister() {
        receiver?.let(activity::unregisterReceiver)
        receiver = null
    }

    fun close() {
        unregister()
        methods.setMethodCallHandler(null)
        events.setStreamHandler(null)
        pending?.success(false)
        pending = null
    }

    private companion object {
        const val REQUEST_PREPARE = 0x5650
    }
}
