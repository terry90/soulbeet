package org.soulbeet.app.data

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Encrypts and decrypts sensitive session credentials (`auth_token`, `subsonic_token`,
 * `subsonic_salt`) using an Android Keystore hardware-backed AES-256-GCM key before storing
 * them in DataStore Preferences.
 */
object SecretCipher {

    private const val KEYSTORE_PROVIDER = "AndroidKeyStore"
    private const val KEY_ALIAS = "org.soulbeet.app.session_secret_key"
    private const val TRANSFORMATION = "AES/GCM/NoPadding"
    private const val GCM_TAG_BITS = 128
    private const val IV_LENGTH_BYTES = 12
    private const val PREFIX_V1 = "enc:v1:"

    fun encrypt(plaintext: String): String {
        if (plaintext.isEmpty()) return ""
        val secretKey = getOrCreateKey() ?: return plaintext
        return runCatching {
            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(Cipher.ENCRYPT_MODE, secretKey)
            val iv = cipher.iv
            val ciphertext = cipher.doFinal(plaintext.toByteArray(Charsets.UTF_8))
            val combined = ByteArray(iv.size + ciphertext.size)
            System.arraycopy(iv, 0, combined, 0, iv.size)
            System.arraycopy(ciphertext, 0, combined, iv.size, ciphertext.size)
            PREFIX_V1 + Base64.encodeToString(combined, Base64.NO_WRAP)
        }.getOrElse { plaintext }
    }

    fun decrypt(stored: String): String {
        if (stored.isEmpty()) return ""
        if (!stored.startsWith(PREFIX_V1)) {
            // Backward compatibility with any unencrypted value
            return stored
        }
        val secretKey = getOrCreateKey() ?: return ""
        return runCatching {
            val payload = Base64.decode(stored.removePrefix(PREFIX_V1), Base64.NO_WRAP)
            if (payload.size <= IV_LENGTH_BYTES) return@runCatching ""
            val iv = payload.copyOfRange(0, IV_LENGTH_BYTES)
            val ciphertext = payload.copyOfRange(IV_LENGTH_BYTES, payload.size)
            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(Cipher.DECRYPT_MODE, secretKey, GCMParameterSpec(GCM_TAG_BITS, iv))
            String(cipher.doFinal(ciphertext), Charsets.UTF_8)
        }.getOrDefault("")
    }

    private fun getOrCreateKey(): SecretKey? = runCatching {
        val keyStore = KeyStore.getInstance(KEYSTORE_PROVIDER).apply { load(null) }
        (keyStore.getKey(KEY_ALIAS, null) as? SecretKey)?.let { return@runCatching it }

        val keyGenerator = KeyGenerator.getInstance(
            KeyProperties.KEY_ALGORITHM_AES,
            KEYSTORE_PROVIDER,
        )
        val spec = KeyGenParameterSpec.Builder(
            KEY_ALIAS,
            KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
        )
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setKeySize(256)
            .build()
        keyGenerator.init(spec)
        keyGenerator.generateKey()
    }.getOrNull()
}
