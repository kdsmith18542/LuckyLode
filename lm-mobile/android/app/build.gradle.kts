plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "cc.luckylode.miner"
    compileSdk = 34

    defaultConfig {
        applicationId = "cc.luckylode.miner"
        minSdk = 24
        targetSdk = 34
        versionCode = 3
        versionName = "0.3.0"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
}

dependencies {
    // Deliberately zero third-party dependencies: plain android.app
    // (no androidx) keeps the sideloaded binary auditable and the
    // build hermetic. The Rust core is the only trusted code besides
    // the platform itself.
    testImplementation("junit:junit:4.13.2")
}
