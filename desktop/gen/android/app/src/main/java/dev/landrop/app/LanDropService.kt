package dev.landrop.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat

/**
 * Keeps the process alive while LanDrop runs in the background, so the node can
 * keep receiving offers and files with the screen off or the app minimized.
 *
 * The Rust node itself runs inside the activity's process; this service only
 * raises the process priority with a quiet, ongoing notification.
 */
class LanDropService : Service() {
  override fun onCreate() {
    super.onCreate()
    createChannel()
    val notification = buildNotification()
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
      startForeground(
        NOTIFICATION_ID,
        notification,
        ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC,
      )
    } else {
      startForeground(NOTIFICATION_ID, notification)
    }
  }

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int = START_STICKY

  override fun onBind(intent: Intent?): IBinder? = null

  private fun createChannel() {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
    val manager = getSystemService(NotificationManager::class.java) ?: return
    val channel =
      NotificationChannel(
        CHANNEL_ID,
        "LanDrop Background Service",
        NotificationManager.IMPORTANCE_LOW,
      ).apply {
        description = "Keeps LanDrop reachable on the local network"
        setShowBadge(false)
      }
    manager.createNotificationChannel(channel)
  }

  private fun buildNotification(): Notification =
    NotificationCompat.Builder(this, CHANNEL_ID)
      .setContentTitle("LanDrop — Ready on Wi-Fi")
      .setContentText("Receiving transfers in the background")
      .setSmallIcon(R.mipmap.ic_launcher)
      .setOngoing(true)
      .setSilent(true)
      .setPriority(NotificationCompat.PRIORITY_LOW)
      .setCategory(NotificationCompat.CATEGORY_SERVICE)
      .build()

  companion object {
    private const val CHANNEL_ID = "landrop_bg"
    private const val NOTIFICATION_ID = 4242
  }
}
