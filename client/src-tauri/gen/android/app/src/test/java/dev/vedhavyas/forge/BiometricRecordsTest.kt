package dev.vedhavyas.forge

import androidx.biometric.BiometricManager
import androidx.biometric.BiometricPrompt
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Test

/**
 * The phone gate's pure half: the stored record's codec and the words a
 * platform answer becomes.
 *
 * **What a JVM test cannot reach is the Keystore and the prompt** - both need
 * a device and a finger - so the record is the piece this pins: a restart
 * reads it back, and a record this build cannot parse falls back to the
 * password rather than to a decrypt of nonsense.
 */
class BiometricRecordsTest {
  @Test
  fun theRecordRoundTripsItsIvAndCiphertext() {
    val iv = byteArrayOf(1, 2, 3, 4)
    val ciphertext = byteArrayOf(9, 8, 7, 6, 5)
    val record = BiometricRecords.encode(iv, ciphertext)

    val parts = BiometricRecords.decode(record)

    assertArrayEquals(iv, parts?.first)
    assertArrayEquals(ciphertext, parts?.second)
  }

  @Test
  fun aRecordWithoutBothPartsIsRefused() {
    assertNull(BiometricRecords.decode(""))
    assertNull(BiometricRecords.decode("abc"))
    assertNull(BiometricRecords.decode("abc:"))
    assertNull(BiometricRecords.decode(":abc"))
    assertNull(BiometricRecords.decode("not base64!:also not!"))
  }

  @Test
  fun oneAliasPerPurpose() {
    assertEquals("forge.biometric.vault", BiometricRecords.alias("vault"))
    assertEquals("forge.biometric.sso", BiometricRecords.alias("sso"))
    assertNotEquals(BiometricRecords.alias("vault"), BiometricRecords.alias("sso"))
  }

  @Test
  fun anUnavailableSensorSaysWhichWay() {
    assertEquals(
      "no fingerprint is enrolled on this device",
      BiometricRecords.availabilityReason(BiometricManager.BIOMETRIC_ERROR_NONE_ENROLLED),
    )
    assertEquals(
      "this device has no fingerprint sensor",
      BiometricRecords.availabilityReason(BiometricManager.BIOMETRIC_ERROR_NO_HARDWARE),
    )
  }

  @Test
  fun aCancelledPromptIsAWordedRefusal() {
    assertEquals(
      "the gesture was cancelled",
      BiometricRecords.promptError(BiometricPrompt.ERROR_USER_CANCELED, "Canceled"),
    )
    assertEquals(
      "the fingerprint sensor is locked out; use the password",
      BiometricRecords.promptError(BiometricPrompt.ERROR_LOCKOUT, "Too many attempts"),
    )
  }
}
