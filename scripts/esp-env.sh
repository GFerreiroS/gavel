#!/usr/bin/env bash
# Put the Xtensa toolchain on PATH.
#
# `espup install` writes ~/export-esp.sh; prefer it, but fall back to
# discovering the toolchain so a fresh checkout works either way.
# Source this, do not execute it.

if [ -f "$HOME/export-esp.sh" ]; then
    # shellcheck disable=SC1091
    . "$HOME/export-esp.sh"
    return 0 2>/dev/null || exit 0
fi

_gcc="$(find "$HOME/.rustup/toolchains/esp" -name 'xtensa-esp32s3-elf-gcc' -type f 2>/dev/null | head -1)"
if [ -n "$_gcc" ]; then
    PATH="$(dirname "$_gcc"):$PATH"
    export PATH
else
    echo "Xtensa toolchain not found. Install it with:" >&2
    echo "    cargo install espup && espup install" >&2
    return 1 2>/dev/null || exit 1
fi
