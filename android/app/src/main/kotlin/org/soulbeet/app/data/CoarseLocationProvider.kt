package org.soulbeet.app.data

import android.Manifest
import android.annotation.SuppressLint
import android.content.Context
import android.content.pm.PackageManager
import android.location.Location
import android.location.LocationManager
import android.os.Build
import android.os.CancellationSignal
import android.os.SystemClock
import androidx.annotation.RequiresApi
import androidx.core.content.ContextCompat
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withTimeoutOrNull
import kotlin.coroutines.resume
import kotlin.math.round

/**
 * Pure Android SDK coarse location reader for foreground-only Weather Mixes.
 *
 * Uses only [LocationManager] (no Google Play Services) with [Manifest.permission.ACCESS_COARSE_LOCATION]
 * and rounds coordinates to 0.1 degree (~11 km city-level accuracy) before they ever leave the device.
 */
object CoarseLocationProvider {

    private const val FRESH_LAST_KNOWN_MAX_AGE_MS: Long = 15L * 60L * 1_000L
    private const val FALLBACK_LAST_KNOWN_MAX_AGE_MS: Long = 60L * 60L * 1_000L

    /**
     * Rounds a coordinate to 1 decimal place (0.1 degree, ~11 km) and normalizes -0.0 to 0.0.
     */
    fun roundToTenthDegree(value: Double): Double {
        if (!value.isFinite()) return value
        val rounded = round(value * 10.0) / 10.0
        return if (rounded == 0.0) 0.0 else rounded
    }

    /**
     * Validates raw coordinates (rejecting NaN, Infinity, out-of-range values, and uninitialized
     * (0.0, 0.0) fixes) and returns them rounded to 0.1 degree.
     */
    fun sanitizeCoarseCoordinates(lat: Double, lon: Double): Pair<Double, Double>? {
        if (!lat.isFinite() || !lon.isFinite()) return null
        if (lat !in -90.0..90.0 || lon !in -180.0..180.0) return null
        if (lat == 0.0 && lon == 0.0) return null
        return roundToTenthDegree(lat) to roundToTenthDegree(lon)
    }

    fun hasCoarseLocationPermission(context: Context): Boolean =
        ContextCompat.checkSelfPermission(
            context,
            Manifest.permission.ACCESS_COARSE_LOCATION,
        ) == PackageManager.PERMISSION_GRANTED

    /**
     * Returns foreground coarse (lat, lon) rounded to 0.1 degree, or null if permission is not
     * granted, location providers are unavailable, or no fix can be obtained within [timeoutMs].
     */
    @SuppressLint("MissingPermission")
    suspend fun getCoarseCoordinates(
        context: Context,
        timeoutMs: Long = 3_000L,
    ): Pair<Double, Double>? {
        if (!hasCoarseLocationPermission(context)) return null
        val lm = context.getSystemService(Context.LOCATION_SERVICE) as? LocationManager ?: return null

        val candidateProviders = buildList {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S && isProviderEnabledSafe(lm, LocationManager.FUSED_PROVIDER)) {
                add(LocationManager.FUSED_PROVIDER)
            }
            if (isProviderEnabledSafe(lm, LocationManager.NETWORK_PROVIDER)) {
                add(LocationManager.NETWORK_PROVIDER)
            }
            if (isProviderEnabledSafe(lm, LocationManager.PASSIVE_PROVIDER)) {
                add(LocationManager.PASSIVE_PROVIDER)
            }
        }

        val nowNanos = SystemClock.elapsedRealtimeNanos()
        var freshestFallbackAgeMs: Long = Long.MAX_VALUE
        var freshestFallbackCoords: Pair<Double, Double>? = null

        for (provider in candidateProviders) {
            val loc = runCatching { lm.getLastKnownLocation(provider) }.getOrNull() ?: continue
            val sanitized = sanitizeCoarseCoordinates(loc.latitude, loc.longitude) ?: continue
            val elapsedNanos = loc.elapsedRealtimeNanos
            if (elapsedNanos <= 0L) continue
            val ageMs = (nowNanos - elapsedNanos) / 1_000_000L
            if (ageMs !in 0L..FALLBACK_LAST_KNOWN_MAX_AGE_MS) continue

            if (ageMs < freshestFallbackAgeMs) {
                freshestFallbackAgeMs = ageMs
                freshestFallbackCoords = sanitized
            }
        }

        if (freshestFallbackCoords != null && freshestFallbackAgeMs <= FRESH_LAST_KNOWN_MAX_AGE_MS) {
            return freshestFallbackCoords
        }

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            val activeProviders = buildList {
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S &&
                    isProviderEnabledSafe(lm, LocationManager.FUSED_PROVIDER)
                ) {
                    add(LocationManager.FUSED_PROVIDER)
                }
                if (isProviderEnabledSafe(lm, LocationManager.NETWORK_PROVIDER)) {
                    add(LocationManager.NETWORK_PROVIDER)
                }
            }
            for (provider in activeProviders) {
                val live = requestCurrentLocationApi30(context, lm, provider, timeoutMs)
                if (live != null) {
                    sanitizeCoarseCoordinates(live.latitude, live.longitude)?.let { return it }
                }
            }
        }

        return freshestFallbackCoords
    }

    private fun isProviderEnabledSafe(lm: LocationManager, provider: String): Boolean =
        runCatching { lm.isProviderEnabled(provider) }.getOrDefault(false)

    @RequiresApi(Build.VERSION_CODES.R)
    @SuppressLint("MissingPermission")
    private suspend fun requestCurrentLocationApi30(
        context: Context,
        locationManager: LocationManager,
        provider: String,
        timeoutMs: Long,
    ): Location? {
        val signal = CancellationSignal()
        return try {
            withTimeoutOrNull(timeoutMs) {
                suspendCancellableCoroutine<Location?> { cont ->
                    cont.invokeOnCancellation { signal.cancel() }
                    try {
                        locationManager.getCurrentLocation(
                            provider,
                            signal,
                            context.mainExecutor,
                        ) { location ->
                            if (cont.isActive) {
                                cont.resume(location)
                            }
                        }
                    } catch (_: SecurityException) {
                        if (cont.isActive) cont.resume(null)
                    } catch (_: IllegalArgumentException) {
                        if (cont.isActive) cont.resume(null)
                    }
                }
            }
        } finally {
            signal.cancel()
        }
    }
}
