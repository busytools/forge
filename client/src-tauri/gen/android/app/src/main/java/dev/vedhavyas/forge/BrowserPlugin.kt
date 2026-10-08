package dev.vedhavyas.forge

import android.app.Activity
import android.graphics.Color
import android.net.LocalSocket
import android.net.LocalSocketAddress
import android.os.Process
import android.util.Log
import android.view.Gravity
import android.view.ViewGroup
import android.webkit.JsPromptResult
import android.webkit.JsResult
import android.webkit.WebChromeClient
import android.webkit.WebSettings
import android.webkit.WebView
import android.widget.Button
import android.widget.FrameLayout
import android.widget.LinearLayout
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.BufferedInputStream
import java.io.File
import java.io.InputStream
import java.io.OutputStream
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.security.SecureRandom

private const val TAG = "FORGE-BROWSER"

/**
 * The relay's gate decision, pure so it is unit-testable (the release's
 * correctness rests on it and it is only string work): **every request must
 * carry the token.** The pinned driver sends `--cdp-header` on the
 * discovery fetch AND the WebSocket upgrade alike (measured against a fake
 * devtools server while reviewing this), so gating everything is strictly
 * stronger than gating discovery alone - another app on the device cannot
 * even open the socket, let alone learn the browser's path.
 */
internal fun relayAllows(headText: String, token: String): Boolean =
    tokenHeaderMatches(headText, token)

/** Whether the head carries `X-Forge-Token: <token>`, header name case-insensitive. */
internal fun tokenHeaderMatches(headText: String, token: String): Boolean {
    for (line in headText.split("\r\n")) {
        val colon = line.indexOf(':')
        if (colon <= 0) continue
        if (line.substring(0, colon).trim().equals("x-forge-token", ignoreCase = true)) {
            return line.substring(colon + 1).trim() == token
        }
    }
    return false
}

@InvokeArg
// Field for field with the shell's `browser::android` start call.
class StartDriverArgs {
  lateinit var socketPath: String
  lateinit var outputDir: String
}

/**
 * The phone's browser host, the half the desktop keeps in Rust: the engine
 * itself. The desktop launches Brave/Chrome and spawns the driver; here the
 * engine is the app's OWN WebView (over its devtools socket) and the driver
 * is the vendored libnode running inside this process.
 *
 * The shell's tab pin and its refuse-a-client-page guard live in the Rust
 * side's `browser/driver.rs` (`start_inapp` and `refuse_a_client_page`), not
 * here - this side only reports the UI's origin for them.
 *
 * What this plugin owns:
 *
 * - the browser WebView: created offscreen at the engine's first use, kept
 *   for the app's life, NEVER the window the client draws until a hand-off's
 *   Open attaches the takeover;
 * - the CDP relay: a loopback TCP listener the in-app node reaches the
 *   WebView's devtools socket through (playwright speaks HTTP, and node's
 *   own abstract-socket connect to that socket is unreliable on this fork -
 *   both measured, see the spike's report). **Every request is token-gated**
 *   (`relayAllows`): the pinned driver sends its header on the discovery
 *   fetch and the WebSocket upgrade alike, so another app on the device
 *   cannot open the socket at all;
 * - the takeover: the approved Android pair - the page full-screen with one
 *   slim bar, the bar's back and the hardware Back both the door, Done
 *   answering the hand-off;
 * - the assets unpack: the driver tree (from the bundled
 *   `assets/browser-stack/playwright-mcp`) and the bootstrap script (from
 *   `assets/forge-browser`) land under filesDir the first time the engine
 *   starts, because node cannot read APK assets.
 *
 * The WebView's own devtools endpoint lists EVERY debuggable page in the
 * process - the client's own UI included - so the shell pins the driver's
 * current tab to the browser page after the handshake and refuses tab
 * commands that resolve to the UI's origin (see the shell's
 * `browser::android`). This side reports the UI's origin for that check.
 */
@TauriPlugin
class BrowserPlugin(private val activity: Activity) : Plugin(activity) {
  companion object {
    /** The live plugin, for the activity's hardware Back and nothing else. */
    @Volatile var current: BrowserPlugin? = null

    /** Whether the takeover is up: the hardware Back is the door. */
    fun takeoverUp(): Boolean = current?.engine?.takeoverUp() == true

    /** Lower the takeover from the activity's Back; true when it consumed it. */
    fun lowerTakeover(): Boolean = current?.engine?.lowerFromBar() ?: false

    /** A renderer death claimed on either WebView; see `rendererGone`. */
    fun rendererDied() {
      current?.engine?.rendererGone()
    }
  }

