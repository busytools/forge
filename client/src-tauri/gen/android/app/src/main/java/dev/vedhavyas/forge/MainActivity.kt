package dev.vedhavyas.forge

import android.os.Bundle
import android.webkit.WebView
import androidx.activity.enableEdgeToEdge

class MainActivity : TauriActivity() {
  // TauriActivity turns wry's back handling off; the client's pages are on
  // history, so the hardware Back walks them rather than finishing the app.
  override val handleBackNavigation: Boolean = true

  private var webView: WebView? = null

  override fun onWebViewCreate(webView: WebView) {
    this.webView = webView
  }

  // The WebView's own back API is inert for this client - its links are
  // pushState entries, and canGoBack() reads false while goBack() does
  // nothing - so the page's Navigation API takes the step, and the activity
  // only decides whether there was one. A WebView without that API exits.
  @Suppress("DEPRECATION")
  override fun onBackPressed() {
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
