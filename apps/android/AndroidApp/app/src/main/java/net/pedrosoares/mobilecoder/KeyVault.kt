package net.pedrosoares.mobilecoder

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Log
import java.io.File
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Encrypted storage for the Claude API key.
 *
 * The key is sealed with AES-GCM under a key that lives in the Android Keystore
 * and, on most devices, never leaves secure hardware. Only the ciphertext
 * touches app storage, so a backup, a filesystem dump, or another app with read
 * access to app data yields nothing usable.
 *
 * Plaintext exists only in memory, and only for as long as the process does.
 */
object KeyVault {

    private const val TAG = "mobile-coder"
    private const val ALIAS = "mobile-coder.api-key"
    private const val FILE = "api-key.bin"
    /** The GitHub token, sealed by the same Keystore key in its own file. */
    private const val GITHUB_FILE = "github-token.bin"
    private const val KEYSTORE = "AndroidKeyStore"
    private const val TRANSFORMATION = "AES/GCM/NoPadding"

    /** GCM's standard IV length. Stored as a prefix on the ciphertext. */
    private const val IV_BYTES = 12

    private fun secretKey(): SecretKey {
        val store = KeyStore.getInstance(KEYSTORE).apply { load(null) }
        (store.getEntry(ALIAS, null) as? KeyStore.SecretKeyEntry)?.let { return it.secretKey }

        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE)
        generator.init(
            KeyGenParameterSpec.Builder(
                ALIAS,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                // Deliberately NOT setUserAuthenticationRequired(true): a coding
                // agent runs long tasks in the background, and requiring a device
                // unlock per decrypt would break that. Revisit if the threat model
                // ever includes an attacker holding an unlocked phone.
                .build(),
        )
        return generator.generateKey()
    }

    private fun file(context: Context) = File(context.filesDir, FILE)

    private fun githubFile(context: Context) = File(context.filesDir, GITHUB_FILE)

    /** As [store], for the GitHub token. */
    fun storeGithub(context: Context, plaintext: String) = seal(githubFile(context), plaintext, "github token")

    /** As [load], for the GitHub token. */
    fun loadGithub(context: Context): String? = unseal(githubFile(context), "github token")

    fun clearGithub(context: Context) {
        githubFile(context).delete()
    }

    /** Seal [plaintext] and replace whatever was stored before. */
    fun store(context: Context, plaintext: String) = seal(file(context), plaintext, "api key")

    private fun seal(target: File, plaintext: String, what: String) {
        val cipher = Cipher.getInstance(TRANSFORMATION).apply { init(Cipher.ENCRYPT_MODE, secretKey()) }
        val sealed = cipher.doFinal(plaintext.toByteArray(Charsets.UTF_8))
        // iv || ciphertext. The IV is generated per encryption and is not secret.
        target.writeBytes(cipher.iv + sealed)
        Log.i(TAG, "${'$'}what stored (${'$'}{sealed.size} bytes sealed)")
    }

    /** Return the stored key, or null if there is none or it cannot be read. */
    fun load(context: Context): String? = unseal(file(context), "api key")

    private fun unseal(f: File, what: String): String? {
        if (!f.exists()) return null
        return try {
            val blob = f.readBytes()
            if (blob.size <= IV_BYTES) {
                Log.w(TAG, "stored ${'$'}what is truncated; ignoring")
                return null
            }
            val cipher = Cipher.getInstance(TRANSFORMATION).apply {
                init(
                    Cipher.DECRYPT_MODE,
                    secretKey(),
                    GCMParameterSpec(128, blob, 0, IV_BYTES),
                )
            }
            String(cipher.doFinal(blob, IV_BYTES, blob.size - IV_BYTES), Charsets.UTF_8)
        } catch (e: Exception) {
            // Most likely the Keystore key was invalidated - a factory reset, or
            // the app's data being restored onto a different device. The stored
            // bytes are then permanently unreadable, so drop them rather than
            // failing this way on every launch.
            Log.w(TAG, "could not decrypt the stored ${'$'}what; clearing it: ${'$'}e")
            f.delete()
            null
        }
    }

    fun clear(context: Context) {
        file(context).delete()
    }
}
