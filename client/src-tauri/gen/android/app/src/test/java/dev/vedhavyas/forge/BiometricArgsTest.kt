package dev.vedhavyas.forge

import com.fasterxml.jackson.annotation.JsonAutoDetect
import com.fasterxml.jackson.annotation.PropertyAccessor
import com.fasterxml.jackson.databind.DeserializationFeature
import com.fasterxml.jackson.databind.ObjectMapper
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/**
 * The plugin's `@Command` arguments as the RELEASE build binds them.
 *
 * **The bug this pins lived only in the release variant**: debug disables
 * minification, while release runs R8 over these classes, which cannot see the
 * no-arg constructor or the fields Jackson reaches by reflection. What keeps
 * them is `@InvokeArg` and Tauri's consumer rule for it. The shipped APK
 * failed at `parseArgs` before the prompt (`Cannot construct instance of
 * dev.vedhavyas.forge.StoreArgs (no Creators)`). This test cannot exercise R8;
 * it pins the mapper contract and the blank-argument refusal the commands rely
 * on.
 */
class BiometricArgsTest {
  /**
   * The mapper shape `PluginManager` builds: fields bound by name, unknown
   * properties ignored. A mapper this test configured differently would prove
   * a contract the app does not use.
   */
  private val mapper = ObjectMapper()
    .disable(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES)
    .enable(DeserializationFeature.FAIL_ON_NULL_FOR_PRIMITIVES)
    .setVisibility(PropertyAccessor.FIELD, JsonAutoDetect.Visibility.ANY)

  @Test
  fun theEnrolmentPayloadBindsIntoStoreArgs() {
    val args = mapper.readValue("""{"purpose":"vault","blob":"..."}""", StoreArgs::class.java)

    assertEquals("vault", args.purpose)
    assertEquals("...", args.blob)
  }

  @Test
  fun thePurposeCommandsBind() {
    val args = mapper.readValue("""{"purpose":"vault"}""", PurposeArgs::class.java)

    assertEquals("vault", args.purpose)
  }

  /**
   * A blank purpose binds rather than throwing, so the command's own guard is
   * what refuses it. Without that guard `store` would write the empty alias
   * and resolve success.
   */
  @Test
  fun aBlankPurposeIsRefusedInsteadOfStored() {
    val args = mapper.readValue("""{"purpose":"","blob":"..."}""", StoreArgs::class.java)

    assertEquals(
      "the biometric request has no purpose",
      BiometricRecords.missingPurpose(args.purpose),
    )
    assertNull(BiometricRecords.missingPurpose("vault"))
  }

  @Test
  fun aBlankBlobIsRefused() {
    val args = mapper.readValue("""{"purpose":"vault","blob":""}""", StoreArgs::class.java)

    assertEquals("the biometric request has no blob", BiometricRecords.missingBlob(args.blob))
    assertNull(BiometricRecords.missingBlob("..."))
  }
}
