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

require_opt_arg() {
  if [[ "$2" -lt 2 ]]; then
    echo "Error: option '$1' requires a value" >&2
    usage >&2
    exit 1
  fi
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --profile)
      require_opt_arg "$1" "$#"
      PROFILE="$2"
      shift 2
      ;;
    --abis)
      require_opt_arg "$1" "$#"
      ABIS="$2"
      shift 2
      ;;
    --min-sdk)
      require_opt_arg "$1" "$#"
      MIN_SDK="$2"
      shift 2
      ;;
    --jni-out)
      require_opt_arg "$1" "$#"
      JNI_OUT_DIR="$2"
      shift 2
      ;;
    --kotlin-out)
      require_opt_arg "$1" "$#"
      KOTLIN_OUT_DIR="$2"
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

# Sort NDK subdirectories newest-first using portable numeric version sort (no GNU `sort -V`).
list_ndk_versions_desc() {
  local root="$1"
  [[ -d "${root}" ]] || return 0
  find "${root}" -mindepth 1 -maxdepth 1 -type d -print \
    | awk -F/ '{ print $NF "\t" $0 }' \
    | sort -t. -k1,1nr -k2,2nr -k3,3nr \
    | cut -f2-
}

# Auto-discover ANDROID_NDK_HOME if not explicitly set.
discover_ndk() {
  if [[ -n "${ANDROID_NDK_HOME:-}" && -d "${ANDROID_NDK_HOME}" ]]; then
    echo "${ANDROID_NDK_HOME}"
    return 0
  fi

  local candidates=()
  local search_roots=()
  if [[ -n "${ANDROID_HOME:-}" ]]; then
    search_roots+=("${ANDROID_HOME}/ndk")
  fi
  search_roots+=(
    "${HOME}/Android/Sdk/ndk"
    "${HOME}/Library/Android/sdk/ndk"
    "${HOME}/Android/ndk"
  )

  local root dir
  for root in "${search_roots[@]}"; do
    while IFS= read -r dir; do
      [[ -n "${dir}" ]] && candidates+=("${dir}")
    done < <(list_ndk_versions_desc "${root}")
  done

  for dir in ${candidates[@]+"${candidates[@]}"}; do
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
Set ANDROID_NDK_HOME or install the NDK under:
  \$ANDROID_HOME/ndk/
  ~/Android/Sdk/ndk/
  ~/Library/Android/sdk/ndk/
  ~/Android/ndk/
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

if [[ "${#NDK_TARGET_FLAGS[@]}" -eq 0 ]]; then
  echo "Error: --abis must specify at least one Android ABI" >&2
  exit 1
fi

cargo ndk \
  --platform "${MIN_SDK}" \
  "${NDK_TARGET_FLAGS[@]}" \
  -o "${JNI_OUT_DIR}" \
  build -p client-core ${CARGO_PROFILE_FLAG[@]+"${CARGO_PROFILE_FLAG[@]}"}

LLVM_STRIP=""
if [[ "${PROFILE}" == "release" ]]; then
  LLVM_STRIP="$(find "${ANDROID_NDK_HOME}/toolchains/llvm/prebuilt" -maxdepth 3 -name llvm-strip -type f 2>/dev/null | head -n 1 || true)"
fi

for abi in ${ABIS}; do
  so_path="${JNI_OUT_DIR}/${abi}/libclient_core.so"
  if [[ ! -s "${so_path}" ]]; then
    echo "Error: expected JNI library missing or empty: ${so_path}" >&2
    exit 1
  fi
  if [[ "${PROFILE}" == "release" && -n "${LLVM_STRIP}" ]]; then
    "${LLVM_STRIP}" --strip-unneeded "${so_path}"
  fi
  echo "Built ${so_path} ($(du -h "${so_path}" | cut -f1))"
done

# 2. Build host cdylib and run uniffi-bindgen to generate Kotlin bindings.
echo "Building host client-core cdylib for UniFFI metadata extraction..."
cargo build -p client-core --lib

TARGET_DIR="${CARGO_TARGET_DIR:-${REPO_ROOT}/target}"
case "$(uname -s)" in
  Darwin) HOST_LIB="${TARGET_DIR}/debug/libclient_core.dylib" ;;
  MINGW*|MSYS*|CYGWIN*) HOST_LIB="${TARGET_DIR}/debug/client_core.dll" ;;
  *) HOST_LIB="${TARGET_DIR}/debug/libclient_core.so" ;;
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