  private val engine = BrowserEngine(activity)

  init {
    current = this
  }

  /**
   * Bring the engine up and start the driver under it. The shell has already
   * bound the MCP socket it wants the driver to dial; everything the engine
   * needs beyond that (the relay, the WebView, the unpacked tree) is this
   * side's, so the reply carries only what the shell pins tabs with.
   */
  @Command
  fun startDriver(invoke: Invoke) {
    val args = invoke.parseArgs(StartDriverArgs::class.java)
    Thread {
      try {
        engine.ensure()
        engine.unpackAssets()
        val nodeStarted = engine.startDriver(args.socketPath, args.outputDir)
        val viewport = engine.viewport()
        invoke.resolve(
          JSObject()
            .put("relayPort", engine.relayPort)
            .put("uiOrigin", engine.uiOrigin())
            .put("viewportWidth", if (viewport.size == 2) viewport[0] else 0)
            .put("viewportHeight", if (viewport.size == 2) viewport[1] else 0)
            // **Whether THIS call launched node**, so the shell can size its
            // accept window honestly: a cold boot takes seconds (measured
            // 8 s warm, 39 s loaded - the shell's cold window is 40 s), while
            // an already-running node redials every second and gets 6 s - a
            // later call that paid the cold window for a dead host would
            // wait 40 s for an answer that cannot come.
            .put("nodeStarted", nodeStarted),
        )
      } catch (err: Exception) {
        invoke.reject(err.message ?: err.toString())
      }
    }.start()
  }

  /**
   * Bring the engine up without a driver: the app's own start, so the
   * WebView's devtools socket and the relay exist before anything asks.
   */
  @Command
  fun ensureEngine(invoke: Invoke) {
    Thread {
      try {
        engine.ensure()
        engine.unpackAssets()
        invoke.resolve()
      } catch (err: Exception) {
        invoke.reject(err.message ?: err.toString())
      }
    }.start()
  }

  /** Raise the takeover: the hand-off's Open. */
  @Command
  fun show(invoke: Invoke) {
    val done = engine.raise()
    if (done == null) invoke.resolve() else invoke.reject(done)
  }

  /** Lower the takeover: the hand-off's Done/Not now. */
  @Command
  fun hide(invoke: Invoke) {
    engine.lower()
    invoke.resolve()
  }

  /** Whether the takeover is up, for the strip's show/hide button. */
  @Command
  fun windowed(invoke: Invoke) {
    invoke.resolve(JSObject().put("windowed", engine.takeoverUp()))
  }

  /**
   * The engine's generation, read PER CALL by the shell: a claimed renderer
   * death bumps it, and the shell's driver identity follows it - a mirror
   * written only inside a rebuild could never notice the death (the
   * rebuild is a response to the identity, so it can never be its source).
   */
  @Command
  fun engineGeneration(invoke: Invoke) {
    invoke.resolve(JSObject().put("generation", engine.generation))
  }
}

/**
 * The engine: one browser WebView, one relay, one in-app node - the shared
 * profile, which is every profile the phone hosts until WebView storage
 * isolation gets its own piece.
 */
internal class BrowserEngine(private val activity: Activity) {
  @Volatile var relayPort: Int = 0
    private set
  private val token: String = randomToken()
  @Volatile private var webview: WebView? = null
  private var overlay: FrameLayout? = null
  /** Where the offscreen webview lives while no takeover is up: an attached,
   *  INVISIBLE frame - see `createWebView` for why a size is load-bearing. */
  private var holder: FrameLayout? = null
  @Volatile private var takeover = false
  @Volatile private var nodeUp = false

  /**
   * Bumped whenever a renderer death is claimed (the engine's own handler
   * and wry's client for the UI page both land in `rendererGone`). The
   * shell reads it PER CALL and folds it into the driver identity, so a
   * death makes the held driver read stale and the next call ATTEMPTS a
   * rebuild. It cannot reattach today (issue #1931): the in-app node keeps
   * its old socket link and this engine refuses a second node, so the call
   * fails with the reason until the app restarts.
   */
  @Volatile var generation = 0
    private set

  fun takeoverUp(): Boolean = takeover

