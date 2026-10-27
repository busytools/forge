package dev.vedhavyas.forge

import android.app.Activity
import android.content.Intent
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.notification.ACTION_INTENT_KEY
import app.tauri.notification.DEFAULT_PRESS_ACTION
import app.tauri.notification.NOTIFICATION_INTENT_KEY
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

/**
 * The notification a launch or a tap names.
 *
 * The notification plugin reads its launch intent while the plugins
 * initialize, before this app's click listener is registered and before there
 * is a page to receive it, so a tap that started the app is dropped. The
 * intent stays on the activity, and its notification id is the one the shell
 * remembered the seat under; this answers it, and reports the intents that
 * arrive while the app is already running.
 */
@TauriPlugin
class LaunchSeatPlugin(private val activity: Activity) : Plugin(activity) {
  @Command
  fun tapped(invoke: Invoke) {
    val id = tapId(activity.intent)
    if (id == null) invoke.resolve() else invoke.resolveObject(id)
  }

  /**
   * **A tap into a running app arrives here**: the activity is reused and the
   * intent does not replace `activity.intent` on its own. The id goes to the
   * shell, which opens the seat the same way the page asks for it after a cold
   * start.
   */
  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    val id = tapId(intent) ?: return
    trigger("tap", JSObject().put("id", id))
  }

  /** The notification a tap opened, when this intent is that tap. */
  private fun tapId(intent: Intent?): Int? {
    val id = intent?.getIntExtra(NOTIFICATION_INTENT_KEY, Int.MIN_VALUE) ?: Int.MIN_VALUE
    // Only the notification plugin's own tap intent counts: a launcher start
    // carries neither, and a stale id must not open an old seat.
    if (intent?.action != Intent.ACTION_MAIN ||
      id == Int.MIN_VALUE ||
      intent.getStringExtra(ACTION_INTENT_KEY) != DEFAULT_PRESS_ACTION
    ) {
      return null
    }
    return id
  }
}
