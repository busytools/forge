package dev.vedhavyas.forge

import android.app.Activity
import android.os.Build
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyPermanentlyInvalidatedException
import android.security.keystore.KeyProperties
import android.security.keystore.UserNotAuthenticatedException
import androidx.biometric.BiometricManager
import androidx.biometric.BiometricPrompt
import androidx.core.content.ContextCompat
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.security.KeyStore
import java.util.Base64
import java.security.UnrecoverableKeyException
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * The phone's gate: one Android Keystore key per purpose, generated with
 * `setUserAuthenticationRequired(true)`, and the consumer's blob encrypted
 * under it. The key never leaves the Keystore, and the ciphertext is the only
 * thing this plugin stores, in the app's own preferences.
 *
 * **The purpose is the whole key.** One alias and one record per purpose, so
 * a second consumer is a new purpose string - no new code, no new key type -
 * and revocation drops that purpose's record and key, nothing else. A change
 * to the enrolled biometrics invalidates the key, which is the platform's
 * own invalidation: a per-use key is still returned by `getKey` and refuses
 * at `init`, so a release falls back to the password and a fresh enrolment
 * replaces the dead key.
 *
 * The Rust side (`client/src-tauri/src/biometric/android.rs`) calls these
 * commands; the page never sees the Keystore.
 */
@TauriPlugin
class BiometricPlugin(private val activity: Activity) : Plugin(activity) {

  companion object {
    private const val KEYSTORE = "AndroidKeyStore"
    private const val PREFERENCES = "forge.biometric"
    private const val TRANSFORMATION = "AES/GCM/NoPadding"
    private const val TAG_BITS = 128
    private const val GCM_IV_BYTES = 12
    private const val RECORD_SEPARATOR = ":"
  }

  @Command
  fun availability(invoke: Invoke) {
    background(invoke) {
      val status = BiometricManager.from(activity)
        .canAuthenticate(BiometricManager.Authenticators.BIOMETRIC_STRONG)
      val answer = JSObject()
      if (status == BiometricManager.BIOMETRIC_SUCCESS) {
        answer.put("available", true).put("biometric", true)
      } else {
        answer.put("available", false).put("biometric", false)
          .put("reason", BiometricRecords.availabilityReason(status))
      }
      invoke.resolve(answer)
    }
  }

  @Command
  fun enrolled(invoke: Invoke) {
    val args = invoke.parseArgs(PurposeArgs::class.java)
    background(invoke) {
      val hasRecord = preferences().contains(args.purpose)
      val hasKey = try {
        // **A per-use key still comes back from `getKey` after a biometric
        // change** and refuses only when a cipher uses it, so its usability
        // is asked here rather than taken from `getKey` answering at all.
        // The probe needs no gesture: the auth check lands at `doFinal`.
        keyFor(args.purpose, create = false)?.let { usable(it) } ?: false
      } catch (unrecoverable: UnrecoverableKeyException) {
        false
      } catch (transient: Exception) {
        // **Only a genuine invalidation may mean not-enrolled.** A provider,
        // auth or key-store error on a healthy key would otherwise draw "not
        // enrolled" and revoke a working enrolment on every read. Report
        // conservatively enrolled; release still falls back to the password
        // if the key truly cannot be used. **A permanently broken keystore is
        // the trade this makes:** the card stays enrolled and the revoke door
        // is the only recovery, where the pre-PR path showed not-enrolled and
        // offered the enrol press. Deliberate: a transient provider failure
        // must not revoke a healthy enrolment on every read.
        true
      }
      invoke.resolve(JSObject().put("enrolled", hasRecord && hasKey))
    }
  }

  /**
   * Whether `key` can still release. `Cipher.init` is where a per-use key
   * reports that the OS invalidated it after a biometric change, and it
   * reports it without a prompt.
   */
  private fun usable(key: SecretKey): Boolean =
    try {
      Cipher.getInstance(TRANSFORMATION).init(
        Cipher.DECRYPT_MODE, key, GCMParameterSpec(TAG_BITS, ByteArray(GCM_IV_BYTES))
      )
      true
    } catch (invalidated: KeyPermanentlyInvalidatedException) {
      false
    } catch (transient: Exception) {
      // **Only a genuine invalidation may mean not-enrolled.** A provider,
      // auth or key-store error on a healthy key is not proof the key is
      // dead; the auth check lands at `doFinal`, and release falls back to
      // the password if the key truly cannot be used. Report conservatively
      // enrolled rather than revoke a working enrolment on a read error. A
      // permanently broken keystore stays enrolled here too, so the revoke
      // door is its only recovery; the same deliberate trade as `enrolled`.
      true
    }

