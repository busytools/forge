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
    // **Every branch is pinned, not just the two the card most often draws.**
    // A branch left unpinned reverts to "fingerprint" green: the suite would
    // pass while the card quoted the wrong word.
    assertEquals(
      "no biometric is enrolled on this device",
      BiometricRecords.availabilityReason(BiometricManager.BIOMETRIC_ERROR_NONE_ENROLLED),
    )
    assertEquals(
      "this device has no biometric sensor",
      BiometricRecords.availabilityReason(BiometricManager.BIOMETRIC_ERROR_NO_HARDWARE),
    )
    assertEquals(
      "the biometric sensor is not available right now",
      BiometricRecords.availabilityReason(BiometricManager.BIOMETRIC_ERROR_HW_UNAVAILABLE),
    )
    assertEquals(
      "this device's security patch is too old for biometric keys",
      BiometricRecords.availabilityReason(BiometricManager.BIOMETRIC_ERROR_SECURITY_UPDATE_REQUIRED),
    )
    assertEquals(
      "this device does not support biometric keys",
      BiometricRecords.availabilityReason(BiometricManager.BIOMETRIC_ERROR_UNSUPPORTED),
    )
    assertEquals(
      "the biometric sensor's state is unknown",
      BiometricRecords.availabilityReason(BiometricManager.BIOMETRIC_STATUS_UNKNOWN),
    )
    assertEquals(
      "the biometric sensor answered 1234",
      BiometricRecords.availabilityReason(1234),
    )
  }

  @Test
  fun aCancelledPromptIsAWordedRefusal() {
    assertEquals(
      "the gesture was cancelled",
      BiometricRecords.promptError(BiometricPrompt.ERROR_USER_CANCELED, "Canceled"),
    )
    assertEquals(
      "the biometric sensor is locked out; use the password",
      BiometricRecords.promptError(BiometricPrompt.ERROR_LOCKOUT, "Too many attempts"),
    )
  }

  @Test
  fun anUnknownPromptErrorCarriesThePlatformMessage() {
    // **The default branch is pinned too.** A branch left unpinned reverts
    // to "fingerprint" green in a later edit, and the card quotes it.
    assertEquals(
      "the biometric sensor answered 42",
      BiometricRecords.promptError(42, "the biometric sensor answered 42"),
    )
    assertEquals(
      "the biometric sensor answered 42",
      BiometricRecords.promptError(42, ""),
    )
  }

  @Test
  fun aReleaseRefusalNamesTheInvalidation() {
    // The two refusals `release` draws when the key is dead or unusable.
    // Pinned here so a reword cannot land without the card's copy moving.
    assertEquals(
      "the device's biometrics changed, so the stored unlock was invalidated; use the password and enrol again",
      BiometricRecords.invalidatedRelease(),
    )
    assertEquals(
      "the stored unlock cannot be used on this device; use the password and enrol again",
      BiometricRecords.unusableRelease(),
    )
  }
}
