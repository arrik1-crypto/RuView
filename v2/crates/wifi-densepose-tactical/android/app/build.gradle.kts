plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.ruvnet.tactical"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.ruvnet.tactical"
        minSdk = 26
        targetSdk = 34
        versionCode = 1
        versionName = "0.3.0"
        // Ship all four ABIs so the APK runs on phones, tablets, and emulators.
        // Trim to just arm64-v8a for a smaller field build.
        ndk {
            abiFilters += listOf("arm64-v8a", "armeabi-v7a", "x86_64", "x86")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    // .so libraries produced by ../build-jni.sh (cargo ndk output).
    sourceSets["main"].jniLibs.srcDirs("src/main/jniLibs")

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
}

dependencies {
    // Intentionally empty: the UI is the Rust-served WebView dashboard, and the
    // engine/server is the bundled native library. No third-party Android deps.
}
