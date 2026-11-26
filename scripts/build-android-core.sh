#!/usr/bin/env bash
set -euo pipefail

# Cross-compiles `lib/client-core` into Android JNI `.so` libraries via `cargo-ndk`
# and generates idiomatic Kotlin bindings via UniFFI (`org.soulbeet.core`).

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

PROFILE="release"
ABIS="arm64-v8a armeabi-v7a x86_64"
MIN_SDK="26"
JNI_OUT_DIR="${REPO_ROOT}/android/core-bindings/src/main/jniLibs"
KOTLIN_OUT_DIR="${REPO_ROOT}/android/core-bindings/src/main/kotlin"

usage() {
  cat <<EOF
Usage: $(basename "$0") [OPTIONS]

Options:
  --profile debug|release      Cargo build profile (default: release)
  --abis "abi1 abi2 ..."       Space-separated Android ABIs (default: "arm64-v8a armeabi-v7a x86_64")
  --min-sdk LEVEL              Android API platform level (default: 26)
  --jni-out DIR                Output directory for jniLibs (default: android/core-bindings/src/main/jniLibs)
  --kotlin-out DIR             Output directory for Kotlin bindings (default: android/core-bindings/src/main/kotlin)
  -h, --help                   Show this help message
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --profile)
      PROFILE="${2:-}"
      shift 2
      ;;
    --abis)
      ABIS="${2:-}"
      shift 2
      ;;
    --min-sdk)
      MIN_SDK="${2:-}"
      shift 2
      ;;
    --jni-out)
      JNI_OUT_DIR="${2:-}"
      shift 2
      ;;
    --kotlin-out)
      KOTLIN_OUT_DIR="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Error: unknown argument '$1'" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if [[ "${PROFILE}" != "debug" && "${PROFILE}" != "release" ]]; then
  echo "Error: --profile must be 'debug' or 'release' (got '${PROFILE}')" >&2
  exit 1
fi

if ! command -v cargo-ndk >/dev/null 2>&1; then
  echo "Error: cargo-ndk is not installed. Install it with: cargo install cargo-ndk" >&2
  exit 1
fi

# Auto-discover ANDROID_NDK_HOME if not explicitly set.
discover_ndk() {
  if [[ -n "${ANDROID_NDK_HOME:-}" && -d "${ANDROID_NDK_HOME}" ]]; then
    echo "${ANDROID_NDK_HOME}"
    return 0
  fi

  local candidates=()
  if [[ -n "${ANDROID_HOME:-}" && -d "${ANDROID_HOME}/ndk" ]]; then
    while IFS= read -r dir; do
      candidates+=("$dir")
    done < <(find "${ANDROID_HOME}/ndk" -mindepth 1 -maxdepth 1 -type d | sort -rV)
  fi
  if [[ -d "${HOME}/Android/Sdk/ndk" ]]; then
    while IFS= read -r dir; do
      candidates+=("$dir")
    done < <(find "${HOME}/Android/Sdk/ndk" -mindepth 1 -maxdepth 1 -type d | sort -rV)
  fi
  if [[ -d "${HOME}/Android/ndk" ]]; then
    while IFS= read -r dir; do
      candidates+=("$dir")
    done < <(find "${HOME}/Android/ndk" -mindepth 1 -maxdepth 1 -type d | sort -rV)
  fi

  for dir in "${candidates[@]}"; do
    if [[ -d "${dir}/toolchains/llvm/prebuilt" ]]; then
      echo "${dir}"
      return 0
    fi
  done

  return 1
}

if ! ANDROID_NDK_HOME="$(discover_ndk)"; then
  cat >&2 <<EOF
Error: Android NDK not found.
Set ANDROID_NDK_HOME or install the NDK under \$ANDROID_HOME/ndk/, ~/Android/Sdk/ndk/, or ~/Android/ndk/.
Example:
  export ANDROID_NDK_HOME="\${HOME}/Android/Sdk/ndk/27.2.12479018"
EOF
  exit 1
fi
export ANDROID_NDK_HOME

echo "Using ANDROID_NDK_HOME=${ANDROID_NDK_HOME}"
echo "Building client-core (profile=${PROFILE}, min-sdk=${MIN_SDK}, abis=${ABIS})"

mkdir -p "${JNI_OUT_DIR}" "${KOTLIN_OUT_DIR}"

CARGO_PROFILE_FLAG=()
if [[ "${PROFILE}" == "release" ]]; then
  CARGO_PROFILE_FLAG=(--release)
fi

cd "${REPO_ROOT}"

# 1. Cross-compile JNI shared libraries for each requested Android ABI.
NDK_TARGET_FLAGS=()
for abi in ${ABIS}; do
  NDK_TARGET_FLAGS+=(-t "${abi}")
done

cargo ndk \
  --platform "${MIN_SDK}" \
  "${NDK_TARGET_FLAGS[@]}" \
  -o "${JNI_OUT_DIR}" \
  build -p client-core "${CARGO_PROFILE_FLAG[@]}"

for abi in ${ABIS}; do
  so_path="${JNI_OUT_DIR}/${abi}/libclient_core.so"
  if [[ ! -s "${so_path}" ]]; then
    echo "Error: expected JNI library missing or empty: ${so_path}" >&2
    exit 1
  fi
  echo "Built ${so_path} ($(du -h "${so_path}" | cut -f1))"
done

# 2. Build host cdylib and run uniffi-bindgen to generate Kotlin bindings.
echo "Building host client-core cdylib for UniFFI metadata extraction..."
cargo build -p client-core --lib

case "$(uname -s)" in
  Darwin) HOST_LIB="${REPO_ROOT}/target/debug/libclient_core.dylib" ;;
  MINGW*|MSYS*|CYGWIN*) HOST_LIB="${REPO_ROOT}/target/debug/client_core.dll" ;;
  *) HOST_LIB="${REPO_ROOT}/target/debug/libclient_core.so" ;;
esac

if [[ ! -s "${HOST_LIB}" ]]; then
  echo "Error: host library not found at ${HOST_LIB}" >&2
  exit 1
fi

echo "Generating Kotlin bindings into ${KOTLIN_OUT_DIR}..."
cargo run -p client-core --features bindgen-cli --bin uniffi-bindgen -- \
  generate \
  --library "${HOST_LIB}" \
  --config "${REPO_ROOT}/lib/client-core/uniffi.toml" \
  --language kotlin \
  --out-dir "${KOTLIN_OUT_DIR}"

GENERATED_KT="${KOTLIN_OUT_DIR}/org/soulbeet/core/client_core.kt"
if [[ ! -s "${GENERATED_KT}" ]]; then
  echo "Error: expected generated Kotlin file missing or empty: ${GENERATED_KT}" >&2
  exit 1
fi

echo "Verified Kotlin bindings: ${GENERATED_KT} ($(wc -l < "${GENERATED_KT}") lines)"
