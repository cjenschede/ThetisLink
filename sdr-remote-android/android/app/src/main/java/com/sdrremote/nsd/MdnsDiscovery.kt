// SPDX-License-Identifier: GPL-2.0-or-later

package com.sdrremote.nsd

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.util.Log
import java.net.Inet4Address
import java.net.InetAddress
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow

/**
 * NSD-based mDNS discovery for ThetisLink servers on the local
 * network. Mirrors the desktop client's `mdns.rs` browse; same service
 * type, same display-label convention.
 *
 * Owns a `MulticastLock` and an `NsdManager.DiscoveryListener`. Lifecycle
 * is driven from `ConnectionPanel`: `start()` on `onResume`/composable
 * `LaunchedEffect`, `stop()` on `onPause`/composable `DisposableEffect`
 * cleanup. Battery-safe: when the user leaves the connect screen the
 * multicast lock is released and discovery stops.
 *
 * mDNS failure (permission denied, no Wi-Fi, AP-isolation) leaves the
 * `servers` flow empty: the manual IP field always stays usable.
 */
class MdnsDiscovery(private val context: Context) {

    private val nsdManager: NsdManager =
        context.getSystemService(Context.NSD_SERVICE) as NsdManager
    private val wifiManager: WifiManager =
        context.applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
    private val multicastLock = wifiManager.createMulticastLock("ThetisLink-mDNS").apply {
        setReferenceCounted(true)
    }
    private var discoveryListener: NsdManager.DiscoveryListener? = null

    private val _servers = MutableStateFlow<List<DiscoveredServer>>(emptyList())
    /** Snapshot list of servers currently seen on the network. */
    val servers: StateFlow<List<DiscoveredServer>> = _servers

    fun start() {
        if (discoveryListener != null) return
        try {
            multicastLock.acquire()
        } catch (e: SecurityException) {
            Log.w(TAG, "Multicast lock denied: ${e.message}")
            return
        }
        val listener = object : NsdManager.DiscoveryListener {
            override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {
                Log.w(TAG, "Discovery start failed: errorCode=$errorCode")
                releaseLockSafe()
            }
            override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) {
                Log.w(TAG, "Discovery stop failed: errorCode=$errorCode")
            }
            override fun onDiscoveryStarted(serviceType: String) {
                Log.i(TAG, "Discovery started: $serviceType")
            }
            override fun onDiscoveryStopped(serviceType: String) {
                Log.i(TAG, "Discovery stopped: $serviceType")
            }
            override fun onServiceFound(serviceInfo: NsdServiceInfo) {
                resolve(serviceInfo)
            }
            override fun onServiceLost(serviceInfo: NsdServiceInfo) {
                val name = serviceInfo.serviceName ?: return
                Log.d(TAG, "Service lost: $name")
                _servers.value = _servers.value.filter { it.instance != name }
            }
        }
        discoveryListener = listener
        nsdManager.discoverServices(
            SERVICE_TYPE,
            NsdManager.PROTOCOL_DNS_SD,
            listener,
        )
    }

    fun stop() {
        val listener = discoveryListener ?: return
        try {
            nsdManager.stopServiceDiscovery(listener)
        } catch (e: IllegalArgumentException) {
            // Already stopped: fine.
        }
        discoveryListener = null
        releaseLockSafe()
    }

    private fun releaseLockSafe() {
        try {
            if (multicastLock.isHeld) multicastLock.release()
        } catch (_: Throwable) {
        }
    }

    /**
     * NSD resolve fills in host + port + TXT records. Resolve callbacks
     * land on a binder thread; we marshall the result into the flow.
     */
    private fun resolve(info: NsdServiceInfo) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            // Android 14+: resolveService(NsdServiceInfo, Executor, ServiceInfoCallback)
            // is the recommended path but bringing in an Executor here is
            // overkill for the legacy single-shot resolve; the deprecated
            // call still works on 14.
        }
        @Suppress("DEPRECATION")
        nsdManager.resolveService(info, object : NsdManager.ResolveListener {
            override fun onResolveFailed(serviceInfo: NsdServiceInfo, errorCode: Int) {
                Log.d(TAG, "Resolve failed for ${serviceInfo.serviceName}: $errorCode")
            }
            override fun onServiceResolved(serviceInfo: NsdServiceInfo) {
                val host = preferIpv4(serviceInfo.host) ?: return
                val port = serviceInfo.port
                if (port <= 0) return
                val attrs = serviceInfo.attributes
                val friendlyName = attrs["name"]?.toString(Charsets.UTF_8)
                val version = attrs["version"]?.toString(Charsets.UTF_8)
                val server = DiscoveredServer(
                    instance = serviceInfo.serviceName ?: host.hostAddress.orEmpty(),
                    friendlyName = friendlyName,
                    addrPort = "${host.hostAddress}:$port",
                    version = version,
                )
                val current = _servers.value.toMutableList()
                val idx = current.indexOfFirst { it.instance == server.instance }
                if (idx >= 0) current[idx] = server else current.add(server)
                _servers.value = current.sortedBy { it.displayLabel() }
            }
        })
    }

    private fun preferIpv4(addr: InetAddress?): InetAddress? {
        if (addr == null) return null
        return if (addr is Inet4Address) addr else addr
    }

    companion object {
        private const val TAG = "ThetisLinkMdns"
        const val SERVICE_TYPE = "_thetislink._udp."
    }
}

data class DiscoveredServer(
    val instance: String,
    val friendlyName: String?,
    val addrPort: String,
    val version: String?,
) {
    fun displayLabel(): String {
        val name = friendlyName ?: instance
        return "$name ($addrPort)"
    }
}
