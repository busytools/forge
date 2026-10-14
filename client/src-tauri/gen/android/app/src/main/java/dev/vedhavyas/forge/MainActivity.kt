package dev.vedhavyas.forge

import android.Manifest
import android.content.pm.PackageManager
import android.graphics.Color
import android.os.Bundle
import android.webkit.WebView
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.webkit.ScriptHandler
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature

class MainActivity : TauriActivity() {
  // TauriActivity turns wry's back handling off; the client's pages are on
  // history, so the hardware Back walks them rather than finishing the app.
  override val handleBackNavigation: Boolean = true

  private var webView: WebView? = null

  /** The page's own hook for the insets, re-registered every time they move. */
  private var safeArea: ScriptHandler? = null

  /** The insets last read, so a page created after them is still handed the current ones. */
  private var lastInsets: WindowInsetsCompat? = null

  /** The lengths last written, so the many dispatches an IME animation
   *  produces do not re-register the hook for an unchanged script. Cleared
   *  when a webview is created, because the replay IS for the new document. */
  private var lastScript: String? = null

  companion object {
    /** The microphone request's own code, for the answer that comes back. */
    private const val MIC_REQUEST = 1

    // The page the client draws, for the browser engine: hardware Back asks
    // the takeover first, and the engine reports the UI's origin so the shell
    // can keep the driver off this page (its devtools target is one of many
    // in this process, and the first playwright finds).
    @Volatile var clientWebView: WebView? = null

    /**
     * The page's own hook, as the string the document runs: the four lengths
     * in CSS pixels, which is the unit the sheet's properties are in.
     *
     * **Pure on purpose.** The property names and the px-to-CSS-px conversion
     * are the contract the sheet's `--safe-*` reads rest on, so they are
     * pinned by a JVM test rather than only exercised on a device.
     */
    internal fun safeAreaScript(
      top: Int,
      right: Int,
      bottom: Int,
      left: Int,
      density: Float,
    ): String {
      val lengths =
        listOf(
          "--safe-top" to top,
          "--safe-right" to right,
          "--safe-bottom" to bottom,
          "--safe-left" to left,
        ).joinToString("") { (name, px) -> "'$name':'${px / density}px'," }
      // A document-start script can run before the root element exists; the
      // listener's own pass then waits for the document rather than dropping
      // the lengths on the floor.
      return """
        (() => {
          const apply = () => {
            const root = document.documentElement;
            if (!root) return false;
            for (const [name, value] of Object.entries({$lengths})) {
              root.style.setProperty(name, value);
            }
            return true;
          };
          if (!apply()) document.addEventListener('DOMContentLoaded', apply, { once: true });
        })();
      """.trimIndent()
    }
  }

  override fun onCreate(savedInstanceState: Bundle?) {
    // The page is dark in every configuration, so the bars' icons are pinned
    // light-on-dark rather than left to `auto`, which reads the SYSTEM theme:
    // dark icons over a near-black page is the "the status bar reads as
    // missing" report this closes.
    enableEdgeToEdge(
      statusBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
      navigationBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
    )
    super.onCreate(savedInstanceState)
    // Android grants the microphone at runtime; the manifest's declaration
    // only makes it askable. Both capture legs need it - the shell's own
    // device, and the webview's stream behind getUserMedia - and neither can
    // open anything until the person has said yes.
    if (
      checkSelfPermission(Manifest.permission.RECORD_AUDIO) !=
        PackageManager.PERMISSION_GRANTED
    ) {
      requestPermissions(arrayOf(Manifest.permission.RECORD_AUDIO), MIC_REQUEST)
    }
    // The window's insets, which `enableEdgeToEdge` above is what takes away
    // from the page. The decor view is listened to rather than the webview:
    // the insets are handed back untouched, so the webview keeps its own
    // handling under it.
    ViewCompat.setOnApplyWindowInsetsListener(window.decorView) { _, insets ->
      handSafeArea(insets)
      insets
    }
  }

  override fun onWebViewCreate(webView: WebView) {
    this.webView = webView
    clientWebView = webView
    // An insets dispatch that landed before this webview existed is replayed,
    // so the page is never left on the sheet's zero fallback.
    lastScript = null
    lastInsets?.let { handSafeArea(it) }
  }

  /**
   * The system bars' own thickness, onto the page's root element.
   *
   * The page draws edge-to-edge, so its edges are under the status bar and
   * the gesture bar, and the sheet pads every page root and its own overlays
   * by the four lengths it reads as `--safe-*`. `env(safe-area-inset-*)` is
   * the same four numbers, but this app's WebView answers zero for them
   * (measured; Chromium forwards them only in later releases), so the shell
   * measures the window's insets and hands them in itself. The
   * document-start hook is what survives the page's load: an inline style
   * written into a document that is about to be replaced goes with it.
   */
  private fun handSafeArea(insets: WindowInsetsCompat) {
    lastInsets = insets
    val page = webView ?: return
    val bars =
      insets.getInsets(
        WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout(),
      )
    // With the keyboard up the app's bottom edge is the keyboard's top, while
    // the nav band is still "visible" at the screen's bottom - padding by it
    // would open a gap above the composer, so the bottom reports zero then.
    val bottom = if (insets.isVisible(WindowInsetsCompat.Type.ime())) 0 else bars.bottom
    val script =
      safeAreaScript(bars.top, bars.right, bottom, bars.left, resources.displayMetrics.density)
    if (script == lastScript) return
    lastScript = script
    safeArea?.remove()
    safeArea =
      if (WebViewFeature.isFeatureSupported(WebViewFeature.DOCUMENT_START_SCRIPT)) {
        WebViewCompat.addDocumentStartJavaScript(page, script, setOf("*"))
      } else {
        null
      }
    page.evaluateJavascript(script, null)
  }

  // The WebView's own back API is inert for this client - its links are
  // pushState entries, and canGoBack() reads false while goBack() does
  // nothing - so the page's Navigation API takes the step, and the activity
  // only decides whether there was one. A WebView without that API exits.
  //
  // **The takeover is asked first** (the approved Android pair): while it is
  // up the Back is its door - the page and the client stay exactly where they
  // were - and only after it lowers does the page walk its own history.
  @Suppress("DEPRECATION")
  override fun onBackPressed() {
    if (BrowserPlugin.takeoverUp()) {
      BrowserPlugin.lowerTakeover()
      return
    }
    val page = webView
    if (page == null) {
      finish()
      return
    }
    page.evaluateJavascript(
      "window.navigation ? (navigation.canGoBack ? (navigation.back(), 'back') : 'root') : 'legacy'",
    ) { answer ->
      if (answer != "\"back\"") finish()
    }
  }
}
