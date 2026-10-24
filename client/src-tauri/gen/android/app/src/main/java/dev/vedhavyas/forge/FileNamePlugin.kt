package dev.vedhavyas.forge

import android.app.Activity
import android.net.Uri
import android.provider.OpenableColumns
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.Plugin

@InvokeArg
// Field for field with `UriArgs` in the shell's `android` module.
class UriArgs {
  lateinit var uri: String
}

/**
 * A picked document's own name, for the page's file chooser.
 *
 * **A content URI carries an opaque document id, not a file name**, so the
 * shell asks the resolver here: the page used to see `0-file` for the resume
 * the person chose. The name is the one the system shows for the document;
 * the URI's last segment is the fallback when the provider publishes none.
 */
@TauriPlugin
class FileNamePlugin(private val activity: Activity) : Plugin(activity) {
  @Command
  fun name(invoke: Invoke) {
    val args = invoke.parseArgs(UriArgs::class.java)
    val uri = Uri.parse(args.uri)
    var display: String? = null
    try {
      activity.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
        ?.use { cursor ->
          if (cursor.moveToFirst() && !cursor.isNull(0)) {
            display = cursor.getString(0)
          }
        }
    } catch (_: Exception) {
      // A provider that refuses the query falls back to the URI segment.
    }
    if (display.isNullOrEmpty()) {
      display = uri.lastPathSegment ?: "file"
    }
    invoke.resolveObject(display)
  }
}