  /**
   * Register one purpose's blob, replacing any blob already there.
   *
   * **The encrypt asks for the gesture too.** The key is auth-bound for every
   * use, so the cipher is not authorised until the person answers the prompt,
   * and the prompt carries the cipher as its `CryptoObject` so the `doFinal`
   * that writes the record is the authorised one (the per-use check lands at
   * `doFinal`, so `init` is expected to succeed).
   *
   * **A key the OS invalidated is replaced here.** A biometric change
   * invalidates a per-use key at use, not at `getKey`: the key comes back and
   * `init` throws `KeyPermanentlyInvalidatedException`. Catching it, deleting
   * the alias and generating again is what makes the card's "enrol again"
   * true on this device.
   */
  @Command
  fun store(invoke: Invoke) {
    val args = invoke.parseArgs(StoreArgs::class.java)
    background(invoke) {
      var key = keyFor(args.purpose, create = true)
        ?: throw IllegalStateException("the keystore refused to make a key")
      var cipher = Cipher.getInstance(TRANSFORMATION)
      try {
        cipher.init(Cipher.ENCRYPT_MODE, key)
      } catch (invalidated: KeyPermanentlyInvalidatedException) {
        // **The stale record goes with the key it was sealed under.** The old
        // record is undecryptable from this moment, so leaving it behind means
        // a cancelled or failed re-enrol strands the client: `enrolled` reads
        // true off the record and the new key, the card offers no enrol door,
        // and every unlock refuses at `doFinal`. Removing the record first
        // means the abort leaves `enrolled` false and the card offers the
        // enrol press again. A key removal that fails is harmless once the
        // record is gone; a record removal that fails aborts the re-enrol.
        val removed = preferences().edit().remove(args.purpose).commit()
        if (!removed) {
          invoke.reject("the invalidated enrolment could not be removed on this device")
          return@background
        }
        deleteKey(args.purpose)
        key = keyFor(args.purpose, create = true)
          ?: throw IllegalStateException("the keystore refused to make a key")
        cipher = Cipher.getInstance(TRANSFORMATION)
        try {
          cipher.init(Cipher.ENCRYPT_MODE, key)
        } catch (unauthenticated: UserNotAuthenticatedException) {
          // The auth bound is the intended state: the prompt below
          // authorises this cipher.
        }
      } catch (unauthenticated: UserNotAuthenticatedException) {
        // The auth bound is the intended state: the prompt below authorises
        // this cipher.
      }
      val host = activity as? androidx.fragment.app.FragmentActivity
      if (host == null) {
        // A prompt that never opened wrote nothing, so a resolved answer here
        // would register an enrolment the gate never stored. Reject like the
        // prompt-throw and commit-false paths.
        invoke.reject("this window cannot show the biometric prompt")
        return@background
      }
      val prompt = BiometricPrompt(
        host,
        ContextCompat.getMainExecutor(activity),
        object : BiometricPrompt.AuthenticationCallback() {
          override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult) {
            try {
              val ready = result.cryptoObject?.cipher ?: cipher
              val ciphertext = ready.doFinal(Base64.getDecoder().decode(args.blob))
              // **The commit's answer is the enrolment's.** A resolved call
              // is a record the caller can rely on; a failed write answering
              // success would register sealed material against a key the gate
              // does not hold.
              val wrote =
                preferences()
                  .edit()
                  .putString(args.purpose, BiometricRecords.encode(ready.iv, ciphertext))
                  .commit()
              if (!wrote) {
                invoke.reject("the enrolment could not be written on this device")
                return
              }
              invoke.resolve()
            } catch (err: Exception) {
              invoke.reject(err.message ?: err.toString())
            }
          }

          override fun onAuthenticationError(code: Int, message: CharSequence) {
            // **A refused prompt stores nothing, so it is an error.** A
            // resolved answer here would let the caller record an enrolment
            // whose key the gate never wrote.
            invoke.reject(BiometricRecords.promptError(code, message))
          }
        }
      )
      activity.runOnUiThread {
        try {
          prompt.authenticate(promptInfo(), BiometricPrompt.CryptoObject(cipher))
        } catch (err: Exception) {
          // The same rule as a refused prompt: a prompt that never opened
          // wrote nothing, so it must not answer as a stored enrolment.
          invoke.reject(err.message ?: err.toString())
        }
      }
    }
  }

  /**
   * Release one purpose's blob, asking for the biometric first.
   *
   * The cipher is initialised before the prompt and completed by it: the key
   * refuses a decrypt until the biometric has been verified, and the prompt's
   * `CryptoObject` is what carries that authorisation into `doFinal`.
   */
  @Command
  fun release(invoke: Invoke) {
    val args = invoke.parseArgs(PurposeArgs::class.java)
    val record = preferences().getString(args.purpose, null)
    if (record == null) {
      invoke.resolve(notEnrolled())
      return
    }
    val parts = BiometricRecords.decode(record)
    if (parts == null) {
      invoke.resolve(refused("the stored unlock record could not be read"))
      return
    }
    val key = try {
      keyFor(args.purpose, create = false)
    } catch (invalidated: KeyPermanentlyInvalidatedException) {
      invoke.resolve(refused(BiometricRecords.invalidatedRelease()))
      return
    } catch (unrecoverable: UnrecoverableKeyException) {
      invoke.resolve(refused(BiometricRecords.unusableRelease()))
      return
    } ?: run {
      invoke.resolve(notEnrolled())
      return
    }
    val cipher = try {
      Cipher.getInstance(TRANSFORMATION).apply {
        init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(TAG_BITS, parts.first))
      }
    } catch (invalidated: KeyPermanentlyInvalidatedException) {
      invoke.resolve(refused(BiometricRecords.invalidatedRelease()))
      return
    } catch (unusable: UnrecoverableKeyException) {
      invoke.resolve(refused(BiometricRecords.unusableRelease()))
      return
    }

    val host = activity as? androidx.fragment.app.FragmentActivity
    if (host == null) {
      invoke.resolve(refused("this window cannot show the biometric prompt"))
      return
    }
    val prompt = BiometricPrompt(
      host,
      ContextCompat.getMainExecutor(activity),
      object : BiometricPrompt.AuthenticationCallback() {
        override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult) {
          try {
            val plaintext = (result.cryptoObject?.cipher ?: cipher).doFinal(parts.second)
            invoke.resolve(
              JSObject().put("outcome", "released")
                .put("blob", Base64.getEncoder().encodeToString(plaintext))
            )
          } catch (err: Exception) {
            invoke.resolve(
              refused(
                "the unlock could not be decrypted: ${err.message ?: err.toString()}"
              )
            )
          }
        }

        override fun onAuthenticationError(code: Int, message: CharSequence) {
          invoke.resolve(refused(BiometricRecords.promptError(code, message)))
        }
      }
    )
    activity.runOnUiThread {
      try {
        prompt.authenticate(promptInfo(), BiometricPrompt.CryptoObject(cipher))
      } catch (err: Exception) {
        invoke.resolve(refused(err.message ?: err.toString()))
      }
    }
  }

  /**
   * The prompt both commands show. **The negative button is required**: with
   * `BIOMETRIC_STRONG` and no device credential allowed, `build()` throws
   * before `authenticate` is ever reached when it is absent.
   */
  private fun promptInfo(): BiometricPrompt.PromptInfo =
    BiometricPrompt.PromptInfo.Builder()
      .setTitle("unlock your vault")
      .setAllowedAuthenticators(BiometricManager.Authenticators.BIOMETRIC_STRONG)
      .setNegativeButtonText("cancel")
      .build()

  /** Drop one purpose's blob and its key. A second purpose is untouched. */
  @Command
  fun remove(invoke: Invoke) {
    val args = invoke.parseArgs(PurposeArgs::class.java)
    background(invoke) {
      preferences().edit().remove(args.purpose).commit()
      deleteKey(args.purpose)
      invoke.resolve()
    }
  }

  /**
   * Delete a purpose's key. A key already gone is the state a delete wants;
   * anything else is reported, because the caller cannot tell the two apart
   * otherwise.
   */
  private fun deleteKey(purpose: String) {
    try {
      keystore().deleteEntry(BiometricRecords.alias(purpose))
    } catch (err: Exception) {
      if (err !is java.security.KeyStoreException) throw err
    }
  }

  private fun preferences() =
    activity.getSharedPreferences(PREFERENCES, Activity.MODE_PRIVATE)

  private fun keystore(): KeyStore = KeyStore.getInstance(KEYSTORE).apply { load(null) }

  private fun keyFor(purpose: String, create: Boolean): SecretKey? {
    val alias = BiometricRecords.alias(purpose)
    val store = keystore()
    val existing = try {
      store.getKey(alias, null) as? SecretKey
    } catch (invalidated: UnrecoverableKeyException) {
      // **A biometric change invalidates the key and `getKey` reports it
      // this way.** A release cannot use it; a fresh enrolment replaces it
      // rather than failing, which is what makes "enrol again" true.
      if (!create) throw invalidated
      null
    }
    existing?.let { return it }
    if (!create) return null
    try {
      store.deleteEntry(alias)
    } catch (_: java.security.KeyStoreException) {
      // A key already gone is the state a fresh enrolment wants.
    }
    val builder = KeyGenParameterSpec.Builder(
      alias,
      KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT
    )
      .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
      .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
      .setKeySize(256)
      .setUserAuthenticationRequired(true)
      .setInvalidatedByBiometricEnrollment(true)
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
      builder.setUserAuthenticationParameters(0, KeyProperties.AUTH_BIOMETRIC_STRONG)
    }
    val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE)
    generator.init(builder.build())
    return generator.generateKey()
  }

  private fun notEnrolled(): JSObject = JSObject().put("outcome", "not_enrolled")

  private fun refused(reason: String): JSObject =
    JSObject().put("outcome", "refused").put("reason", reason)

  /** Off the main thread: a Keystore read is a binder call into keystored. */
  private fun background(invoke: Invoke, work: () -> Unit) {
    Thread {
      try {
        work()
      } catch (err: Exception) {
        invoke.reject(err.message ?: err.toString())
      }
    }.start()
  }
}

