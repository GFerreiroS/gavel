#!/usr/bin/env bash
# Portability gate for cluster-core.
#
# Three levels, each catching what the one before it cannot:
#
#   1. host --no-default-features  proves the crate does not USE std
#   2. cross-compile               proves it can BE BUILT for a device
#   3. scripts/qemu-test.sh        proves it RUNS correctly on one
#
# Level 1 alone is not enough, and that is not hypothetical: it passed happily
# while the crate could not build for riscv32imc at all, because RoundRobin
# held an AtomicUsize and the ESP32-C3 has no atomic instructions.
#
#   xtensa-esp32s3  ESP32-S3          <- the primary target
#   riscv32imc      ESP32-C3, C2      (no atomics)
#   riscv32imac     ESP32-C6, H2      (atomics)
set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
status=0

report() { printf '%-38s %s\n' "$1" "$2"; }

printf '%-38s ' "host (no_std, no default features)"
if cargo check -p cluster-core --no-default-features >/dev/null 2>&1; then
    echo "PASS"
else
    echo "FAIL"; status=1
fi

# --- ESP32-S3: the primary target -------------------------------------------
printf '%-38s ' "xtensa-esp32s3-none-elf"
if rustup toolchain list 2>/dev/null | grep -q '^esp'; then
    # shellcheck disable=SC1091
    . "$root/scripts/esp-env.sh" >/dev/null 2>&1
    # Xtensa has no prebuilt core, hence build-std.
    if cargo +esp check -p cluster-core --no-default-features \
        --target xtensa-esp32s3-none-elf -Z build-std=core,alloc >/dev/null 2>&1; then
        echo "PASS"
    else
        echo "FAIL"; status=1
    fi
else
    echo "SKIP (cargo install espup && espup install)"
fi

# --- the other families, so the core does not quietly become S3-only --------
for target in riscv32imc-unknown-none-elf riscv32imac-unknown-none-elf; do
    printf '%-38s ' "$target"
    if ! rustup target list --installed 2>/dev/null | grep -qx "$target"; then
        echo "SKIP (rustup target add $target)"
        continue
    fi
    if cargo check -p cluster-core --no-default-features --target "$target" >/dev/null 2>&1; then
        echo "PASS"
    else
        echo "FAIL"; status=1
    fi
done

# --- and that it actually runs ----------------------------------------------
printf '%-38s ' "runs on emulated ESP32-S3"
if [ -x "$root/tools/qemu/bin/qemu-system-xtensa" ] && rustup toolchain list 2>/dev/null | grep -q '^esp'; then
    if "$root/scripts/qemu-test.sh" >/dev/null 2>&1; then
        echo "PASS"
    else
        echo "FAIL"; status=1
    fi
else
    echo "SKIP (scripts/fetch-qemu.sh)"
fi

exit $status
