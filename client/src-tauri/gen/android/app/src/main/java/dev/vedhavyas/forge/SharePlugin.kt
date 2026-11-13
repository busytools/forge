package dev.vedhavyas.forge

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.OpenableColumns
import android.util.Log
import androidx.core.content.IntentCompat
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSArray
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File
import java.io.InputStream
import java.util.concurrent.Executors
import java.util.concurrent.Future
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicReference

/** One file a share handed this app, copied into its cache and named for the page. */
internal data class SharedFile(
  val path: String,
  val name: String,
  val size: Long,
  val mime: String?,
)

/** The delivery the page acknowledges: the sequence number it was handed. */
@InvokeArg
// Field for field with `AckShare` in the shell's `share` module.
class AckArgs {
  var seq: Long = 0
}

/**
 * The share sheet's intake: the files another app shared with forge.
 *
 * **The page never sees a content URI.** A provider's grant is scoped to this
 * activity and dies with it, so each shared file is copied into the app's
 * cache here, under its display name, and the page is handed absolute paths.
 * The copy runs on a bounded pool: a share can be a few hundred megabytes,
 * and a provider that stalls a read must not hold a thread for ever or start
 * a new one per share. **The timeout runs on a thread of its own**, because a
 * timer sharing the copy pool would be starved by exactly the stuck copies it
 * exists to cut loose.
 *
 * **The budget is enforced before and during the copy.** The provider's
 * published sizes are checked against the 200 MB cap first, and the bytes
 * that actually land are counted against the remainder as they are written -
 * a provider that under-reports cannot get past it.
 *
 * A cold start's share is the activity's launch intent and the page asks for
 * it; a share into a running app arrives as a new intent and is pushed as an
 * event. Both answer the same payload, so the page has one reader. **A share
 * that arrives before the page announces its listener is buffered** and
 * answered by the page's first ask, and **the answered token lives for this
 * process only**: a reload does not prompt twice, while a genuine re-share -
 * including one that re-delivers a launch intent after a process restart -
 * answers again. A duplicate sheet is visible and dismissable; a swallowed
 * share is not, and Android cannot tell the two apart across a process.
 *
 * **The buffer, its acknowledgement and the intent plumbing are device-only.**
 * `SharePluginTest` pins the pure rules - what counts as a share, the token,
 * the copy budget - and the delivery path itself is exercised on a device,
 * where an intent and a webview exist to exercise it.
 */
@TauriPlugin
class SharePlugin(private val activity: Activity) : Plugin(activity) {
  /** The copy threads: bounded, so a share cannot start a thread per file. */
  private val copies = Executors.newFixedThreadPool(2)

  /** The copy timers, alone, so a stuck copy cannot hold the thread a timeout needs. */
  private val timers = Executors.newSingleThreadScheduledExecutor()

  /**
   * Whether the page's own listener is registered.
   *
   * **`hasListener` reports the shell's channel, not the page's `listen`**:
   * the channel is registered at plugin setup, long before the page's first
   * draw, so a trigger that trusts it races the page and drops the share.
   * The page announces this after its listener is up.
   */
  @Volatile private var pageReady = false

  init {
    // **A build that wrote the answered token to disk leaves it behind**, and
    // "this process only" has to hold on an upgrade too: one clear at startup
    // finishes the move off SharedPreferences. **Once per process**, so a
    // later key under that name is not wiped by a plugin reconstructed for a
    // re-created activity.
    if (!prefsCleared) {
      prefsCleared = true
      activity.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit().clear().apply()
    }
  }

  @Command
  fun take(invoke: Invoke) {
    // **A share buffered while the page had no listener is this ask's
    // answer**: a warm share that arrived before the listener registered is
    // not lost, and taking it is what marks it delivered.
    takeBuffered()?.let { (answer, token) ->
      markAnswered(token)
      invoke.resolveObject(answer)
      return
    }
    val intent = MainActivity.newest
    val streams = streamsOf(intent)
    val token = shareToken(MainActivity.launches, intent?.action, intent?.type, streams)
    if (token == null || !shouldAnswer(token, answered)) {
      invoke.resolveObject(payload(emptyList(), null))
      return
    }
    answer(streams) { answerPayload ->
      markAnswered(token)
      invoke.resolveObject(answerPayload)
    }
  }

