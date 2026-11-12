package dev.vedhavyas.forge

import android.app.Activity
import android.content.Context
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
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

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
 * The copy runs on one bounded executor: a share can be a few hundred
 * megabytes, and a provider that stalls a read must not hold a thread for
 * ever or start a new one per share.
 *
 * **The budget is enforced before and during the copy.** The provider's
 * published sizes are checked against the 200 MB cap first, and the bytes
 * that actually land are counted against the remainder as they are written -
 * a provider that under-reports cannot get past it.
 *
 * A cold start's share is the activity's launch intent and the page asks for
 * it; a share into a running app arrives as a new intent and is pushed as an
 * event. Both answer the same payload, so the page has one reader. **A share
 * that arrives before the page listens is buffered**, and the answered token
 * is kept across a process restart, so a sent share is not prompted again.
 */
@TauriPlugin
class SharePlugin(private val activity: Activity) : Plugin(activity) {
  /** One copy thread plus one timeout timer; a stuck copy cannot block the timer. */
  private val copies = Executors.newScheduledThreadPool(2)

  @Command
  fun take(invoke: Invoke) {
    // **A share buffered while no page listened is this ask's answer**: a
    // warm share that arrived before the listener registered is not lost.
    takeBuffered()?.let {
      invoke.resolveObject(it)
      return
    }
    val intent = MainActivity.newest
    val streams = streamsOf(intent)
    val action = intent?.action
    val token = shareToken(action, intent?.type, streams)
    if (token == null || !shouldAnswer(token, answeredToken())) {
      invoke.resolveObject(payload(emptyList(), null))
      return
    }
    answer(token, streams, invoke::resolveObject)
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
    answer(token, streams) { answerPayload ->
      activity.runOnUiThread {
        // **A page that has not registered its listener yet gets the share
        // buffered** rather than dropped; its first ask answers it.
        if (hasListener("share")) trigger("share", answerPayload) else buffer(answerPayload)
      }
    }
  }

  /** Answer one share: an empty SEND is said, and the rest are copied. */
  private fun answer(
    token: String,
    streams: List<String>,
    done: (JSObject) -> Unit,
  ) {
    markAnswered(token)
    if (streams.isEmpty()) {
      // **A SEND that carries no stream is a share of nothing**, and saying so
      // is what makes the sheet's "this share carried no file" reachable.
      done(payload(emptyList(), NO_FILE))
      return
    }
    if (streams.size > MAX_FILES) {
      done(payload(emptyList(), "one share carries at most $MAX_FILES files"))
      return
    }
    copy(streams, done)
  }

  /**
   * The copy, on the bounded executor, answered once.
   *
   * **A provider that stalls a read cannot hold the share for ever**: the
   * timer answers the timeout even while the copy thread is blocked, and the
   * first answer wins.
   */
  private fun copy(streams: List<String>, done: (JSObject) -> Unit) {
    val delivered = AtomicBoolean(false)
    val finish: (List<SharedFile>, String?) -> Unit = { files, error ->
      if (delivered.compareAndSet(false, true)) {
        activity.runOnUiThread { done(payload(files, error)) }
      }
    }
    copies.submit {
      val answer =
        try {
          Pair(copyStreams(streams), null)
        } catch (error: Exception) {
          // A share that cannot be read is said rather than drawn as an empty
          // one: the page shows the reason instead of a sheet with no files.
          Pair(emptyList(), error.message ?: error.toString())
        }
      finish(answer.first, answer.second)
    }
    copies.schedule(
      { finish(emptyList(), COPY_TIMEOUT) },
      COPY_TIMEOUT_SECONDS,
      TimeUnit.SECONDS,
    )
  }

