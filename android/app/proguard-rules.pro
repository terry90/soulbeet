# Keep UniFFI / JNA bindings and Kotlin Compose app classes intact when shrinking is enabled.
-keep class org.soulbeet.core.** { *; }
-keep class com.sun.jna.** { *; }
-keep class * implements com.sun.jna.** { *; }
