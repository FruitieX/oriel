import org.jetbrains.kotlin.gradle.dsl.JvmTarget

// AGP 9 compiles Kotlin itself; the root build file picks the Kotlin version.
plugins {
    alias(libs.plugins.android.application)
}

// Built by scripts/build-android.sh: the Rust library (cargo ndk) and the
// launcher icon land in ../target/android first.
val shared = rootProject.file("../target/android")

// Firebase Cloud Messaging lets the hub wake the phone to share it. Its
// config is the Firebase console's google-services.json, kept out of the
// repository: ORIEL_GOOGLE_SERVICES_JSON names the file, or put it at
// app/google-services.json (ignored by git). Without one, push stays off.
val firebaseConfig: Map<String, String> = run {
    val path = System.getenv("ORIEL_GOOGLE_SERVICES_JSON")?.takeIf { it.isNotBlank() }
    val source = path?.let(::file) ?: file("google-services.json")
    if (!source.isFile) return@run emptyMap()
    @Suppress("UNCHECKED_CAST")
    val json = groovy.json.JsonSlurper().parse(source) as Map<String, Any?>
    val project = json["project_info"] as Map<String, Any?>
    @Suppress("UNCHECKED_CAST")
    val client = (json["client"] as List<Map<String, Any?>>).first { client ->
        val info = client["client_info"] as Map<String, Any?>
        (info["android_client_info"] as Map<String, Any?>)["package_name"] == "org.fruitiex.oriel"
    }
    @Suppress("UNCHECKED_CAST")
    val apiKey = (client["api_key"] as List<Map<String, Any?>>).first()["current_key"]
    mapOf(
        "FIREBASE_APP_ID" to (client["client_info"] as Map<String, Any?>)["mobilesdk_app_id"].toString(),
        "FIREBASE_API_KEY" to apiKey.toString(),
        "FIREBASE_PROJECT_ID" to project["project_id"].toString(),
        "FIREBASE_SENDER_ID" to project["project_number"].toString(),
    )
}

android {
    namespace = "org.fruitiex.oriel"
    compileSdk = 37
    buildToolsVersion = "37.0.0"
    ndkVersion = "30.0.16248370"

    defaultConfig {
        applicationId = "org.fruitiex.oriel"
        minSdk = 28
        targetSdk = 37
        versionCode = System.getenv("ORIEL_VERSION_CODE")?.toIntOrNull() ?: 1
        versionName = System.getenv("ORIEL_VERSION_NAME") ?: "0.1.0"
        for (key in listOf("FIREBASE_APP_ID", "FIREBASE_API_KEY", "FIREBASE_PROJECT_ID", "FIREBASE_SENDER_ID")) {
            buildConfigField("String", key, "\"${firebaseConfig[key].orEmpty()}\"")
        }
    }

    // ORIEL_ANDROID_KEYSTORE (+ _PASS, _ALIAS): the release key
    // (scripts/android-release.sh fetches it); else the debug key.
    val keystore = System.getenv("ORIEL_ANDROID_KEYSTORE")?.takeIf { it.isNotBlank() }
    signingConfigs {
        if (keystore != null) {
            create("release") {
                storeFile = file(keystore)
                storePassword = System.getenv("ORIEL_ANDROID_KEYSTORE_PASS")
                keyAlias = System.getenv("ORIEL_ANDROID_KEYSTORE_ALIAS") ?: "oriel"
                keyPassword = System.getenv("ORIEL_ANDROID_KEYSTORE_PASS")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.findByName("release") ?: signingConfigs.getByName("debug")
        }
    }

    sourceSets["main"].apply {
        jniLibs.srcDir(shared.resolve("jniLibs"))
        res.srcDir(shared.resolve("icons"))
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    buildFeatures {
        buildConfig = true
    }
    // Extract the native libraries at install (AGP 9 refuses the manifest's
    // extractNativeLibs).
    packaging {
        jniLibs {
            useLegacyPackaging = true
        }
    }
}

kotlin {
    compilerOptions {
        jvmTarget = JvmTarget.JVM_17
    }
}

dependencies {
    implementation(libs.firebase.messaging)
}