  /**
   * The page's listener is registered, so a warm share may be pushed.
   *
   * **The page says this after `watchShares()` resolves**, and only then may
   * `onNewIntent` trigger; a share delivered before it is buffered instead of
   * emitted into a webview with no listener, which Tauri drops silently.
   */
  @Command
  fun ready(invoke: Invoke) {
    pageReady = true
    // **Every share buffered before the announcement is pushed now, in arrival
    // order**: the launch does not wait past its readiness cap, so the
    // announcement can land after the page's ask already found an empty
    // buffer - and a later share would otherwise wait for the next launch.
    val waiting = synchronized(lock) { pending.toList() }
    if (waiting.isNotEmpty()) {
      activity.runOnUiThread { waiting.forEach { trigger("share", it.answer) } }
    }
    invoke.resolve()
  }

  /**
   * The page received the share it was pushed, so the delivery is proven.
   *
   * **The mark follows an acknowledgement, never a fire-and-forget push**: a
   * trigger into a webview with no listener (a page reload, a re-created
   * activity) is dropped silently by Tauri, so marking there would lose the
   * share for good. The answer stays buffered until this arrives or the next
   * ask drains it.
   */
  @Command
  fun ack(invoke: Invoke) {
    val args = invoke.parseArgs(AckArgs::class.java)
    synchronized(lock) {
      val index = pending.indexOfFirst { it.seq == args.seq }
      if (index >= 0) {
        markAnswered(pending[index].token)
        pending.removeAt(index)
      }
    }
    invoke.resolve()
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
    val token = shareToken(MainActivity.launches, intent.action, intent.type, streams) ?: return
    val seq = nextSeq()
    answer(streams) { answerPayload ->
      answerPayload.put("seq", seq)
      activity.runOnUiThread {
        // **The answer is held until the page takes or acknowledges it.** A
        // push into a webview with no listener is dropped silently, so the
        // buffer is what makes a lost push recoverable by the next ask; the
        // page's acknowledgement marks a delivered one.
        buffer(answerPayload, token, seq)
        if (pageReady) trigger("share", answerPayload)
      }
    }
  }

