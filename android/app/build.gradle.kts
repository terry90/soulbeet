import org.gradle.api.GradleException

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.compose)
}

val soulbeetVersionName = System.getenv("SOULBEET_VERSION_NAME")
    ?.trim()
    ?.removePrefix("v")
    ?.takeIf { it.isNotBlank() }
    ?: "0.1.0"

val soulbeetVersionCode = System.getenv("SOULBEET_VERSION_CODE")
    ?.trim()
    ?.toIntOrNull()
    ?.takeIf { it >= 1 }
    ?: 1

val releaseKeystorePath = System.getenv("ANDROID_KEYSTORE_PATH")?.trim()?.takeIf { it.isNotBlank() }
val releaseKeystorePassword = System.getenv("ANDROID_KEYSTORE_PASSWORD")?.takeIf { it.isNotBlank() }
val releaseKeyAlias = System.getenv("ANDROID_KEY_ALIAS")?.trim()?.takeIf { it.isNotBlank() }
val releaseKeyPassword = System.getenv("ANDROID_KEY_PASSWORD")?.takeIf { it.isNotBlank() }

val signingVars = linkedMapOf(
    "ANDROID_KEYSTORE_PATH" to releaseKeystorePath,
    "ANDROID_KEYSTORE_PASSWORD" to releaseKeystorePassword,
    "ANDROID_KEY_ALIAS" to releaseKeyAlias,
    "ANDROID_KEY_PASSWORD" to releaseKeyPassword,
)
val hasReleaseSigning = if (signingVars.values.any { it != null }) {
    val missing = signingVars.filterValues { it == null }.keys
    if (missing.isNotEmpty()) {
        throw GradleException(
            "Incomplete Android release signing configuration; missing or blank env var(s): ${missing.joinToString(", ")}",
        )
    }
    val keystoreFile = file(releaseKeystorePath!!)
    if (!keystoreFile.isFile) {
        throw GradleException(
            "Android release keystore file does not exist: ${keystoreFile.path}",
        )
    }
    true
} else {
    false
}

android {
    namespace = "org.soulbeet.app"
    compileSdk = 35

    defaultConfig {
        applicationId = "org.soulbeet.app"
        minSdk = 26
        targetSdk = 35
        versionCode = soulbeetVersionCode
        versionName = soulbeetVersionName

        vectorDrawables {
            useSupportLibrary = true
        }
    }

    if (hasReleaseSigning) {
        signingConfigs {
            create("release") {
                storeFile = file(releaseKeystorePath!!)
                storePassword = releaseKeystorePassword
                keyAlias = releaseKeyAlias
                keyPassword = releaseKeyPassword
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            if (hasReleaseSigning) {
                signingConfig = signingConfigs.getByName("release")
            }
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    buildFeatures {
        compose = true
    }

    packaging {
        resources {
            excludes += "/META-INF/{AL2.0,LGPL2.1}"
        }
    }

    testOptions {
        unitTests {
            isIncludeAndroidResources = true
            isReturnDefaultValues = true
        }
    }

    lint {
        abortOnError = true
        warningsAsErrors = false
    }
}

dependencies {
    implementation(project(":android-media"))
    implementation(project(":core-bindings"))

    implementation(libs.androidx.core.ktx)
    implementation(libs.kotlinx.coroutines.core)
    implementation(libs.kotlinx.coroutines.android)
    implementation(libs.kotlinx.coroutines.guava)

    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.compose.ui)
    implementation(libs.androidx.compose.ui.graphics)
    implementation(libs.androidx.compose.ui.tooling.preview)
    implementation(libs.androidx.compose.foundation)
    implementation(libs.androidx.compose.material3)
    implementation(libs.androidx.compose.material.icons.extended)
    debugImplementation(libs.androidx.compose.ui.tooling)

    implementation(libs.androidx.activity.compose)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    implementation(libs.androidx.lifecycle.runtime.compose)
    implementation(libs.androidx.lifecycle.viewmodel.compose)
    implementation(libs.androidx.datastore.preferences)
    implementation(libs.coil.compose)
    implementation(libs.okhttp)

    testImplementation(libs.jna)
    testImplementation(libs.junit4)
    testImplementation(libs.kotlinx.coroutines.test)
    testImplementation(libs.robolectric)
    testImplementation(libs.androidx.test.core)
}
