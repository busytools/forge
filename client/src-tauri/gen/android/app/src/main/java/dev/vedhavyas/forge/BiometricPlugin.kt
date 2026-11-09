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
 * to the enrolled fingerprints invalidates the key (the OS deletes it), which
 * is the platform's own invalidation: the record stays and the decrypt is
 * refused, so the client falls back to the password and re-enrols.
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
        keyFor(args.purpose, create = false) != null
      } catch (invalidated: KeyPermanentlyInvalidatedException) {
        // A key the OS invalidated after a fingerprint change is not one this
        // device can release with, which is what the caller is asking.
        false
      } catch (unrecoverable: UnrecoverableKeyException) {
        false
      }
      invoke.resolve(JSObject().put("enrolled", hasRecord && hasKey))
    }
  }

  /**
   * Register one purpose's blob, replacing any blob already there.
   *
   * **The encrypt asks for the gesture too**: the key is auth-bound for every
   * use, so `init` calls `UserNotAuthenticatedException` until the person
   * answers a prompt, and the prompt carries the cipher as its `CryptoObject`
   * so the `doFinal` that writes the record is the authorised one. The
   * consequence is that a device enrols with a prompt right after the
   * password unlock that asked for it.
   */
  @Command
  fun store(invoke: Invoke) {
    val args = invoke.parseArgs(StoreArgs::class.java)
    background(invoke) {
      val key = keyFor(args.purpose, create = true)
        ?: throw IllegalStateException("the keystore refused to make a key")
      val cipher = Cipher.getInstance(TRANSFORMATION)
      try {
        cipher.init(Cipher.ENCRYPT_MODE, key)
      } catch (unauthenticated: UserNotAuthenticatedException) {
        // The key's auth bound is the intended state: the prompt below is
        // what authorises this cipher.
      }
      val host = activity as? androidx.fragment.app.FragmentActivity
      if (host == null) {
        invoke.resolve(refused("this window cannot show the fingerprint prompt"))
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
              preferences().edit()
                .putString(args.purpose, BiometricRecords.encode(ready.iv, ciphertext))
                .commit()
              invoke.resolve()
            } catch (err: Exception) {
              invoke.reject(err.message ?: err.toString())
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
  }

  /**
   * Release one purpose's blob, asking for the fingerprint first.
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
      invoke.resolve(refused(
        "the device's fingerprints changed, so the stored unlock was invalidated; use the password and enrol again"
      ))
      return
    } catch (unrecoverable: UnrecoverableKeyException) {
      invoke.resolve(refused(
        "the stored unlock cannot be used on this device; use the password and enrol again"
      ))
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
      invoke.resolve(refused(
        "the device's fingerprints changed, so the stored unlock was invalidated; use the password and enrol again"
      ))
      return
    } catch (unusable: UnrecoverableKeyException) {
      invoke.resolve(refused(
        "the stored unlock cannot be used on this device; use the password and enrol again"
      ))
      return
    }

    val host = activity as? androidx.fragment.app.FragmentActivity
    if (host == null) {
      invoke.resolve(refused("this window cannot show the fingerprint prompt"))
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
      try {
        keystore().deleteEntry(BiometricRecords.alias(args.purpose))
      } catch (err: Exception) {
        // A key already gone is the state revocation wants; anything else is
        // reported, because the caller cannot tell the two apart otherwise.
        if (err !is java.security.KeyStoreException) throw err
      }
      invoke.resolve()
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
      // **A fingerprint change invalidates the key and `getKey` reports it
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

  /** The platform's own words for a failed availability check. */
  fun availabilityReason(status: Int): String = when (status) {
    BiometricManager.BIOMETRIC_ERROR_NO_HARDWARE -> "this device has no fingerprint sensor"
    BiometricManager.BIOMETRIC_ERROR_HW_UNAVAILABLE ->
      "the fingerprint sensor is not available right now"
    BiometricManager.BIOMETRIC_ERROR_NONE_ENROLLED -> "no fingerprint is enrolled on this device"
    BiometricManager.BIOMETRIC_ERROR_SECURITY_UPDATE_REQUIRED ->
      "this device's security patch is too old for fingerprint keys"
    BiometricManager.BIOMETRIC_ERROR_UNSUPPORTED ->
      "this device does not support fingerprint keys"
    BiometricManager.BIOMETRIC_STATUS_UNKNOWN -> "the fingerprint sensor's state is unknown"
    else -> "the fingerprint sensor answered $status"
  }

  /** The platform's own words for a prompt that did not succeed. */
  fun promptError(code: Int, message: CharSequence): String = when (code) {
    BiometricPrompt.ERROR_USER_CANCELED,
    BiometricPrompt.ERROR_NEGATIVE_BUTTON,
    BiometricPrompt.ERROR_CANCELED -> "the gesture was cancelled"
    BiometricPrompt.ERROR_LOCKOUT,
    BiometricPrompt.ERROR_LOCKOUT_PERMANENT ->
      "the fingerprint sensor is locked out; use the password"
    else -> message.toString().ifEmpty { "the fingerprint sensor answered $code" }
  }
}
