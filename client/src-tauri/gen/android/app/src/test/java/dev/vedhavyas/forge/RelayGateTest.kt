package dev.vedhavyas.forge

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The CDP relay's gate: the release's correctness rests on this string
 * decision, so it is pinned here rather than only exercised on a device.
 */
class RelayGateTest {
  private val token = "0123456789abcdef"

  @Test
  fun `an ungated discovery request is refused`() {
    val head = "GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:9334\r\n\r\n"
    assertFalse(relayAllows(head, token))
  }

  @Test
  fun `a tokened discovery request passes`() {
    val head = "GET /json/version/ HTTP/1.1\r\nHost: 127.0.0.1:9334\r\nX-Forge-Token: $token\r\n\r\n"
    assertTrue(relayAllows(head, token))
  }

  @Test
  fun `a wrong token is refused`() {
    val head = "GET /json/version HTTP/1.1\r\nX-Forge-Token: not-the-token\r\n\r\n"
    assertFalse(relayAllows(head, token))
  }

  // The driver sends the header on EVERY request, the WebSocket upgrade
  // included (measured against a fake devtools server), so the upgrade is
  // gated like everything else.
  @Test
  fun `the WebSocket upgrade is gated like everything else`() {
    val ungated = "GET /devtools/browser HTTP/1.1\r\nHost: 127.0.0.1:9334\r\nUpgrade: websocket\r\n\r\n"
    assertFalse(relayAllows(ungated, token))

    val gated = "GET /devtools/browser HTTP/1.1\r\nUpgrade: websocket\r\nX-Forge-Token: $token\r\n\r\n"
    assertTrue(relayAllows(gated, token))
  }

  @Test
  fun `the header name is case-insensitive`() {
    val head = "GET /json/list HTTP/1.1\r\nx-forge-token: $token\r\n\r\n"
    assertTrue(relayAllows(head, token))
  }
}
