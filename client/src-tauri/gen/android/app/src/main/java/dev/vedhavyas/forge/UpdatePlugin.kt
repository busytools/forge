package dev.vedhavyas.forge

import android.app.Activity
import android.content.Intent
import android.content.pm.PackageInfo
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import androidx.core.content.FileProvider
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest
import org.json.JSONObject

private const val USER_AGENT = "forge-client-android"
private const val CONNECT_TIMEOUT_MS = 15_000
private const val READ_TIMEOUT_MS = 60_000

@InvokeArg
class CheckArgs {
  lateinit var version: String
  lateinit var endpoint: String
}

@InvokeArg
class InstallArgs {
  lateinit var version: String
  lateinit var url: String
}

/**
 * The phone's half of the client's own update: reads the release manifest,
 * downloads the APK, checks it against this install, and hands it to the
 * system installer - which is the only install path a sideloaded app has.
 *
 * The shell's own commands bridge here, so the header's update line is one
 * surface on both platforms; this class carries the parts that must be
 * Android's (the package manager's signer read and the installer intent).
 */
@TauriPlugin
class UpdatePlugin(private val activity: Activity) : Plugin(activity) {
  @Command
  fun check(invoke: Invoke) {
    val args = invoke.parseArgs(CheckArgs::class.java)
    background(invoke) {
      val manifest = fetch(args.endpoint)
      val android = manifest.optJSONObject("android")
      val version = manifest.optString("version")
      if (android != null && isNewer(version, args.version)) {
        invoke.resolve(JSObject().put("version", version).put("url", android.getString("url")))
      } else {
        invoke.resolve()
      }
    }
  }

  @Command
  fun install(invoke: Invoke) {
    val args = invoke.parseArgs(InstallArgs::class.java)
    background(invoke) {
      val apk = download(args.version, args.url)
      try {
        verify(apk, args.version)
      } catch (err: Exception) {
        // A file that failed its check must not be the one a later tap reuses.
        apk.delete()
        throw err
      }
      handToInstaller(invoke, apk)
    }
  }

  /** Commands run on the main thread, and all of this is network and disk. */
  private fun background(invoke: Invoke, work: () -> Unit) {
    Thread {
      try {
        work()
      } catch (err: Exception) {
        invoke.reject(err.message ?: err.toString())
      }
    }.start()
  }

  private fun fetch(endpoint: String): JSONObject {
    val connection = URL(endpoint).openConnection() as HttpURLConnection
    connection.connectTimeout = CONNECT_TIMEOUT_MS
    connection.readTimeout = READ_TIMEOUT_MS
    connection.setRequestProperty("User-Agent", USER_AGENT)
    connection.setRequestProperty("Accept", "application/json")
    try {
      connection.inputStream.use { stream ->
        return JSONObject(stream.readBytes().decodeToString())
      }
    } finally {
      connection.disconnect()
    }
  }

  /** The file for `version`, downloaded once: a verified one is reused. */
  private fun download(version: String, url: String): File {
    val file = File(activity.cacheDir, "forge-$version.apk")
    if (file.exists()) {
      return file
    }
    val connection = URL(url).openConnection() as HttpURLConnection
    connection.connectTimeout = CONNECT_TIMEOUT_MS
    connection.readTimeout = READ_TIMEOUT_MS
    connection.setRequestProperty("User-Agent", USER_AGENT)
    try {
      connection.inputStream.use { stream ->
        file.outputStream().use { stream.copyTo(it) }
      }
    } catch (err: Exception) {
      file.delete()
      throw err
    } finally {
      connection.disconnect()
    }
    return file
  }

  /**
   * The download must be the version the manifest named, carrying this
   * install's own signer - the identity the system installer checks anyway,
   * so a mismatch is refused here with the reason rather than as an install
   * failure later. A debug-signed install therefore refuses the release APK
   * instead of trying an upgrade the installer would reject.
   */
  private fun verify(apk: File, version: String) {
    val manager = activity.packageManager
    val flags = signerFlags()
    val archive =
      manager.getPackageArchiveInfo(apk.absolutePath, flags)
        ?: throw IllegalStateException("the downloaded file is not an app package")
    if (archive.versionName != version) {
      throw IllegalStateException("the download is version ${archive.versionName}, expected $version")
    }
    val mine = digestOf(manager.getPackageInfo(activity.packageName, flags))
    val theirs = digestOf(archive)
    if (mine != theirs) {
      throw IllegalStateException("the download's signer is $theirs, this install's is $mine")
    }
  }

  private fun signerFlags(): Int =
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
      PackageManager.GET_SIGNING_CERTIFICATES
    } else {
      @Suppress("DEPRECATION") PackageManager.GET_SIGNATURES
    }

  private fun digestOf(info: PackageInfo): String {
    val signer =
      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
        info.signingInfo?.apkContentsSigners?.firstOrNull()
      } else {
        @Suppress("DEPRECATION") info.signatures?.firstOrNull()
      } ?: throw IllegalStateException("a signer could not be read")
    return MessageDigest.getInstance("SHA-256").digest(signer.toByteArray()).joinToString("") {
      "%02x".format(it)
    }
  }

  private fun handToInstaller(invoke: Invoke, apk: File) {
    val uri: Uri = FileProvider.getUriForFile(activity, "${activity.packageName}.fileprovider", apk)
    val intent =
      Intent(Intent.ACTION_VIEW).apply {
        setDataAndType(uri, "application/vnd.android.package-archive")
        addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
      }
    // On the UI thread, and answered from inside it: a device with nothing
    // handling the intent throws at startActivity, and that must be a
    // rejection rather than a crash on the main thread.
    activity.runOnUiThread {
      try {
        activity.startActivity(intent)
        invoke.resolve()
      } catch (err: Exception) {
        invoke.reject(err.message ?: err.toString())
      }
    }
  }

  private fun parts(version: String): List<Int>? {
    val parts = version.trimStart('v').split('.').mapNotNull { it.toIntOrNull() }
    return if (parts.size == 3) parts else null
  }

  /** Strictly newer, and only when both sides parse; anything else is no offer. */
  private fun isNewer(candidate: String, current: String): Boolean {
    val next = parts(candidate) ?: return false
    val held = parts(current) ?: return false
    for (index in 0..2) {
      if (next[index] != held[index]) {
        return next[index] > held[index]
      }
    }
    return false
  }
}
