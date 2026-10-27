package dev.vedhavyas.forge

import android.content.Intent
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The one predicate that decides whether a launch is a notification tap: its
 * action, its id, and the plugin's own press action. Pinned here rather than
 * only exercised on a device.
 */
class LaunchSeatTest {
  @Test
  fun `a tap names its notification`() {
    assertTrue(
      "a tap did not name its notification",
      LaunchSeatPlugin.tapSeatId(Intent.ACTION_MAIN, 7, "tap") == 7,
    )
  }

  @Test
  fun `a launcher start names nothing`() {
    assertNull(
      "a launcher start named a notification",
      LaunchSeatPlugin.tapSeatId(Intent.ACTION_MAIN, Int.MIN_VALUE, null),
    )
  }

  @Test
  fun `another action names nothing`() {
    assertNull(
      "an action that is not a launch named a notification",
      LaunchSeatPlugin.tapSeatId(Intent.ACTION_VIEW, 7, "tap"),
    )
  }

  @Test
  fun `another press action names nothing`() {
    assertNull(
      "a press that is not a tap named a notification",
      LaunchSeatPlugin.tapSeatId(Intent.ACTION_MAIN, 7, "dismiss"),
    )
  }
}