  /** Copy every shared URI into one directory for this share, within the budget. */
  private fun copyStreams(streams: List<String>): List<SharedFile> {
    if (streams.isEmpty()) return emptyList()
    // **The provider's declared sizes are checked before a byte is copied**:
    // a share over the cap is refused rather than copied and then refused.
    var declared = 0L
    for (raw in streams) {
      val (_, size) = displayOf(Uri.parse(raw))
      if (size == null) continue
      if (size > MAX_TOTAL || declared > MAX_TOTAL - size) {
        throw IllegalArgumentException(TOO_LARGE)
      }
      declared += size
    }
    val dir = shareDir()
    var remaining = MAX_TOTAL
    return streams.map { raw ->
      val uri = Uri.parse(raw)
      val (display, _) = displayOf(uri)
      val mime =
        try {
          activity.contentResolver.getType(uri)
        } catch (_: Exception) {
          null
        }
      val copied =
        copyInto(dir, display, mime, remaining) {
          activity.contentResolver.openInputStream(uri)
        }
      remaining -= copied.size
      copied
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

  /** The share this app last answered, from this process or before it. */
  private fun answeredToken(): String? =
    answered ?: activity.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getString(ANSWERED, null)

  private fun markAnswered(token: String) {
    answered = token
    // **A process death must not re-prompt a share it already sent**: the
    // launch intent outlives the process, so the token does too.
    activity
      .getSharedPreferences(PREFS, Context.MODE_PRIVATE)
      .edit()
      .putString(ANSWERED, token)
      .apply()
  }

  private fun buffer(answer: JSObject) {
    synchronized(lock) { pending = answer }
  }

  private fun takeBuffered(): JSObject? {
    synchronized(lock) {
      val answer = pending
      pending = null
      return answer
    }
  }

  companion object {
    /** How long a copied share is kept before the next one drops it. */
    private const val KEEP = 24L * 60 * 60 * 1000

    /** A cache name's own cap, in BYTES: an astral name would cross the filesystem's. */
    private const val NAME_CAP = 120

    /** The most one share may carry, the same wall the server enforces. */
    private const val MAX_TOTAL = 200L * 1024 * 1024

    /** The most files one share may name, the same bound the server holds. */
    private const val MAX_FILES = 256

    /** How long one share's copy may run before the sheet is told it stalled. */
    private const val COPY_TIMEOUT_SECONDS = 60L

    private const val PREFS = "forge-share"
    private const val ANSWERED = "answered"

    internal const val NO_FILE = "this share carried no file"
    internal const val TOO_LARGE = "the share is larger than 200 MB"
    internal const val COPY_TIMEOUT = "the share took too long to copy"

    /** The stashed share a page had no listener for. */
    private var pending: JSObject? = null
    private val lock = Any()

    /** The share this process last answered, as the fast path over [`PREFS`]. */
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
     * **A SEND with no stream still has a token**: it is a share this app
     * takes, it carried no file, and the sheet says so rather than the app
     * opening on nothing.
     */
    internal fun shareToken(action: String?, type: String?, streams: List<String>): String? {
      if (action != Intent.ACTION_SEND && action != Intent.ACTION_SEND_MULTIPLE) return null
      return (action ?: "") + "\u0000" + (type ?: "") + "\u0000" + streams.joinToString("\u0000")
    }

    /**
     * Whether this ask should answer the share.
     *
     * **A reload is not a second share**: the token is compared with the one
     * already answered, across a process restart too.
     */
    internal fun shouldAnswer(token: String?, answered: String?): Boolean =
      token != null && token != answered

    /** A display name made safe for the cache, and never a path. */
    internal fun cacheName(display: String?): String {
      val last = (display ?: "").split('/', '\\').lastOrNull()?.trim().orEmpty()
      val cleaned = last.filter { !it.isISOControl() }.trim()
      if (cleaned.isEmpty() || cleaned == "." || cleaned == "..") return "file"
      // **The cap is bytes**: a name of astral characters is two UTF-16 chars
      // each and four bytes, so a char count would cross the filesystem's.
      var used = 0
      var end = 0
      var index = 0
      while (index < cleaned.length) {
        val point = cleaned.codePointAt(index)
        val bytes = String(Character.toChars(point)).toByteArray(Charsets.UTF_8).size
        if (used + bytes > NAME_CAP) break
        used += bytes
        index += Character.charCount(point)
        end = index
      }
      return if (end == 0) "file" else cleaned.substring(0, end)
    }

    /**
     * Copy one stream into `dir` under `display`, answering the file it wrote.
     *
     * The size is the bytes that actually landed rather than what the
     * provider claimed, and **the bytes are counted against the share's
     * remainder as they are written**: a provider that under-reports its
     * length cannot get past the cap.
     */
    internal fun copyInto(
      dir: File,
      display: String?,
      mime: String?,
      budget: Long,
      open: () -> InputStream?,
    ): SharedFile {
      val name = cacheName(display)
      val target = unique(dir, name)
      val stream = open() ?: throw IllegalStateException("the shared file could not be opened")
      var copied = 0L
      try {
        stream.use { input ->
          target.outputStream().use { output ->
            val buffer = ByteArray(64 * 1024)
            while (true) {
              val read = input.read(buffer)
              if (read < 0) break
              copied += read
              if (copied > budget) throw IllegalStateException(TOO_LARGE)
              output.write(buffer, 0, read)
            }
          }
        }
      } catch (error: Exception) {
        target.delete()
        throw error
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
