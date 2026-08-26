#!/usr/bin/env bash
# Build the node firmware for ESP32-S3, run it under QEMU, and check the verdict.
#
# This is the gate host tests cannot provide: it proves `cluster-core` compiles
# for Xtensa, links into a real flash image, boots under the ESP-IDF bootloader,
# and produces correct answers on the device.
set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
deadline="${QEMU_TIMEOUT:-60}"

# shellcheck disable=SC1091
. "$root/scripts/esp-env.sh" || exit 1
"$root/scripts/fetch-qemu.sh" >/dev/null || exit 1

echo "building node firmware for xtensa-esp32s3-none-elf ..."
( cd "$root/firmware" && cargo +esp build --release ) || exit 1
elf="$root/firmware/target/xtensa-esp32s3-none-elf/release/node-firmware"

log="$(mktemp -t esp-qemu-XXXXXX.log)"
trap 'rm -f "$log"' EXIT

echo "booting on emulated ESP32-S3 ..."
# The firmware halts in a spin loop rather than powering off, so QEMU has no
# reason to exit. Watch for the halt marker and stop it ourselves; the deadline
# is the backstop for a firmware that never gets there.
"$root/scripts/qemu-run.sh" "$elf" >"$log" 2>&1 &
qemu_pid=$!

elapsed=0
while kill -0 "$qemu_pid" 2>/dev/null; do
    grep -q "=== node firmware halted ===" "$log" && break
    sleep 0.25
    elapsed=$(awk "BEGIN{print $elapsed + 0.25}")
    if awk "BEGIN{exit !($elapsed >= $deadline)}"; then
        echo "timed out after ${deadline}s" >&2
        break
    fi
done
kill "$qemu_pid" 2>/dev/null
wait "$qemu_pid" 2>/dev/null

# Show the firmware's own output, not the bootloader chatter.
sed -n '/=== esp-web-cluster node firmware ===/,/=== node firmware halted ===/p' "$log"

if grep -q "^RESULT: PASS" "$log"; then
    echo
    echo "device tests PASSED on emulated ESP32-S3"
    exit 0
fi

echo
echo "device tests FAILED: no PASS verdict in the boot log" >&2
exit 1
