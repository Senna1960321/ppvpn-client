plugins { id("com.android.application") version "9.1.0" }

android {
    namespace = "com.peakpassvpn.ppvpn"
    compileSdk = 36
    defaultConfig {
        applicationId = "com.peakpassvpn.ppvpn"
        minSdk = 26
        targetSdk = 36
        versionCode = 100
        versionName = "0.4.0"
    }
    // The production flavor generator inserts its declarations here.
    buildTypes {
        debug { }
    }
    testOptions {
        unitTests.isIncludeAndroidResources = true
        unitTests.all {
            it.maxHeapSize = "1g"
            it.systemProperty("robolectric.dependency.repo.url", "https://repo.maven.apache.org/maven2")
            it.testLogging.events("passed", "failed", "skipped")
            it.jvmArgs("--add-opens=java.base/java.lang=ALL-UNNAMED",
                "--add-opens=java.base/java.util=ALL-UNNAMED", "--add-opens=java.base/java.io=ALL-UNNAMED",
                "--add-opens=java.base/java.net=ALL-UNNAMED", "--add-opens=java.base/java.security=ALL-UNNAMED",
                "--add-opens=java.base/jdk.internal.access=ALL-UNNAMED")
        }
    }
}

dependencies {
    implementation(files(System.getenv("PPVPN_TEST_FLUTTER_JAR")))
    testImplementation("junit:junit:4.13.2")
    testImplementation("org.robolectric:robolectric:4.17")
}
