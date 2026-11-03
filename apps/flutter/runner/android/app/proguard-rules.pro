# The release APK is shrunk and obfuscated by R8 (Flutter's Gradle plugin
# adds this file to its rules). What native code calls by name stays as it
# is: R8 sees no Java caller and would rename or drop it.

# The tunnel host's JNI (crates/tunnel-host src/android.rs): its native
# methods, and the listener's protect and onExit, which it calls back.
-keep class com.peakpassvpn.ppvpn.tunnel.TunnelHost { *; }
-keep interface com.peakpassvpn.ppvpn.tunnel.TunnelHost$Listener { *; }
-keep class * implements com.peakpassvpn.ppvpn.tunnel.TunnelHost$Listener { *; }

# The saved sign-in's key (apps/flutter/rust src/android.rs): seal and open.
-keep class com.peakpassvpn.ppvpn.Credentials { *; }

# The self-hosted push agent runs in the VPN service and calls this listener.
-keep class com.peakpassvpn.ppvpn.NativePushAgent { *; }
-keep interface com.peakpassvpn.ppvpn.NativePushAgent$Listener { *; }
-keep class * implements com.peakpassvpn.ppvpn.NativePushAgent$Listener { *; }

-keep class com.peakpassvpn.ppvpn.NativePushToken { *; }
