// SPDX-License-Identifier: GPL-2.0-or-later

package com.sdrremote.measure

import android.Manifest
import android.annotation.SuppressLint
import android.content.Context
import android.content.pm.PackageManager
import android.location.Location
import android.location.LocationListener
import android.location.LocationManager
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.telephony.CellIdentityNr
import android.telephony.CellInfo
import android.telephony.CellInfoGsm
import android.telephony.CellInfoLte
import android.telephony.CellInfoNr
import android.telephony.CellInfoWcdma
import android.telephony.CellSignalStrengthGsm
import android.telephony.CellSignalStrengthLte
import android.telephony.CellSignalStrengthNr
import android.telephony.CellSignalStrengthWcdma
import android.telephony.TelephonyManager
import android.util.Log
import androidx.core.content.ContextCompat
import uniffi.sdr_remote.SdrBridge
import uniffi.sdr_remote.logLine
import kotlin.math.roundToInt

/**
 * The phone's side of the measurement file (build 181): about once a second,
 * the serving cell, its signal (or the wifi RSSI) and the GPS position, handed
 * to the engine, which writes them into the same file on the same clock.
 *
 * Only while the app is on screen: with the screen off Android stops giving
 * location and cell readings to an app without a location foreground service.
 * The rows then stop and start again - a gap in these rows is not a gap in the
 * link.
 *
 * Cell identity and GPS need the precise location permission; the signal does
 * not. Without the permission the measurement runs without `cell` and `gps`.
 * Facts only: a handover is a different cell id in the next row.
 */
class NetworkMeter(private val context: Context) {

    private companion object {
        const val TAG = "NetworkMeter"
        const val PERIOD_MS = 1000L
        /** Android's "not given" (CellInfo.UNAVAILABLE); the file leaves it empty. */
        const val U = Int.MAX_VALUE
    }

    private val handler = Handler(Looper.getMainLooper())
    /** Read and written on the main thread only. */
    private var sink: SdrBridge? = null
    /** The last state `follow` saw; the poll thread's own. */
    @Volatile private var followed = false
    /**
     * Set by `close` when the app goes; main thread. A poll round that was
     * already running can still post a start after that, and that start must
     * not happen (review of 182).
     */
    private var closed = false
    private var located = false
    private var gps: LocationListener? = null

    private val tick = object : Runnable {
        override fun run() {
            val bridge = sink ?: return
            runCatching { sample(bridge) }.onFailure { Log.w(TAG, "a reading failed", it) }
            handler.postDelayed(this, PERIOD_MS)
        }
    }

    /**
     * Runs while the engine writes a measurement file, and only then (build 182):
     * the file's name in the state is the one truth. A measurement the engine
     * ends by itself - it could not start, or a write failed - stops the
     * readings too, and with them the GPS (review of 181). Called from the
     * poll, on any thread; the work happens on the main thread.
     */
    fun follow(measuring: Boolean, bridge: SdrBridge) {
        if (measuring == followed) return
        followed = measuring
        handler.post {
            if (closed) return@post
            if (measuring && sink == null) start(bridge) else if (!measuring && sink != null) stop()
        }
    }

    /** For good: the app goes. Stops reading and ignores a start still on its way. Main thread. */
    fun close() {
        closed = true
        stop()
    }

    /** Starts reading; a second start begins clean (working rule 9). */
    private fun start(bridge: SdrBridge) {
        stop(quiet = true)
        sink = bridge
        located = ContextCompat.checkSelfPermission(context, Manifest.permission.ACCESS_FINE_LOCATION) ==
            PackageManager.PERMISSION_GRANTED
        if (located) startGps(bridge)
        // The Android version says which unit LTE's SINR column has (tenths of
        // a dB on Android 10, dB from 11), and location switched off in the
        // quick settings gives no cell and GPS rows although permitted (review
        // of 181). Named in advance (working rule 11, builds 181 and 182).
        val android = "Android ${Build.VERSION.SDK_INT}"
        val servicesOff = Build.VERSION.SDK_INT >= Build.VERSION_CODES.P &&
            context.getSystemService(LocationManager::class.java)?.isLocationEnabled == false
        note(
            if (located && servicesOff) "Measurement: cell and GPS on, but location is switched off ($android)"
            else if (located) "Measurement: cell and GPS on ($android)"
            else "Measurement: no precise location permission - measuring without cell and GPS ($android)"
        )
        handler.post(tick)
    }

    /** Stops reading. Harmless when not running. Main thread. */
    fun stop(quiet: Boolean = false) {
        val wasRunning = sink != null
        handler.removeCallbacks(tick)
        gps?.let { runCatching { context.getSystemService(LocationManager::class.java)?.removeUpdates(it) } }
        gps = null
        sink = null
        if (wasRunning && !quiet) note("Measurement: cell and GPS off")
    }

    private fun note(text: String) {
        runCatching { logLine(text) }
    }

