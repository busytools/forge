package dev.vedhavyas.forge

import android.Manifest
import android.content.pm.PackageManager
import android.os.Bundle
import android.webkit.WebView
import androidx.activity.enableEdgeToEdge

class MainActivity : TauriActivity() {
  // TauriActivity turns wry's back handling off; the client's pages are on
  // history, so the hardware Back walks them rather than finishing the app.
  override val handleBackNavigation: Boolean = true

  private var webView: WebView? = null

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
  }

  override fun onWebViewCreate(webView: WebView) {
    this.webView = webView
    clientWebView = webView
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
