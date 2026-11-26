# `:core-bindings`

Android library module wrapping the Rust `lib/client-core` native library (`libclient_core.so`)
and UniFFI-generated Kotlin bindings (`org.soulbeet.core`), plus a testable Kotlin facade
(`org.soulbeet.corebindings.CoreClient` & `SoulbeetCoreProvider`).

## Regenerating Native Libraries & UniFFI Kotlin Bindings

Generated `jniLibs/` and `src/main/kotlin/org/soulbeet/core/client_core.kt` are gitignored.
Before compiling Android modules locally or in CI, run from the repository root:

```bash
# Fast local debug build (single ABI)
./scripts/build-android-core.sh --profile debug --abis arm64-v8a

# Full release build (arm64-v8a, armeabi-v7a, x86_64)
./scripts/build-android-core.sh --profile release
```

Or invoke the convenience Gradle task from `android/`:

```bash
./gradlew :core-bindings:generateRustBindings
```