  /**
   * The WebView and the relay, once each. Called before anything acts; safe
   * to call again.
   */
  @Synchronized
  fun ensure() {
    if (relayPort == 0) startRelay()
    activity.runOnUiThread {
      if (webview == null) webview = createWebView()
    }
    // The WebView must exist before the driver dials the relay; the create
    // above is quick, but a tool call racing it would relay to a socket that
    // is not there yet.
    var waited = 0
    while (webview == null && waited < 5000) {
      Thread.sleep(20)
      waited += 20
    }
  }

  /**
   * Unpack the driver tree and the bootstrap under filesDir. The marker's
   * version must be bumped whenever the payload or the vendored driver
   * changes: an APK update reinstalls the assets but keeps filesDir, so a
   * stale unpack otherwise survives forever.
   */
  fun unpackAssets() {
    val dest = File(activity.filesDir, "browser/engine")
    val marker = File(dest, ".unpacked-3")
    val entry = File(dest, "payload/bootstrap.js")
    val cli = File(dest, "playwright-mcp/node_modules/@playwright/mcp/cli.js")
    if (marker.exists() && entry.isFile && cli.isFile) {
      return
    }
    Log.i(TAG, "unpacking the engine tree into $dest")
    val started = System.currentTimeMillis()
    findDelete(dest)
    copyAssetDir("forge-browser", File(dest, "payload"))
    copyAssetDir("browser-stack/playwright-mcp", File(dest, "playwright-mcp"))
    marker.writeText("ok")
    Log.i(TAG, "engine tree unpacked in ${System.currentTimeMillis() - started}ms")
  }

  /**
   * Start libnode on the bootstrap. The driver talks MCP to the shell over
   * the unix socket it dials, and CDP to this side's relay; the argv carries
   * both plus the token the relay gates on. **Answers whether THIS call
   * launched node** - see the reply's `nodeStarted` where it is built.
   */
  fun startDriver(socketPath: String, outputDir: String): Boolean {
    val engineDir = File(activity.filesDir, "browser/engine")
    val argv =
      arrayOf(
        "node",
        File(engineDir, "payload/bootstrap.js").absolutePath,
        "--mcp",
        File(engineDir, "playwright-mcp/node_modules/@playwright/mcp/cli.js").absolutePath,
        "--mcp-socket",
        socketPath,
        "--cdp",
        "http://127.0.0.1:$relayPort",
        "--cdp-token",
        token,
        "--output",
        outputDir,
      )
    // **One node per app run.** Android has no second node binary to spawn
    // and libnode's `node::Start` is not made for a second call in one
    // process; the driver's transport is bound once, so a later start reuses
    // the running node rather than starting another. A node that died is the
    // app's to repair by restart (the shell's calls say so meanwhile).
    if (nodeUp) {
      Log.i(TAG, "in-app node is already up; not starting another")
      return false
    }
    nodeUp = true
    Log.i(TAG, "starting in-app node (relay $relayPort)")
    Thread({ NodeHost.startNode(argv, engineDir.absolutePath) }, "forge-node").start()
    return true
  }

  /**
   * The engine's real viewport, read on the UI thread (the same rule as
   * `uiOrigin`). The shell seeds the driver's context with it: an invisible
   * WebView lays its page out, but playwright's own geometry bookkeeping
   * starts at nothing, and click/screenshot checks run against IT (measured:
   * "element is outside of the viewport" while the page's own innerWidth
   * read 980).
   */
  fun viewport(): IntArray {
    val page = webview ?: return IntArray(0)
    var width = 0
    var height = 0
    val latch = java.util.concurrent.CountDownLatch(1)
    activity.runOnUiThread {
      width = page.width
      height = page.height
      latch.countDown()
    }
    try {
      latch.await(2, java.util.concurrent.TimeUnit.SECONDS)
    } catch (ignored: InterruptedException) {
      // an empty pair answers below
    }
    return intArrayOf(width, height)
  }

  /**
   * The client UI's origin, which the shell's tab pin and guard compare
   * against. **Read on the UI thread**: every WebView method asserts its
   * thread, and this runs on the driver-start thread - reading `url` here
   * threw, which rejected the whole start (measured live: the shell's socket
   * was gone by the time the driver dialed it).
   */
  fun uiOrigin(): String {
    val page = MainActivity.clientWebView ?: return ""
    var url = ""
    val latch = java.util.concurrent.CountDownLatch(1)
    activity.runOnUiThread {
      url = page.url ?: ""
      latch.countDown()
    }
    try {
      latch.await(2, java.util.concurrent.TimeUnit.SECONDS)
    } catch (ignored: InterruptedException) {
      // the read below answers "" on an empty url
    }
    val parsed = android.net.Uri.parse(url)
    val scheme = parsed.scheme ?: return ""
    val authority = parsed.authority ?: return ""
    return "$scheme://$authority"
  }