class PurposeArgs {
  lateinit var purpose: String
}

class StoreArgs {
  lateinit var purpose: String
  lateinit var blob: String
}

/**
 * The stored record and the prompt's words, kept free of Android classes so
 * the JVM unit tests can read them: the record codec is the only thing a
 * restart depends on, and a platform reason is what the card quotes.
 */
internal object BiometricRecords {
  private const val ALIAS_PREFIX = "forge.biometric."
  private const val RECORD_SEPARATOR = ":"

  fun alias(purpose: String): String = ALIAS_PREFIX + purpose

  /** The stored record: base64(iv):base64(ciphertext). */
  fun encode(iv: ByteArray, ciphertext: ByteArray): String =
    Base64.getEncoder().encodeToString(iv) + RECORD_SEPARATOR +
      Base64.getEncoder().encodeToString(ciphertext)

  fun decode(record: String): Pair<ByteArray, ByteArray>? {
    val at = record.indexOf(RECORD_SEPARATOR)
    if (at <= 0 || at == record.length - 1) return null
    return try {
      Base64.getDecoder().decode(record.substring(0, at)) to
        Base64.getDecoder().decode(record.substring(at + 1))
    } catch (err: IllegalArgumentException) {
      null
    }
  }

  /** The platform's own words for a failed availability check. **Biometric,
   *  not fingerprint**: `BIOMETRIC_STRONG` includes face unlock on the
   *  devices that offer it, and these words are drawn in the card. */
  fun availabilityReason(status: Int): String = when (status) {
    BiometricManager.BIOMETRIC_ERROR_NO_HARDWARE -> "this device has no biometric sensor"
    BiometricManager.BIOMETRIC_ERROR_HW_UNAVAILABLE ->
      "the biometric sensor is not available right now"
    BiometricManager.BIOMETRIC_ERROR_NONE_ENROLLED -> "no biometric is enrolled on this device"
    BiometricManager.BIOMETRIC_ERROR_SECURITY_UPDATE_REQUIRED ->
      "this device's security patch is too old for biometric keys"
    BiometricManager.BIOMETRIC_ERROR_UNSUPPORTED ->
      "this device does not support biometric keys"
    BiometricManager.BIOMETRIC_STATUS_UNKNOWN -> "the biometric sensor's state is unknown"
    else -> "the biometric sensor answered $status"
  }

  /** The refusal a release gives when the key itself was invalidated. */
  fun invalidatedRelease(): String =
    "the device's biometrics changed, so the stored unlock was invalidated; use the password and enrol again"

  /** The refusal a release gives when the key exists but cannot be used. */
  fun unusableRelease(): String =
    "the stored unlock cannot be used on this device; use the password and enrol again"

  /** The platform's own words for a prompt that did not succeed. */
  fun promptError(code: Int, message: CharSequence): String = when (code) {
    BiometricPrompt.ERROR_USER_CANCELED,
    BiometricPrompt.ERROR_NEGATIVE_BUTTON,
    BiometricPrompt.ERROR_CANCELED -> "the gesture was cancelled"
    BiometricPrompt.ERROR_LOCKOUT,
    BiometricPrompt.ERROR_LOCKOUT_PERMANENT ->
      "the biometric sensor is locked out; use the password"
    else -> message.toString().ifEmpty { "the biometric sensor answered $code" }
  }
}