    @SuppressLint("MissingPermission") // checked in start()
    private fun startGps(bridge: SdrBridge) {
        val lm = context.getSystemService(LocationManager::class.java) ?: return
        val listener = object : LocationListener {
            override fun onLocationChanged(loc: Location) {
                if (sink == null) return
                runCatching {
                    bridge.measureGps(
                        (loc.latitude * 1e6).roundToInt(),
                        (loc.longitude * 1e6).roundToInt(),
                        if (loc.hasSpeed()) (loc.speed * 3.6f).roundToInt() else U,
                        if (loc.hasAccuracy()) loc.accuracy.roundToInt() else U,
                    )
                }
            }
            // Implemented, not left to the interface: before Android 11 these
            // have no default body.
            override fun onProviderEnabled(provider: String) {}
            override fun onProviderDisabled(provider: String) {}
            @Deprecated("Deprecated in Java")
            override fun onStatusChanged(provider: String?, status: Int, extras: Bundle?) {}
        }
        try {
            lm.requestLocationUpdates(LocationManager.GPS_PROVIDER, PERIOD_MS, 0f, listener, Looper.getMainLooper())
            gps = listener
        } catch (e: Exception) {
            Log.w(TAG, "no GPS updates", e)
            note("Measurement: GPS not available (${e.javaClass.simpleName})")
        }
    }

    private fun sample(bridge: SdrBridge) {
        val tm = context.getSystemService(TelephonyManager::class.java)
        if (tm != null) {
            signal(bridge, tm)
            if (located) cells(bridge, tm)
        }
        wifi(bridge)
    }

    /**
     * Every signal the phone reports, the 5G of a 4G anchor included. From
     * Android 10: `getCellSignalStrengths` does not exist on 9, and calling it
     * there threw before the cells were read (review of 181).
     */
    private fun signal(bridge: SdrBridge, tm: TelephonyManager) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return
        val strengths = tm.signalStrength?.cellSignalStrengths ?: return
        for (s in strengths) {
            when {
                s is CellSignalStrengthLte -> bridge.measureSignal("lte", s.rsrp, s.rsrq, s.rssnr, s.level)
                Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q && s is CellSignalStrengthNr ->
                    bridge.measureSignal("nr", s.ssRsrp, s.ssRsrq, s.ssSinr, s.level)
                s is CellSignalStrengthWcdma -> bridge.measureSignal("wcdma", s.dbm, U, U, s.level)
                s is CellSignalStrengthGsm -> bridge.measureSignal("gsm", s.dbm, U, U, s.level)
            }
        }
    }

    /** The wifi RSSI, when wifi carries the traffic. */
    private fun wifi(bridge: SdrBridge) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return
        val cm = context.getSystemService(ConnectivityManager::class.java) ?: return
        val caps = cm.getNetworkCapabilities(cm.activeNetwork) ?: return
        if (!caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)) return
        val rssi = caps.signalStrength
        bridge.measureSignal("wifi", if (rssi == NetworkCapabilities.SIGNAL_STRENGTH_UNSPECIFIED) U else rssi, U, U, U)
    }

    @SuppressLint("MissingPermission") // only called when `located`
    private fun cells(bridge: SdrBridge, tm: TelephonyManager) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            // A fresh reading; `allCellInfo` alone can be a stale one.
            tm.requestCellInfoUpdate(context.mainExecutor, object : TelephonyManager.CellInfoCallback() {
                override fun onCellInfo(cellInfo: MutableList<CellInfo>) {
                    if (sink != null) report(bridge, cellInfo)
                }
            })
        } else {
            tm.allCellInfo?.let { report(bridge, it) }
        }
    }

    private fun report(bridge: SdrBridge, cells: List<CellInfo>) {
        for (c in cells) {
            if (!c.isRegistered) {
                // With 5G beside 4G the 4G cell is the registered one; the 5G
                // cell, if Android lists it, is not. Its own line, with how it
                // is used, so it does not read as the cell the phone is on
                // (build 210).
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q && c is CellInfoNr) {
                    runCatching {
                        (c.cellIdentity as CellIdentityNr).let {
                            bridge.measureNrCell(c.cellConnectionStatus, it.nci, it.pci, it.nrarfcn, it.tac)
                        }
                    }
                }
                continue
            }
            runCatching {
                when {
                    c is CellInfoLte -> c.cellIdentity.let {
                        bridge.measureCell("lte", long(it.ci), it.pci, it.earfcn, it.tac)
                    }
                    Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q && c is CellInfoNr ->
                        (c.cellIdentity as CellIdentityNr).let {
                            bridge.measureCell("nr", it.nci, it.pci, it.nrarfcn, it.tac)
                        }
                    c is CellInfoWcdma -> c.cellIdentity.let {
                        bridge.measureCell("wcdma", long(it.cid), it.psc, it.uarfcn, it.lac)
                    }
                    c is CellInfoGsm -> c.cellIdentity.let {
                        bridge.measureCell("gsm", long(it.cid), it.bsic, it.arfcn, it.lac)
                    }
                }
            }
        }
    }

    /** A 32-bit cell id as the file's 64-bit one, "not given" kept. */
    private fun long(v: Int): Long = if (v == U) Long.MAX_VALUE else v.toLong()
}
