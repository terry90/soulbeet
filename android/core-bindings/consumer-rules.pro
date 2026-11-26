# Preserve UniFFI generated JNA interface & callback classes for R8/ProGuard.
-keep class org.soulbeet.core.** { *; }
-keep class com.sun.jna.** { *; }
-keep class * implements com.sun.jna.** { *; }
