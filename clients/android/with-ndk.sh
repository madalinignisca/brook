#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
# SPDX-License-Identifier: AGPL-3.0-or-later

# Run a command with the environment cargo needs to cross-compile for Android.
#
# Usage: with-ndk.sh <ndk dir> <command...>
#   e.g. with-ndk.sh "$HOME/Android/Sdk/ndk/30.0.16248370" cargo build --target aarch64-linux-android
#
# Why this exists: cargo does not know where the NDK is. Crates that compile C code (ring for
# TLS, the bundled SQLite) go through the `cc` crate, which looks for a compiler and archiver
# named after the target (CC_<target>, AR_<target>); without them it finds nothing for an
# Android target and the build fails. Rust itself needs the NDK clang as the linker
# (CARGO_TARGET_<TARGET>_LINKER).
#
# Why API 33: it is the app's minimum Android version. Building against it means the .so never
# calls libc functions newer than the oldest device we support.
#
# The NDK directory is required and never guessed: building against whatever NDK happens to be
# found would give builds that differ between machines.

set -eu

if [ "$#" -lt 2 ]; then
    echo "usage: $0 <ndk dir> <command...>" >&2
    exit 2
fi

ndk=$1
shift

if [ ! -d "$ndk" ]; then
    echo "with-ndk.sh: NDK directory not found: $ndk" >&2
    exit 1
fi

bin=$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin
if [ ! -x "$bin/llvm-ar" ]; then
    echo "with-ndk.sh: $ndk does not look like an NDK (missing $bin/llvm-ar)" >&2
    exit 1
fi

export CC_aarch64_linux_android="$bin/aarch64-linux-android33-clang"
export AR_aarch64_linux_android="$bin/llvm-ar"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$bin/aarch64-linux-android33-clang"

export CC_x86_64_linux_android="$bin/x86_64-linux-android33-clang"
export AR_x86_64_linux_android="$bin/llvm-ar"
export CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER="$bin/x86_64-linux-android33-clang"

exec "$@"
