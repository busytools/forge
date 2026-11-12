package dev.vedhavyas.forge

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.provider.OpenableColumns
import androidx.core.content.IntentCompat
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSArray
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File
import java.io.InputStream

/** One file a share handed this app, copied into its cache and named for the page. */
internal data class SharedFile(
  val path: String,
  val name: String,
  val size: Long,
  val mime: String?,
)

/**
 * The share sheet's intake: the files another app shared with forge.
 *
 * **The page never sees a content URI.** A provider's grant is scoped to this
 * activity and dies with it, so each shared file is copied into the app's
 * cache here, under its display name, and the page is handed absolute paths.
 * The copy runs off the main thread: a share can be a few hundred megabytes.
 *
 * A cold start's share is the activity's launch intent and the page asks for
 * it; a share into a running app arrives as a new intent and is pushed as an
 * event. Both answer the same payload, so the page has one reader.
 */
@TauriPlugin
class SharePlugin(private val activity: Activity) : Plugin(activity) {
  @Command
  fun take(invoke: Invoke) {
    val intent = MainActivity.newest
    val streams = streamsOf(intent)
    val token = shareToken(intent?.action, intent?.type, streams)
    // **A reload is not a second share.** The launch intent stays on the
    // activity for its whole life, so a page that asks again would copy the
    // same files into the cache again and open the sheet a second time.
    if (token == null || token == answered) {
      invoke.resolveObject(payload(emptyList(), null))
      return
    }
    answered = token
    copy(streams) { files, error -> invoke.resolveObject(payload(files, error)) }
  }

