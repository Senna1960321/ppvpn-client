package com.peakpassvpn.ppvpn

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.VpnService
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.ParcelFileDescriptor
import android.util.Log
import com.peakpassvpn.ppvpn.tunnel.TunnelHost
import java.io.File
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.RejectedExecutionException
import java.util.concurrent.TimeUnit
import org.json.JSONArray
import org.json.JSONObject

// The system VPN (#448), in its own process (:tunnel, ci-build.sh's
// manifest entry): started and stopped by VpnPlugin, its state broadcast
// within the package for the app's process.
//
// On start: the TUN as the tunnel host's settings say (docs/mobile.md), the
// network pushed to the host (the first one before it starts, as on iOS),
// then the host on the TUN's fd, serving tunnel.sock in filesDir, which the
// app's client connects to (crates/desktop tunnel.rs). "on" once it serves.
// The host's own sockets leave outside the tunnel through protect. A start
// from the system (always-on, no action) is not served yet.
//
// The host's calls block (stop waits for the engine): they and every change
// of state run on one worker thread, in order, so a stop right after a
// start finds it started and a start right after a stop starts again. A
// stop ends the service only if no start came after it (stopSelfResult).
//
// IPv4 and IPv6 both enter the TUN (ipv6_address, ipv6_routes; #511), so
// an app's IPv6 on a dual-stack network does not go around it. The
// network's addresses go to the host with its IPv6 ones: a global one
// tells the engine the network reaches the IPv6 internet.
class TunnelService : VpnService() {
    // Written on the worker only; read by publish from any thread.
    @Volatile private var state = STATE_OFF
    /** The last start's id, for a stop from the host or the system. */
    @Volatile private var lastStartId = 0
    private val worker = Executors.newSingleThreadExecutor()
    private var tun: ParcelFileDescriptor? = null
    private var network: NetworkWatch? = null
    private var push: PushFallback? = null

    private val query = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) = publish()
    }

    override fun onCreate() {
        super.onCreate()
        val filter = IntentFilter(ACTION_QUERY)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            registerReceiver(query, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            registerReceiver(query, filter)
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        lastStartId = startId
        when (intent?.action) {
            ACTION_START -> {
                val notification = PushNotifications.vpn(this)
                if (Build.VERSION.SDK_INT >= 34) startForeground(PushNotifications.VPN_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_SYSTEM_EXEMPTED)
                else startForeground(PushNotifications.VPN_ID, notification)
                run(::start)
            }
            ACTION_STOP -> stop(startId)
            else -> run { if (state == STATE_OFF) stopSelfResult(startId) }
        }
        return START_NOT_STICKY
    }

    /** Another VPN app took over, or the user revoked the permission. */
    override fun onRevoke() = stop(lastStartId)

    override fun onDestroy() {
        unregisterReceiver(query)
        // Destroyed without a stop first (by the system): the host still
        // goes down before the process may.
        run(::shutDown)
        worker.shutdown()
        worker.awaitTermination(SHUTDOWN_WAIT_S, TimeUnit.SECONDS)
        set(STATE_OFF)
        stopForeground(STOP_FOREGROUND_REMOVE)
        super.onDestroy()
    }

    /** On the worker; nothing once the service is destroyed (a late onExit). */
    private fun run(task: () -> Unit) {
        try {
            worker.execute(task)
        } catch (e: RejectedExecutionException) {
            Log.w(TAG, "the service is gone: $e")
        }
    }

    /** On the worker. */
    private fun start() {
        if (state != STATE_OFF) {
            publish()
            return
        }
        set(STATE_CONNECTING)
        val failure = try {
            open()
        } catch (e: Exception) {
            error(e.toString())
        }
        if (failure == null) {
            Log.i(TAG, "tunnel started")
            set(STATE_ON)
            push = PushFallback(this, ::run).also { it.start() }
            return
        }
        Log.e(TAG, "start: $failure")
        shutDown()
        set(STATE_OFF)
        stopSelfResult(lastStartId)
    }

    /** The TUN, the network, the host: null once the host serves, else its error as JSON. */
    private fun open(): String? {
        val settings = JSONObject(TunnelHost.settings() ?: return error("no tun settings"))
        val builder = Builder().setSession(SESSION).setMtu(settings.getInt("mtu"))
        cidr(settings.getString("ipv4_address")).let { (address, prefix) -> builder.addAddress(address, prefix) }
        for (route in settings.getJSONArray("ipv4_routes").strings()) {
            cidr(route).let { (address, prefix) -> builder.addRoute(address, prefix) }
        }
        settings.optString("ipv6_address").takeIf { it.isNotEmpty() }?.let {
            cidr(it).let { (address, prefix) -> builder.addAddress(address, prefix) }
        }
        for (route in settings.optJSONArray("ipv6_routes")?.strings().orEmpty()) {
            cidr(route).let { (address, prefix) -> builder.addRoute(address, prefix) }
        }
        settings.getJSONArray("dns_servers").strings().forEach { builder.addDnsServer(it) }
        val fd = builder.establish() ?: return error("the VPN is not allowed (establish)")
        tun = fd
        network = NetworkWatch(this).also { it.start() }
        val listener = object : TunnelHost.Listener {
            override fun protect(fd: Int): Boolean = this@TunnelService.protect(fd)

            override fun onExit(reason: String) {
                Log.e(TAG, "host exit: $reason")
                stop(lastStartId)
            }
        }
        return TunnelHost.start(
            fd.fd,
            File(filesDir, SOCKET_FILE).path,
            File(filesDir, STATE_DIR).path,
            null,
            listener,
        )
    }

    /** Ends the service unless a start newer than [startId] came. */
    private fun stop(startId: Int) = run {
        if (state != STATE_OFF) {
            set(STATE_DISCONNECTING)
            shutDown()
            set(STATE_OFF)
        }
        stopSelfResult(startId)
    }

    /**
     * On the worker: no more networks, the host (which blocks until done),
     * then the TUN. A network change the callback's thread is pushing in
     * the meantime only records the network for the next start: the host
     * is taken out before it stops.
     */
    private fun shutDown() {
        push?.close()
        push = null
        network?.stop()
        network = null
        TunnelHost.stop()
        tun?.close()
        tun = null
    }

    private fun set(next: String) {
        state = next
        publish()
    }

    private fun publish() {
        sendBroadcast(Intent(ACTION_STATE).setPackage(packageName).putExtra(EXTRA_STATE, state))
    }

    companion object {
        const val ACTION_START = "com.peakpassvpn.ppvpn.vpn.START"
        const val ACTION_STOP = "com.peakpassvpn.ppvpn.vpn.STOP"
        const val ACTION_STATE = "com.peakpassvpn.ppvpn.vpn.STATE"
        const val ACTION_QUERY = "com.peakpassvpn.ppvpn.vpn.QUERY"
        const val EXTRA_STATE = "state"
        const val STATE_OFF = "off"
        const val STATE_CONNECTING = "connecting"
        const val STATE_ON = "on"
        const val STATE_DISCONNECTING = "disconnecting"

        /** The VPN's name in the system's settings and notification. */
        private const val SESSION = "PPVPN"
        /** In filesDir, where the app's client connects (crates/desktop tunnel.rs SOCKET_FILE). */
        private const val SOCKET_FILE = "tunnel.sock"
        private const val STATE_DIR = "tunnel-state"
        private const val SHUTDOWN_WAIT_S = 10L

        fun intent(context: Context, action: String): Intent =
            Intent(context, TunnelService::class.java).setAction(action)

        /** An error as the host's JSON. */
        private fun error(message: String): String =
            JSONObject().put("code", "TUNNEL_INTERNAL").put("message", message).toString()

        /** "10.60.159.89/30" as its address and prefix length. */
        private fun cidr(text: String): Pair<String, Int> {
            val slash = text.indexOf('/')
            require(slash > 0) { "not a CIDR: $text" }
            return text.substring(0, slash) to text.substring(slash + 1).toInt()
        }

        private fun JSONArray.strings(): List<String> = (0 until length()).map(::getString)
    }
}

