#!/usr/bin/env bash
# Boot a firmware ELF on an emulated ESP32-S3.
#
# Also the cargo runner for the firmware crate, so `cargo +esp run` inside
# firmware/ boots QEMU instead of trying to flash a board that is not there.
#
#   scripts/qemu-run.sh <path-to-elf> [extra qemu args...]
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
elf="${1:?usage: qemu-run.sh <elf> [qemu args...]}"
shift || true

qemu="$root/tools/qemu/bin/qemu-system-xtensa"
[ -x "$qemu" ] || { echo "Espressif QEMU missing. Run scripts/fetch-qemu.sh" >&2; exit 1; }

# QEMU boots a flash image, not an ELF: the ESP-IDF bootloader has to find a
# partition table and an app descriptor where it expects them.
image="$(mktemp -t esp-flash-XXXXXX.bin)"
trap 'rm -f "$image"' EXIT

espflash save-image --chip esp32s3 --merge --flash-size 4mb "$elf" "$image" >/dev/null

exec "$qemu" \
    -nographic \
    -machine esp32s3 \
    -m 4M \
    -drive "file=$image,if=mtd,format=raw" \
    "$@"
