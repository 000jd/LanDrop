package dev.landrop.app

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Bundle
import android.provider.OpenableColumns
import androidx.activity.enableEdgeToEdge
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import java.util.concurrent.Executors

class MainActivity : TauriActivity() {
  private val shareExecutor = Executors.newSingleThreadExecutor()
  private var multicastLock: WifiManager.MulticastLock? = null

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    startBackgroundService()
    handleShare(intent)
  }

  /// A foreground service keeps the node reachable with the screen off. Some
  /// OEM ROMs restrict background starts; that must never crash the activity.
  private fun startBackgroundService() {
    runCatching {
      val intent = Intent(this, LanDropService::class.java)
      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
        startForegroundService(intent)
      } else {
        startService(intent)
      }
    }
  }

  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    setIntent(intent)
    handleShare(intent)
  }

  override fun onResume() {
    super.onResume()
    acquireMulticastLock()
  }

  override fun onPause() {
    releaseMulticastLock()
    super.onPause()
  }

  override fun onDestroy() {
    runCatching { stopService(Intent(this, LanDropService::class.java)) }
    shareExecutor.shutdown()
    super.onDestroy()
  }

  // mDNS sockets only receive multicast while a MulticastLock is held.
  private fun acquireMulticastLock() {
    if (multicastLock?.isHeld == true) return
    val wifi =
      applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager ?: return
    runCatching {
      wifi.createMulticastLock("landrop-mdns").apply {
        setReferenceCounted(false)
        acquire()
      }
    }.onSuccess { multicastLock = it }
  }

  private fun releaseMulticastLock() {
    val lock = multicastLock ?: return
    runCatching {
      if (lock.isHeld) lock.release()
    }
    multicastLock = null
  }

  // ---------------------------------------------------------------------------
  // System share sheet -> private cache -> local backend inbox
  // ---------------------------------------------------------------------------

  private fun handleShare(intent: Intent?) {
    if (intent == null) return
    val action = intent.action ?: return
    if (action != Intent.ACTION_SEND && action != Intent.ACTION_SEND_MULTIPLE) return

    val uris = mutableListOf<Uri>()
    intent.clipData?.let { clip ->
      for (i in 0 until clip.itemCount) {
        clip.getItemAt(i).uri?.let { uris.add(it) }
      }
    }
    if (action == Intent.ACTION_SEND) {
      streamUri(intent)?.let { uri -> if (!uris.contains(uri)) uris.add(uri) }
    } else {
      for (uri in streamUris(intent)) {
        if (!uris.contains(uri)) uris.add(uri)
      }
    }
    val text = intent.getCharSequenceExtra(Intent.EXTRA_TEXT)?.toString()
    if (uris.isEmpty() && text.isNullOrBlank()) return

    // Consume the payload so a relaunch with the same intent cannot queue it twice.
    intent.removeExtra(Intent.EXTRA_STREAM)
    intent.removeExtra(Intent.EXTRA_TEXT)
    intent.clipData = null

    shareExecutor.execute { deliverShare(uris, text) }
  }

  @Suppress("DEPRECATION")
  private fun streamUri(intent: Intent): Uri? =
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
      intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
    } else {
      intent.getParcelableExtra(Intent.EXTRA_STREAM)
    }

  @Suppress("DEPRECATION")
  private fun streamUris(intent: Intent): List<Uri> =
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
      intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri::class.java) ?: emptyList()
    } else {
      intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM) ?: emptyList()
    }

  private fun displayName(uri: Uri): String? {
    if (uri.scheme == "content") {
      runCatching {
        contentResolver
          .query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
          ?.use { cursor ->
            if (cursor.moveToFirst()) {
              val index = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
              if (index >= 0) return cursor.getString(index)
            }
          }
      }
    }
    return uri.lastPathSegment?.substringAfterLast('/')
  }

  private fun copySharedFiles(uris: List<Uri>): List<String> {
    if (uris.isEmpty()) return emptyList()
    val dir = File(cacheDir, "shared").apply { mkdirs() }
    val out = mutableListOf<String>()
    for ((index, uri) in uris.withIndex()) {
      val raw = displayName(uri) ?: "shared-$index"
      val name =
        raw
          .replace('/', '_')
          .replace('\\', '_')
          .replace('\u0000', '_')
          .ifBlank { "shared-$index" }
      var target = File(dir, name)
      var suffix = 1
      while (target.exists()) {
        target = File(dir, "$suffix-$name")
        suffix += 1
      }
      val copied =
        runCatching {
          contentResolver.openInputStream(uri)?.use { input ->
            target.outputStream().use { output -> input.copyTo(output) }
          } != null
        }.getOrDefault(false)
      if (copied && target.isFile) out.add(target.absolutePath)
    }
    return out
  }

  private fun deliverShare(uris: List<Uri>, text: String?) {
    val paths = copySharedFiles(uris)
    if (paths.isEmpty() && text.isNullOrBlank()) return
    val port = waitForBackend() ?: return
    val payload =
      JSONObject().apply {
        put("paths", JSONArray(paths))
        if (!text.isNullOrBlank()) put("text", text)
      }
    postInbox(port, payload.toString())
  }

  /// The backend writes instance.json before it starts serving requests, so
  /// retry briefly instead of losing a share that arrived during startup.
  private fun waitForBackend(timeoutMs: Long = 30_000): Int? {
    val info = File(applicationInfo.dataDir, "instance.json")
    val deadline = System.currentTimeMillis() + timeoutMs
    while (System.currentTimeMillis() < deadline) {
      runCatching {
        val port = JSONObject(info.readText()).optInt("port", 0)
        if (port > 0) return port
      }
      Thread.sleep(250)
    }
    return null
  }

  private fun postInbox(port: Int, payload: String): Boolean {
    repeat(20) {
      val ok =
        runCatching {
          val url = URL("http://127.0.0.1:$port/api/ui/shared-inbox")
          val conn = url.openConnection() as HttpURLConnection
          conn.requestMethod = "POST"
          conn.doOutput = true
          conn.connectTimeout = 2000
          conn.readTimeout = 5000
          conn.setRequestProperty("Content-Type", "application/json")
          conn.outputStream.use { it.write(payload.toByteArray(Charsets.UTF_8)) }
          val code = conn.responseCode
          conn.disconnect()
          code in 200..299
        }.getOrDefault(false)
      if (ok) return true
      Thread.sleep(250)
    }
    return false
  }
}
