package dev.vedhavyas.forge

import android.app.Activity
import android.content.Intent
import android.net.Uri
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.net.HttpURLConnection
import java.net.URL
import org.json.JSONObject

private const val USER_AGENT = "forge-client-android"
private const val CONNECT_TIMEOUT_MS = 15_000
private const val READ_TIMEOUT_MS = 60_000

@InvokeArg
// Field for field with `CheckArgs` in the shell's `android` module.
class CheckArgs {
  lateinit var version: String
  lateinit var endpoint: String
}

@InvokeArg
// Field for field with `OpenArgs` in the shell's `android` module.
class OpenArgs {
  lateinit var url: String
}

/**
 * A release version as three integers, or `null` for anything else - a
 * prerelease suffix, a fourth segment, junk. Every segment must parse: a
 * compare that dropped the ones that did not would read `x.1.0.116` as
 * `1.0.116` and offer it.
 */
internal fun versionParts(version: String): List<Int>? {
  val segments = version.trim().trimStart('v').split('.')
  if (segments.size != 3) {
    return null
  }
  return segments.map { it.toIntOrNull() ?: return null }
}

/** Strictly newer, and only when both sides parse; anything else is no offer. */
internal fun isNewerVersion(candidate: String, current: String): Boolean {
  val next = versionParts(candidate) ?: return false
  val held = versionParts(current) ?: return false
  for (index in 0..2) {
    if (next[index] != held[index]) {
      return next[index] > held[index]
    }
  }
  return false
}

/** The release a manifest names, in the shape the app's own version takes. */
internal fun normalizeVersion(version: String): String? = versionParts(version)?.joinToString(".")

/**
 * The phone's half of the client's own update: reads the release manifest and
 * opens the release page the header's line offers.
 *
 * The app installs nothing itself - a sideloaded install is the person's own -
 * so the offer is a browser hand-off rather than a download.
 */
@TauriPlugin
class UpdatePlugin(private val activity: Activity) : Plugin(activity) {
  @Command
  fun check(invoke: Invoke) {
    val args = invoke.parseArgs(CheckArgs::class.java)
    background(invoke) {
      val manifest = fetch(args.endpoint)
      val android = manifest.optJSONObject("android")
      val version = normalizeVersion(manifest.optString("version"))
      // A manifest with no usable `release` offers nothing: only an http(s)
      // URL has a handler to open, and an offer whose tap always fails is
      // worse than no line. `optString` also answers "null" for a JSON null.
      val release =
        android
          ?.optString("release")
          ?.takeIf { it.startsWith("https://") || it.startsWith("http://") }
      if (version != null && release != null && isNewerVersion(version, args.version)) {
        invoke.resolve(JSObject().put("version", version).put("url", release))
      } else {
        invoke.resolve()
      }
    }
  }

  /** The offered release's page, in the browser the phone already has. */
  @Command
  fun open(invoke: Invoke) {
    val args = invoke.parseArgs(OpenArgs::class.java)
    val intent = Intent(Intent.ACTION_VIEW, Uri.parse(args.url))
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

  /** Commands run on the main thread, and this is network. */
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
}