  /** Raise the takeover over the client. Null on success, a reason otherwise. */
  fun raise(): String? {
    val page = webview ?: return "the browser engine is not up"
    activity.runOnUiThread {
      val root = activity.findViewById<ViewGroup>(android.R.id.content)
      val view = overlay ?: buildOverlay().also { overlay = it }
      // **The page moves into the takeover** - it lives in the invisible
      // holder otherwise, and a View cannot have two parents.
      (page.parent as? ViewGroup)?.removeView(page)
      val column = view.getChildAt(0) as? LinearLayout
      if (column != null && page.parent == null) {
        page.layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f)
        column.addView(page)
      }
      if (view.parent == null) {
        root.addView(view, ViewGroup.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
      }
      takeover = true
      // The strip's row re-reads on this word: the raise's UI body runs on
      // the UI thread, so a caller's own read can land before it - the
      // event is what makes the late state visible (measured).
      notifyJs("raised")
      Log.i(TAG, "takeover raised")
    }
    return null
  }

  /** Lower the takeover. Idempotent; every path says `lowered`, the bar's Done adds `done`. */
  fun lower() {
    activity.runOnUiThread {
      val page = webview
      // **The page moves back to the invisible holder**, or the next raise
      // re-parents a view whose frame just died with the overlay.
      if (page != null) {
        (page.parent as? ViewGroup)?.removeView(page)
      }
      overlay?.let { over ->
        (over.parent as? ViewGroup)?.removeView(over)
      }
      if (page != null && holder != null) {
        holder?.addView(
          page,
          ViewGroup.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT),
        )
        holder?.visibility = android.view.View.INVISIBLE
      }
      takeover = false
      notifyJs("lowered")
      Log.i(TAG, "takeover lowered")
    }
  }

  /**
   * A renderer death was claimed on either WebView (the engine's own client
   * or wry's for the UI page). One renderer serves every WebView in the
   * process, so either death may be THIS page's; the repair is one:
   * bump the generation so the shell's next call sees a stale driver (see
   * `generation` for what that rebuild can and cannot reach), lower a
   * raised takeover, and drop the dead view.
   */
  fun rendererGone() {
    // Non-atomic on a @Volatile, safe because both claimers run on the UI
    // thread; a shared-renderer death lands here twice, so the value may
    // jump by 2. Nothing rides on the exact value, only on its changing.
    generation += 1
    if (takeover) {
      lower()
    }
    if (webview != null) {
      val page = webview
      (page?.parent as? ViewGroup)?.removeView(page)
      page?.destroy()
      webview = null
    }
    Log.w(TAG, "renderer death handled; engine generation now $generation")
  }

  /**
   * The bar's back and the hardware Back share this door, `lower()`'s
   * `lowered` included - a lower that sent no word left the collapsed row
   * saying "hide" over a window already down (measured).
   */
  fun lowerFromBar(): Boolean {
    if (!takeover) {
      return false
    }
    lower()
    return true
  }

  // --- the WebView ----------------------------------------------------------

  private fun createWebView(): WebView {
    // Process-wide, and the engine is CDP-driven: without it the browser
    // page has no devtools target at all in a release build.
    WebView.setWebContentsDebuggingEnabled(true)
    val view = WebView(activity)
    with(view.settings) {
      javaScriptEnabled = true
      domStorageEnabled = true
      // The engine drives real sites; a page that never gets a real
      // user-agent besides its default is fine, so just leave the rest.
      loadWithOverviewMode = true
      useWideViewPort = true
      cacheMode = WebSettings.LOAD_DEFAULT
    }
    // **A dead renderer must be a dead PAGE, not a dead app.** The engine
    // renders arbitrary agent-directed pages (a heavy page's OOM, a
    // chrome://crash), all WebViews share the renderer, and Android's
    // default on an unhandled render-process-gone is to kill the app
    // process. Claiming the callback, dropping the view and letting the
    // next `ensure` build a fresh one turns it into a failed tool call.
    view.webViewClient =
      object : android.webkit.WebViewClient() {
        override fun onRenderProcessGone(
          view: WebView,
          detail: android.webkit.RenderProcessGoneDetail?,
        ): Boolean {
          Log.w(TAG, "the browser page's renderer died (crashed=${detail?.didCrash()})")
          rendererGone()
          return true
        }
      }
    // Hold JS dialogs open: the default WebView cancels an alert the same
    // millisecond CDP announces it, so a CDP client could never accept one
    // (measured). Resolving happens through the devtools session.
    view.webChromeClient =
      object : WebChromeClient() {
        override fun onJsAlert(view: WebView, url: String, message: String, result: JsResult): Boolean {
          Log.i(TAG, "js dialog alert held")
          return true
        }

        override fun onJsConfirm(view: WebView, url: String, message: String, result: JsResult): Boolean {
          Log.i(TAG, "js dialog confirm held")
          return true
        }

        override fun onJsPrompt(
          view: WebView,
          url: String,
          message: String,
          defaultValue: String,
          result: JsPromptResult,
        ): Boolean {
          Log.i(TAG, "js dialog prompt held")
          return true
        }
      }
    // **A navigation AND a frame, or the page is inert.** Two measured
    // facts, both fatal alone: a WebView created and left untouched has no
    // renderer (every renderer-scoped CDP command hangs and playwright's
    // connectOverCDP times out), and a WebView with no layout pass has a
    // ZERO-SIZED viewport - navigation still works (a URL needs no
    // geometry) while click points, scrolling and screenshots all fail
    // ("element is outside of the viewport", "Cannot take screenshot with
    // 0 width"). So: navigate, and attach the view INVISIBLE at real size
    // while no takeover is up; the renderer runs, the page lays out, and
    // the person sees it only when a takeover raises it.
    view.loadUrl("about:blank")
    // createWebView runs on the UI thread (see `ensure`), so the attach is
    // direct.
    val root = activity.findViewById<ViewGroup>(android.R.id.content)
    if (holder == null) {
      holder = FrameLayout(activity)
      root.addView(
        holder,
        ViewGroup.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT),
      )
    }
    holder?.addView(
      view,
      ViewGroup.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT),
    )
    holder?.visibility = android.view.View.INVISIBLE
    Log.i(TAG, "browser webview created (invisible frame, about:blank)")
    return view
  }

  /** The takeover frame: the slim bar and a column the page is moved into
   *  (by `raise`, so the overlay itself holds no page). */
  private fun buildOverlay(): FrameLayout {
    val frame = FrameLayout(activity)
    val column = LinearLayout(activity)
    column.orientation = LinearLayout.VERTICAL
    val bar = LinearLayout(activity)
    bar.orientation = LinearLayout.HORIZONTAL
    bar.gravity = Gravity.CENTER_VERTICAL
    bar.setBackgroundColor(Color.rgb(24, 24, 27))
    bar.setPadding(dp(6), dp(4), dp(6), dp(4))
    val back =
      Button(activity).apply {
        text = "← forge"
        minHeight = dp(44)
        minWidth = dp(96)
        setOnClickListener {
          lower()
        }
      }
    val spacer = android.view.View(activity).apply { layoutParams = LinearLayout.LayoutParams(0, 1, 1f) }
    val done =
      Button(activity).apply {
        text = "Done"
        minHeight = dp(44)
        minWidth = dp(88)
        setOnClickListener {
          lower()
          notifyJs("done")
        }
      }
    bar.addView(back)
    bar.addView(spacer)
    bar.addView(done)
    column.addView(bar)
    frame.addView(column, ViewGroup.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
    return frame
  }

  private fun dp(value: Int): Int = (value * activity.resources.displayMetrics.density).toInt()

  // --- the CDP relay --------------------------------------------------------

  /**
   * Loopback TCP, ephemeral port: playwright speaks HTTP and websockets only,
   * so this is the one hop that must be TCP. **Every request is gated**
   * (`relayAllows`): the pinned driver sends its `--cdp-header` token on the
   * discovery fetch and the WebSocket upgrade alike, so another app on the
   * device cannot even open the socket. Loopback alone would leave any of
   * them able to drive the WebView.
   */
  private fun startRelay() {
    val server = ServerSocket(0, 8, InetAddress.getByName("127.0.0.1"))
    relayPort = server.localPort
    Thread(
      {
        while (true) {
          val client = server.accept()
          Thread({ relayConnection(client) }, "cdp-relay").start()
        }
      },
      "cdp-relay-accept",
    ).start()
  }

  private fun relayConnection(client: Socket) {
    val sockName = "webview_devtools_remote_" + Process.myPid()
    try {
      val input = BufferedInputStream(client.getInputStream())
      val head = readHead(input) ?: run {
        client.close()
        return
      }
      val headText = String(head, Charsets.ISO_8859_1)
      Log.i(TAG, "relay request: " + headText.lineSequence().firstOrNull()?.take(90) + " hdr=" + (headText.contains("X-Forge-Token: ", ignoreCase = true)))
      if (!relayAllows(headText, token)) {
        Log.w(TAG, "relay refused an ungated request")
        val out = client.getOutputStream()
        out.write("HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n".toByteArray())
        out.flush()
        client.close()
        return
      }
      val target = LocalSocket()
      target.connect(LocalSocketAddress(sockName, LocalSocketAddress.Namespace.ABSTRACT))
      val toTarget: OutputStream = target.outputStream
      toTarget.write(head)
      toTarget.flush()
      val fromTarget: InputStream = target.inputStream
      val clientOut: OutputStream = client.outputStream
      val up =
        Thread(
          {
            pump(fromTarget, clientOut)
            quietClose(target)
            quietClose(client)
          },
          "cdp-up",
        )
      val down =
        Thread(
          {
            pump(input, toTarget)
            quietClose(client)
            quietClose(target)
          },
          "cdp-down",
        )
      up.start()
      down.start()
    } catch (err: Exception) {
      Log.w(TAG, "relay connection failed: ${err.message}")
      quietClose(client)
    }
  }

  /** The request head up to the blank line, or null when the peer said nothing. */
  private fun readHead(input: InputStream): ByteArray? {
    val buffer = java.io.ByteArrayOutputStream()
    var state = 0
    while (buffer.size() < 65536) {
      val b = input.read()
      if (b < 0) {
        return if (buffer.size() == 0) null else buffer.toByteArray()
      }
      buffer.write(b)
      state =
        when {
          (state == 0 || state == 2) && b == '\r'.code -> state + 1
          (state == 1 || state == 3) && b == '\n'.code -> state + 1
          b == '\r'.code -> 1
          else -> 0
        }
      if (state == 4) {
        return buffer.toByteArray()
      }
    }
    return buffer.toByteArray()
  }

  private fun pump(from: InputStream, to: OutputStream) {
    val buffer = ByteArray(1 shl 16)
    try {
      while (true) {
        val n = from.read(buffer)
        if (n < 0) break
        to.write(buffer, 0, n)
        to.flush()
      }
    } catch (ignored: Exception) {
      // a closed peer ends the pump
    }
  }

  private fun quietClose(closeable: java.io.Closeable?) {
    try {
      closeable?.close()
    } catch (ignored: Exception) {
      // closing anyway
    }
  }

  // --- assets ---------------------------------------------------------------

  private fun copyAssetDir(assetPath: String, dest: File) {
    val children = activity.assets.list(assetPath)
    if (children != null && children.isNotEmpty()) {
      for (child in children) {
        copyAssetDir("$assetPath/$child", File(dest, child))
      }
      return
    }
    dest.parentFile?.mkdirs()
    activity.assets.open(assetPath).use { input ->
      dest.outputStream().use { output ->
        input.copyTo(output)
      }
    }
  }

  private fun findDelete(dir: File) {
    val children = dir.listFiles() ?: return
    for (child in children) {
      if (child.isDirectory) findDelete(child)
      child.delete()
    }
  }

  private fun notifyJs(what: String) {
    // **Whitelisted, because the argument is interpolated into a JS string**
    // - every call site is already a literal, and this keeps it that way.
    if (what != "raised" && what != "lowered" && what != "done") {
      Log.w(TAG, "notifyJs refused an unknown word: " + what.take(24))
      return
    }
    val page = MainActivity.clientWebView ?: return
    activity.runOnUiThread {
      page.evaluateJavascript("window.__forgeTakeover && window.__forgeTakeover('$what')", null)
    }
  }

  private fun randomToken(): String {
    val bytes = ByteArray(24)
    SecureRandom().nextBytes(bytes)
    return bytes.joinToString("") { "%02x".format(it) }
  }
}
