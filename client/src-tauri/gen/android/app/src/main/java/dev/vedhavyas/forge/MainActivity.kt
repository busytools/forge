package dev.vedhavyas.forge

import android.Manifest
import android.content.pm.PackageManager
import android.os.Bundle
import android.webkit.WebView
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

  companion object {
    /** The microphone request's own code, for the answer that comes back. */
    private const val MIC_REQUEST = 1

    // The page the client draws, for the browser engine: hardware Back asks
    // the takeover first, and the engine reports the UI's origin so the shell
    // can keep the driver off this page (its devtools target is one of many
    // in this process, and the first playwright finds).
    @Volatile var clientWebView: WebView? = null
  }

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
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
    lastInsets?.let { handSafeArea(it) }
  }

  /**
   * The system bars' own thickness, onto the page's root element.
   *
   * The page draws edge-to-edge, so its edges are under the status bar and
   * the gesture bar, and the sheet pads every page root and its own overlays
   * by the four lengths it reads as `--safe-*`. `env(safe-area-inset-*)` is
   * the same four numbers, but Android's WebView answers zero for them before
   * WebView 144, so the shell measures the window's insets and hands them in
   * itself. The document-start hook is what survives the page's load: an
   * inline style written into a document that is about to be replaced goes
   * with it.
   */
  private fun handSafeArea(insets: WindowInsetsCompat) {
    lastInsets = insets
    val page = webView ?: return
    val script = safeAreaScript(insets)
    safeArea?.remove()
    safeArea =
      if (WebViewFeature.isFeatureSupported(WebViewFeature.DOCUMENT_START_SCRIPT)) {
        WebViewCompat.addDocumentStartJavaScript(page, script, setOf("*"))
      } else {
        null
      }
    page.evaluateJavascript(script, null)
  }

  /** The insets as CSS pixels, which is the unit the sheet's properties are in. */
  private fun safeAreaScript(insets: WindowInsetsCompat): String {
    val bars =
      insets.getInsets(
        WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout(),
      )
    val density = resources.displayMetrics.density
    val lengths =
      listOf(
        "--safe-top" to bars.top,
        "--safe-right" to bars.right,
        "--safe-bottom" to bars.bottom,
        "--safe-left" to bars.left,
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
