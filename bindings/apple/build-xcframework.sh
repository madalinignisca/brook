#!/usr/bin/env bash
# Build BrookCoreFFI.xcframework + the generated Swift sources for the BrookCore package.
#
# Apple Silicon only, by decision: no x86_64 slice is ever built. macOS ships first;
# the iOS slices (aarch64-apple-ios, aarch64-apple-ios-sim) are added to SLICES when the
# iOS client starts — each slice is one more `-library` entry below.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
PKG="$HERE/swift/BrookCore"
BUILD="$HERE/build"
SLICES=(aarch64-apple-darwin)
# Keep in step with Package.swift's `platforms`.
export MACOSX_DEPLOYMENT_TARGET=26.0

command -v xcodebuild >/dev/null || { echo "xcodebuild not found" >&2; exit 1; }
for t in "${SLICES[@]}"; do
  rustup target list --installed | grep -qx "$t" || { echo "missing Rust target: $t (rustup target add $t)" >&2; exit 1; }
done

rm -rf "$BUILD" "$PKG/BrookCoreFFI.xcframework" "$PKG/Sources/BrookCoreGenerated" "$PKG/Sources/BrookCore/Generated"
mkdir -p "$BUILD/headers" "$PKG/Sources/BrookCoreGenerated"

cd "$ROOT"
for t in "${SLICES[@]}"; do
  cargo build --release --locked -p brook-ffi --target "$t"
done
cargo build --release --locked -p brook-ffi --features cli --bin uniffi-bindgen-swift
BINDGEN="$ROOT/target/release/uniffi-bindgen-swift"

# Library mode: bindings are generated from the UniFFI metadata embedded in the built library.
META_LIB="$ROOT/target/${SLICES[0]}/release/libbrook_ffi.dylib"
"$BINDGEN" --swift-sources "$META_LIB" "$PKG/Sources/BrookCoreGenerated"
"$BINDGEN" --headers "$META_LIB" "$BUILD/headers"
# A plain (non-`framework`) module named as the generated Swift imports it: the xcframework
# wraps a static `.a` + headers, not framework bundles, so `--xcframework` does not apply.
"$BINDGEN" --modulemap --module-name brook_ffiFFI --modulemap-filename module.modulemap "$META_LIB" "$BUILD/headers"

LIBS=()
for t in "${SLICES[@]}"; do
  LIBS+=(-library "$ROOT/target/$t/release/libbrook_ffi.a" -headers "$BUILD/headers")
done
xcodebuild -create-xcframework "${LIBS[@]}" -output "$PKG/BrookCoreFFI.xcframework"

echo "built: $PKG/BrookCoreFFI.xcframework (${SLICES[*]})"
