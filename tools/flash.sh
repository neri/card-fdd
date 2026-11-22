#!/usr/bin/env bash
# cargo run runner: flash -> clear FORCE_DOWNLOAD_BOOT and boot -> show logs.
#
# When the firmware uses USB as OTG (mass storage + development CDC-ACM),
# send "download" to CDC-ACM to enter download mode before flashing.
#
# Environment variables:
#   ESPFLASH_PORT  port used for flashing (default: search /dev/serial/by-id/)
#   NO_MONITOR=1   do not show logs after flashing
set -euo pipefail

elf="$1"
tools="$(cd "$(dirname "$0")" && pwd)"
ROM_GLOB='/dev/serial/by-id/usb-Espressif_*'
CDC_GLOB='/dev/serial/by-id/usb-card-fdd_*'

first() { compgen -G "$1" | sort | head -1 || true; }

wait_for() {
    local pattern="$1" secs="$2" p
    for _ in $(seq $((secs * 10))); do
        p="$(first "$pattern")"
        if [[ -n "$p" ]]; then echo "$p"; return 0; fi
        sleep 0.1
    done
    return 1
}

port="${ESPFLASH_PORT:-$(first "$ROM_GLOB")}"
if [[ -z "$port" ]]; then
    cdc="$(first "$CDC_GLOB")"
    if [[ -z "$cdc" ]]; then
        echo "No flashing port found. Press Ctrl+Alt+D on the device to enter download mode." >&2
        port="$(wait_for "$ROM_GLOB" 60)" || { echo "Timed out" >&2; exit 1; }
    else
        echo "Entering download mode via CDC-ACM ($cdc)"
        # Right after boot, ModemManager etc. may be touching the port and the command can be lost, so send it several times
        for _ in 1 2 3; do
            python3 "$tools/monitor.py" --send download "$cdc" 2>/dev/null || true
            if port="$(wait_for "$ROM_GLOB" 4)"; then break; fi
        done
        [[ -n "$port" ]] || { echo "Download mode port did not appear" >&2; exit 1; }
    fi
fi
echo "Flashing port: $port"

espflash flash --chip esp32s3 --port "$port" --after no-reset "$elf"
# If the bit set by Ctrl+Alt+D remains, the chip boots into download mode even after flashing
python3 "$tools/clear-download-boot.py" "$port"

if [[ "${NO_MONITOR:-0}" != 1 ]]; then
    if cdc="$(wait_for "$CDC_GLOB" 10)"; then
        exec python3 "$tools/monitor.py" "$cdc"
    else
        exec python3 "$tools/monitor.py" --no-reset "$port"
    fi
fi
