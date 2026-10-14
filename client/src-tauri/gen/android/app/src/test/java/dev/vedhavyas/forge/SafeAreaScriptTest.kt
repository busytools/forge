package dev.vedhavyas.forge

import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The insets hook the page's `--safe-*` reads rest on: the property names and
 * the px-to-CSS-px conversion are a string decision, so they are pinned here
 * rather than only exercised on a device.
 */
class SafeAreaScriptTest {
  @Test
  fun `writes the four bands in CSS pixels`() {
    val script =
      MainActivity.safeAreaScript(top = 96, right = 0, bottom = 66, left = 24, density = 3f)
    assertTrue(script, script.contains("'--safe-top':'32.0px',"))
    assertTrue(script, script.contains("'--safe-right':'0.0px',"))
    assertTrue(script, script.contains("'--safe-bottom':'22.0px',"))
    assertTrue(script, script.contains("'--safe-left':'8.0px',"))
  }

  @Test
  fun `waits for the document when the root is not there yet`() {
    val script =
      MainActivity.safeAreaScript(top = 0, right = 0, bottom = 0, left = 0, density = 1f)
    assertTrue(
      script,
      script.contains("document.addEventListener('DOMContentLoaded', apply"),
    )
  }
}
