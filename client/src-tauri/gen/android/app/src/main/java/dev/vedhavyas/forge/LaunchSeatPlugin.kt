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
    val intent = MainActivity.newest ?: activity.intent
    val id = tapSeatId(intent?.action, notificationId(intent), pressAction(intent))
    if (id == null) {
      invoke.resolve()
      return
    }
    // **The activity launch goes with the id**: a page reload in the same
    // activity is not a launch, but a re-created activity asks again.
    invoke.resolveObject(JSObject().put("id", id).put("launch", MainActivity.launches))
  }

  /**
   * **A tap into a running app arrives here**: the activity is reused and the
   * intent does not replace `activity.intent` on its own. The id goes to the
   * shell, which opens the seat the same way the page asks for it after a cold
   * start.
   */
  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    val id = tapSeatId(intent.action, notificationId(intent), pressAction(intent)) ?: return
    trigger("tap", JSObject().put("id", id))
  }

  private fun notificationId(intent: Intent?): Int =
    intent?.getIntExtra(NOTIFICATION_INTENT_KEY, Int.MIN_VALUE) ?: Int.MIN_VALUE

  private fun pressAction(intent: Intent?): String? = intent?.getStringExtra(ACTION_INTENT_KEY)

  companion object {
    /**
     * The notification a tap opened, when this intent is that tap.
     *
     * **Pure on purpose**, so the predicate deciding what counts as a tap is
     * pinned by a JVM test rather than only exercised on a device: only the
     * notification plugin's own press action does, and a launcher start
     * carries neither an id nor that action.
     */
    internal fun tapSeatId(action: String?, id: Int, press: String?): Int? {
      if (action != Intent.ACTION_MAIN || id == Int.MIN_VALUE || press != DEFAULT_PRESS_ACTION) {
        return null
      }
      return id
    }
  }
}
