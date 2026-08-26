#!/usr/bin/env bash
# Fetch Espressif's QEMU fork.
#
# Stock qemu-system-xtensa has no ESP machines at all -- `-machine esp32s3`
# only exists in Espressif's build. Lands in tools/, which is gitignored.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
qemu="$root/tools/qemu/bin/qemu-system-xtensa"

if [ -x "$qemu" ] && "$qemu" -machine help 2>/dev/null | grep -q esp32s3; then
    echo "Espressif QEMU already present: $qemu"
    exit 0
fi

echo "Fetching Espressif QEMU (xtensa) ..."
mkdir -p "$root/tools"
url="$(curl -fsSL https://api.github.com/repos/espressif/qemu/releases/latest \
    | python3 -c "
import json,sys
for a in json.load(sys.stdin)['assets']:
    if a['name'].startswith('qemu-xtensa-softmmu') and 'x86_64-linux-gnu' in a['name']:
        print(a['browser_download_url']); break")"

[ -n "$url" ] || { echo "could not find a linux-x86_64 xtensa asset" >&2; exit 1; }

curl -fsSL "$url" -o "$root/tools/qemu-xtensa.tar.xz"
tar -xf "$root/tools/qemu-xtensa.tar.xz" -C "$root/tools"
rm -f "$root/tools/qemu-xtensa.tar.xz"

"$qemu" -machine help | grep esp32s3
echo "Installed: $qemu"