  /** Answer one share: an empty SEND is said, and the rest are copied. */
  private fun answer(streams: List<String>, done: (JSObject) -> Unit) {
    if (streams.isEmpty()) {
      // **A SEND that carries no stream is a share of nothing, not a
      // failure**: the payload says so with its own flag, so the sheet draws
      // its neutral "carried no files" rather than an error it did not meet.
      done(payload(emptyList(), null, empty = true))
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
        // **The door that asked marks the delivery, not this copy**: an ask
        // resolves it to the page, an event buffers and triggers it, and only
        // the page's acknowledgement proves the pushed one arrived.
        activity.runOnUiThread { done(payload(files, error)) }
      }
    }
    val running = AtomicReference<Future<*>?>(null)
    running.set(
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
    )
    // **The timer cancels the copy it timed out**: a provider stuck in a read
    // holds its pool thread until it is interrupted, and two of those would
    // leave the next share nothing to run on.
    timers.schedule(
      {
        running.get()?.cancel(true)
        finish(emptyList(), COPY_TIMEOUT)
      },
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
    var landed = 0L
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
        copyInto(dir, display, mime, MAX_TOTAL - landed) {
          activity.contentResolver.openInputStream(uri)
        }
      landed = landedAfter(landed, copied.size)
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

  /** Remember this process answered the share, so a reload does not. */
  private fun markAnswered(token: String) {
    // **The mark dies with the process on purpose**: across a restart Android
    // cannot tell a re-delivered launch intent from a genuine re-share of the
    // same file, and the recoverable mistake is a duplicate sheet.
    answered = token
  }

  /**
   * Hold one share's answer until the page takes it or acknowledges it.
   *
   * **A small queue, because two shares can arrive inside one round trip**:
   * a single slot would silently lose the first. Past the cap the oldest goes
   * and the loss is logged - the buffer bounds memory for a page that never
   * comes back, and a person's own pace never reaches it.
   */
  private fun buffer(answer: JSObject, token: String, seq: Long) {
    synchronized(lock) {
      pending.addLast(Buffered(answer, token, seq))
      while (pending.size > MAX_BUFFERED) {
        pending.removeFirst()
        Log.w(TAG, "a buffered share was dropped before the page took it")
      }
    }
  }

  /** The oldest buffered share, with the token a take marks delivered. */
  private fun takeBuffered(): Pair<JSObject, String>? {
    synchronized(lock) {
      val first = pending.removeFirstOrNull() ?: return null
      return Pair(first.answer, first.token)
    }
  }

  companion object {
    private const val TAG = "SharePlugin"

    /** The preferences a build before the in-process token wrote to. */
    private const val PREFS = "forge-share"

    /** Whether the pre-move preferences were cleared for this process. */
    @Volatile private var prefsCleared = false

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

    internal const val TOO_LARGE = "the share is larger than 200 MB"
    internal const val COPY_TIMEOUT = "the share took too long to copy"
    internal const val STALLED = "the share's provider stopped before the file ended"

    /**
     * What a share's landed bytes become after one file, refusing a share
     * that would cross the wall.
     *
     * **The budget is the share's, not one file's**: every file is measured
     * against what the earlier ones already used, so a provider that
     * under-reports one file cannot buy the next one room.
     */
    internal fun landedAfter(landed: Long, copied: Long): Long {
      if (copied > MAX_TOTAL - landed) throw IllegalArgumentException(TOO_LARGE)
      return landed + copied
    }

    /** One share held until the page takes or acknowledges it. */
    private data class Buffered(val answer: JSObject, val token: String, val seq: Long)

    /** The shares a page has not taken yet, oldest first. */
    private val pending = ArrayDeque<Buffered>()

    /** How many buffered shares are held for a page that never asks. */
    private const val MAX_BUFFERED = 8

    private var nextSeq: Long = 1
    private val lock = Any()

    /** The next delivery's sequence number, so an ack names the one it took. */
    private fun nextSeq(): Long =
      synchronized(lock) {
        val seq = nextSeq
        nextSeq += 1
        seq
      }

    /** The share this process last answered; cleared by a new process. */
    @Volatile private var answered: String? = null

    /**
     * The payload both doors answer with: the files, or why there are none.
     *
     * **`empty` is its own fact, not an error**: a SEND that carried no stream
     * is a share of nothing and draws the sheet's neutral line, while `error`
     * is a share that could not be read and draws its reason.
     */
    internal fun payload(files: List<SharedFile>, error: String?, empty: Boolean = false): JSObject {
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
      payload.put("empty", empty)
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
     * What identifies one launch-door delivery of a share.
     *
     * **A reload is not a second share, and a new activity launch is.** The
     * launch number is in the token, so a page reload inside one launch
     * matches the answered token, while a share that opens a fresh activity
     * (the launch count moves) answers again even for the same files. The
     * process stamp is deliberately absent: it cannot survive a restart, and
     * comparing across one is what lost a genuine re-share before - after a
     * restart the answered token is empty, so everything answers.
     *
     * **A SEND with no stream still has a token**: it is a share this app
     * takes, it carried no file, and the sheet says so rather than the app
     * opening on nothing.
     */
    internal fun shareToken(
      launch: Int,
      action: String?,
      type: String?,
      streams: List<String>,
    ): String? {
      if (action != Intent.ACTION_SEND && action != Intent.ACTION_SEND_MULTIPLE) return null
      return listOf(launch.toString(), action ?: "", type ?: "", streams.joinToString("\u0000"))
        .joinToString("\u0000")
    }

    /**
     * Whether this ask should answer the share.
     *
     * **A reload is not a second share**: the token is compared with the one
     * this process already answered. Nothing else is suppressed - across a
     * process the answered token is gone, so a re-delivered launch intent
     * prompts again rather than a genuine re-share being lost.
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
              // **A read that answers nothing is not progress**: a provider
              // that returns zero would spin this loop for ever, so it is a
              // stall the sheet can say rather than a hang it cannot.
              if (read == 0) throw IllegalStateException(STALLED)
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
