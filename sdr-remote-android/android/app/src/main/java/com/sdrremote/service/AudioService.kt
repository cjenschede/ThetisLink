// SPDX-License-Identifier: GPL-2.0-or-later

package com.sdrremote.service

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat

class AudioService : Service() {

    companion object {
        private const val CHANNEL_ID = "thetislink_audio"
        private const val NOTIFICATION_ID = 1

        fun start(context: Context) {
            val intent = Intent(context, AudioService::class.java)
            ContextCompat.startForegroundService(context, intent)
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, AudioService::class.java))
        }
    }

    override fun onCreate() {
        super.onCreate()
        val channel = NotificationChannel(
            CHANNEL_ID,
            "ThetisLink Audio",
            NotificationManager.IMPORTANCE_LOW
        ).apply {
            description = "Shows status when ThetisLink is connected"
        }
        getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val notification = NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle("ThetisLink")
            .setContentText("Connected")
            .setSmallIcon(android.R.drawable.ic_btn_speak_now)
            .setOngoing(true)
            .build()

        startForeground(NOTIFICATION_ID, notification)
        // NOT_STICKY: do not let Android auto-restart the service after it is killed.
        // The app starts it explicitly on connect; a zombie restart would put the
        // ongoing notification back (the "active call" on a carkit) with no connection.
        return START_NOT_STICKY
    }

    // User swiped the app out of Recents -> treat it as an explicit "off": drop the
    // ongoing notification and stop the service (the connection then closes as the
    // process is reaped; the server tolerates the abrupt disconnect, see the
    // recv_from ConnectionReset fix). This does NOT fire on plain backgrounding
    // (home button / screen off), so background audio keeps working there. Fixes the
    // report where a swiped-away app left an "active call" notification on the carkit.
    override fun onTaskRemoved(rootIntent: Intent?) {
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
        super.onTaskRemoved(rootIntent)
    }

    override fun onBind(intent: Intent?): IBinder? = null
}