  /**
   * A share into an app that is already running.
   *
   * **The intent is answered here rather than left for the page's ask**: the
   * activity is reused, so `MainActivity.newest` moves to it and the page
   * would otherwise only meet it on its next reload.
   */
  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    val streams = streamsOf(intent)
    val token = shareToken(intent.action, intent.type, streams) ?: return
    answered = token
    copy(streams) { files, error ->
      activity.runOnUiThread { trigger("share", payload(files, error)) }
    }
  }

  /** The copy, off the main thread, answered on it. */
  private fun copy(
    streams: List<String>,
    done: (List<SharedFile>, String?) -> Unit,
  ) {
    Thread {
      val answer =
        try {
          Pair(copyStreams(streams), null)
        } catch (error: Exception) {
          // A share that cannot be read is said rather than drawn as an empty
          // one: the page shows the reason instead of a sheet with no files.
          Pair(emptyList(), error.message ?: error.toString())
        }
      activity.runOnUiThread { done(answer.first, answer.second) }
    }.start()
  }

  /** Copy every shared URI into one directory for this share. */
  private fun copyStreams(streams: List<String>): List<SharedFile> {
    if (streams.isEmpty()) return emptyList()
    val dir = shareDir()
    return streams.map { raw ->
      val uri = Uri.parse(raw)
      val (display, _) = displayOf(uri)
      val mime = try {
        activity.contentResolver.getType(uri)
      } catch (_: Exception) {
        null
      }
      copyInto(dir, display, mime) {
        activity.contentResolver.openInputStream(uri)
      }
    }
  }

  /** The name and size the provider publishes for a document, when it publishes them. */
  private fun displayOf(uri: Uri): Pair<String?, Long?> {
    var name: String? = null
    var size: Long? = null
    try {
      activity.contentResolver
        .query(uri, arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE), null, null, null)
        ?.use { cursor ->
          if (cursor.moveToFirst()) {
            val nameAt = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
            if (nameAt >= 0 && !cursor.isNull(nameAt)) name = cursor.getString(nameAt)
            val sizeAt = cursor.getColumnIndex(OpenableColumns.SIZE)
            if (sizeAt >= 0 && !cursor.isNull(sizeAt)) size = cursor.getLong(sizeAt)
          }
        }
    } catch (_: Exception) {
      // A provider that refuses the query falls back to the URI's own name.
    }
    return Pair(name, size)
  }

  /**
   * A fresh directory for this share, with older ones dropped.
   *
   * **One directory per share** so two files of the same display name do not
   * collide, and so a retry after a failed upload still reads the same paths.
   * The cache is the system's to evict; this only keeps our own litter from
   * growing without bound.
   */
  private fun shareDir(): File {
    val root = File(activity.cacheDir, "share")
    root.mkdirs()
    val now = System.currentTimeMillis()
    root.listFiles()?.forEach { old ->
      if (now - old.lastModified() > KEEP) old.deleteRecursively()
    }
    val dir = File(root, now.toString())
    dir.mkdirs()
    return dir
  }

  companion object {
    /** How long a copied share is kept before the next one drops it. */
    private const val KEEP = 24L * 60 * 60 * 1000

    /** A cache name's own cap, so a provider's long name cannot fail the write. */
    private const val NAME_CAP = 120

    /**
     * The share this process last answered.
     *
     * A cold start's intent lives as long as the activity, so the token is
     * what tells a page reload from a new share. A share into a running app
     * always answers, and moves the token to its own.
     */
    @Volatile private var answered: String? = null

    /** The payload both doors answer with: the files, or why there are none. */
    internal fun payload(files: List<SharedFile>, error: String?): JSObject {
      val rows = JSArray()
      for (file in files) {
        val row = JSObject()
        row.put("path", file.path)
        row.put("name", file.name)
        row.put("size", file.size)
        row.put("mime", file.mime)
        rows.put(row)
      }
      val payload = JSObject()
      payload.put("files", rows)
      payload.put("error", error)
      return payload
    }

    /**
     * The URIs one intent shares, as strings.
     *
     * **Pure on purpose**, so what counts as a share is pinned by a JVM test
     * rather than only exercised on a device: a single share carries one
     * `EXTRA_STREAM`, a multiple share a list, and anything else shares
     * nothing this app takes.
     */
    internal fun streamStrings(
      action: String?,
      single: String?,
      multiple: List<String>?,
    ): List<String> =
      when (action) {
        Intent.ACTION_SEND -> listOfNotNull(single)
        Intent.ACTION_SEND_MULTIPLE -> multiple ?: emptyList()
        else -> emptyList()
      }

    /**
     * What identifies one share, so a reload can tell it from a new one.
     *
     * The action, the type and the URIs together: a second share of the same
     * files into a running app is answered through `onNewIntent` regardless,
     * so a repeated token there costs nothing.
     */
    internal fun shareToken(action: String?, type: String?, streams: List<String>): String? {
      if (streams.isEmpty()) return null
      return (action ?: "") + "\u0000" + (type ?: "") + "\u0000" + streams.joinToString("\u0000")
    }

    /** A display name made safe for the cache, and never a path. */
    internal fun cacheName(display: String?): String {
      val last = (display ?: "").split('/', '\\').lastOrNull()?.trim().orEmpty()
      val cleaned = last.filter { !it.isISOControl() }.trim()
      if (cleaned.isEmpty() || cleaned == "." || cleaned == "..") return "file"
      return if (cleaned.length <= NAME_CAP) cleaned else cleaned.take(NAME_CAP)
    }

    /**
     * Copy one stream into `dir` under `display`, answering the file it wrote.
     *
     * The size is the bytes that actually landed rather than what the
     * provider claimed: a name and a length are the provider's word, and the
     * upload declares what this copy holds.
     */
    internal fun copyInto(
      dir: File,
      display: String?,
      mime: String?,
      open: () -> InputStream?,
    ): SharedFile {
      val name = cacheName(display)
      val target = unique(dir, name)
      val stream = open() ?: throw IllegalStateException("the shared file could not be opened")
      stream.use { input ->
        target.outputStream().use { output -> input.copyTo(output) }
      }
      return SharedFile(target.absolutePath, name, target.length(), mime)
    }

    /** A path in `dir` no file holds yet, suffixing the stem rather than the extension. */
    internal fun unique(dir: File, name: String): File {
      val first = File(dir, name)
      if (!first.exists()) return first
      val dot = name.lastIndexOf('.')
      val stem = if (dot > 0) name.substring(0, dot) else name
      val extension = if (dot > 0) name.substring(dot) else ""
      var index = 2
      while (true) {
        val next = File(dir, "$stem-$index$extension")
        if (!next.exists()) return next
        index += 1
      }
    }
  }

  /** The URIs this activity's newest intent carries. */
  private fun streamsOf(intent: Intent?): List<String> {
    if (intent == null) return emptyList()
    val single = IntentCompat.getParcelableExtra(intent, Intent.EXTRA_STREAM, Uri::class.java)
    val multiple =
      IntentCompat.getParcelableArrayListExtra(intent, Intent.EXTRA_STREAM, Uri::class.java)
    return streamStrings(intent.action, single?.toString(), multiple?.map { it.toString() })
  }
}
