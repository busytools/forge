package dev.vedhavyas.forge

import android.app.Activity
import android.content.Context
import android.media.AudioManager
import android.os.Build
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

/**
 * What the platform is doing to this app's microphone, for the row that has to
 * name a capture the device is feeding nothing.
 *
 * Two facts, and only the ones the public API gives an app on this target: the
 * device's own mute, and whether the platform is silencing THIS app's capture
 * (a per-app mic mode, the privacy toggle - `isClientSilenced` is the effect
 * itself). The active configurations an app sees from API 29 on are its OWN,
 * so this is the capture being asked about; another app's recordings are not
 * visible at all, which is why the row says "another app" only as a thing to
 * check and never as a cause read from here.
 */
@TauriPlugin
class MicStatePlugin(private val activity: Activity) : Plugin(activity) {
  @Command
  fun micState(invoke: Invoke) {
    // **Off the main thread**: the configuration read is a binder call into
    // audioserver, and Tauri runs a `@Command` on the main thread - a wedged
    // audio service would park the UI behind a row's own sentence.
    Thread {
      try {
        val audio = activity.getSystemService(Context.AUDIO_SERVICE) as? AudioManager
        var silenced = false
        if (audio != null && Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
          for (config in audio.activeRecordingConfigurations) {
            if (config.isClientSilenced) silenced = true
          }
        }
        invoke.resolve(
          JSObject().put("muted", audio?.isMicrophoneMute ?: false).put("silenced", silenced)
        )
      } catch (err: Exception) {
        invoke.reject(err.message ?: err.toString())
      }
    }.start()
  }
}
