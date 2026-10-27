package dev.vedhavyas.forge

import android.app.Activity
import android.content.Intent
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.notification.ACTION_INTENT_KEY
import app.tauri.notification.DEFAULT_PRESS_ACTION
import app.tauri.notification.NOTIFICATION_INTENT_KEY
import app.tauri.plugin.Invoke
import app.tauri.plugin.Plugin

/**
 * The notification this activity was launched from.
 *
 * The notification plugin reads its launch intent while the plugins
 * initialize, before this app's click listener is registered and before there
 * is a page to receive it, so a tap that started the app is dropped. The
 * intent stays on the activity, and its notification id is the one the shell
 * remembered the seat under; this answers that id to the page that asks.
 */
@TauriPlugin
class LaunchSeatPlugin(private val activity: Activity) : Plugin(activity) {
  @Command
  fun tapped(invoke: Invoke) {
    val intent: Intent? = activity.intent
    val id = intent?.getIntExtra(NOTIFICATION_INTENT_KEY, Int.MIN_VALUE) ?: Int.MIN_VALUE
    // Only the notification plugin's own tap intent counts: a launcher start
    // carries neither, and a stale id must not open an old seat.
    if (intent?.action != Intent.ACTION_MAIN ||
      id == Int.MIN_VALUE ||
      intent.getStringExtra(ACTION_INTENT_KEY) != DEFAULT_PRESS_ACTION
    ) {
      invoke.resolve()
      return
    }
    invoke.resolveObject(id)
  }
}