private const val TAG = "PPVPN"

/**
 * The network outside the tunnel, pushed to the host as the engine's
 * NetworkState JSON (docs/host-integration.md section 11): the best network
 * that is not a VPN, its interface, type, addresses and gateway; `{}` when
 * there is none. Each change once. [start] returns once the first is in
 * (offline if none comes within a moment), so the host starts with it.
 */
private class NetworkWatch(private val service: VpnService) {
    private val connectivity = service.getSystemService(ConnectivityManager::class.java)
    private val thread = HandlerThread("ppvpn-network").apply { start() }
    private val handler = Handler(thread.looper)
    private val first = CountDownLatch(1)

    // On the handler's thread.
    private var current: Network? = null
    private var capabilities: NetworkCapabilities? = null
    private var links: LinkProperties? = null
    private var sent: String? = null

    private val callback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            if (network != current) {
                current = network
                capabilities = connectivity.getNetworkCapabilities(network)
                links = connectivity.getLinkProperties(network)
            }
            push()
        }

        override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) {
            if (network != current) return
            capabilities = caps
            push()
        }

        override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) {
            if (network != current) return
            links = properties
            push()
        }

        override fun onLost(network: Network) {
            if (network != current) return
            current = null
            capabilities = null
            links = null
            push()
        }
    }

    fun start() {
        val request = NetworkRequest.Builder()
            .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_RESTRICTED)
            .addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
            .build()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            connectivity.registerBestMatchingNetworkCallback(request, callback, handler)
        } else {
            connectivity.requestNetwork(request, callback, handler)
        }
        if (!first.await(FIRST_WAIT_MS, TimeUnit.MILLISECONDS)) {
            handler.post { push() }
            first.await()
        }
    }

    fun stop() {
        try {
            connectivity.unregisterNetworkCallback(callback)
        } catch (e: IllegalArgumentException) {
            // Never registered.
        }
        thread.quitSafely()
    }

    private fun push() {
        val state = json()
        if (state != sent) {
            sent = state
            // The tunnel is metered as its network is.
            service.setUnderlyingNetworks(current?.let { arrayOf(it) })
            TunnelHost.networkChanged(state)?.let { Log.e(TAG, "network: $it") }
        }
        first.countDown()
    }

    private fun json(): String {
        val links = links ?: return "{}"
        val caps = capabilities ?: return "{}"
        val name = links.interfaceName ?: return "{}"
        val type = when {
            caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) -> "wifi"
            caps.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) -> "cellular"
            caps.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET) -> "ethernet"
            else -> "other"
        }
        val addresses = JSONArray()
        for (link in links.linkAddresses) {
            val host = link.address.hostAddress?.substringBefore('%') ?: continue
            addresses.put("$host/${link.prefixLength}")
        }
        val gateway = links.routes
            .firstOrNull { it.isDefaultRoute && it.gateway?.isAnyLocalAddress == false }
            ?.gateway?.hostAddress?.substringBefore('%')
        return JSONObject()
            .put("interface", name)
            .put("type", type)
            .put("addresses", addresses)
            .apply { if (gateway != null) put("gateway", gateway) }
            .put("expensive", !caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED))
            .put(
                "constrained",
                connectivity.restrictBackgroundStatus == ConnectivityManager.RESTRICT_BACKGROUND_STATUS_ENABLED,
            )
            .toString()
    }

    private companion object {
        const val FIRST_WAIT_MS = 2000L
    }
}
