plugins {
    alias(libs.plugins.android.library)
    alias(libs.plugins.kotlin.android)
}

android {
    namespace = "org.soulbeet.corebindings"
    compileSdk = 36

    defaultConfig {
        minSdk = 26
        consumerProguardFiles("consumer-rules.pro")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    lint {
        abortOnError = true
        warningsAsErrors = false
        lintConfig = file("lint.xml")
    }
}

dependencies {
    // CRITICAL: @aar ensures Android packages libjnidispatch.so for every ABI.
    api(libs.jna) {
        artifact {
            type = "aar"
        }
    }
    implementation(libs.androidx.annotation)
    api(libs.kotlinx.coroutines.core)
    api(libs.kotlinx.coroutines.android)

    testImplementation(libs.jna)
    testImplementation(libs.junit4)
}

tasks.register<Exec>("generateRustBindings") {
    group = "soulbeet"
    description = "Cross-compiles lib/client-core and regenerates UniFFI Kotlin bindings & jniLibs."
    workingDir = rootProject.projectDir.parentFile
    commandLine("bash", "scripts/build-android-core.sh", "--profile", "debug", "--abis", "arm64-v8a")
}
