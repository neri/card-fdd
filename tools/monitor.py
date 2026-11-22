#!/usr/bin/env python3
"""Simple monitor for reading serial logs.

- USB-Serial-JTAG port: reset via RTS before reading (so early boot logs are not missed)
- card-fdd development CDC-ACM port: read with DTR asserted (the firmware sends no logs until DTR is set)

If the port is omitted, search /dev/serial/by-id/ (CDC-ACM preferred).

Usage:
  monitor.py [--no-reset] [--seconds N] [PORT]
  monitor.py --send download [PORT]     CDC-ACM send one command line to CDC-ACM
"""

import argparse
import glob
import sys
import time

import serial

JTAG_GLOB = "/dev/serial/by-id/usb-Espressif_*"
CDC_GLOB = "/dev/serial/by-id/usb-card-fdd_*"


def find_port() -> str | None:
    for pattern in (CDC_GLOB, JTAG_GLOB):
        ports = sorted(glob.glob(pattern))
        if ports:
            return ports[0]
    return None


def is_cdc(port: str) -> bool:
    return "card-fdd" in port


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("port", nargs="?")
    ap.add_argument("--seconds", type=float, default=0, help="Exit after this many seconds (0 = until Ctrl+C)")
    ap.add_argument("--no-reset", action="store_true", help="Read without resetting (JTAG only)")
    ap.add_argument("--send", help="Send this command to CDC-ACM and exit")
    args = ap.parse_args()

    name = args.port or find_port()
    if name is None:
        print("No serial port found", file=sys.stderr)
        return 1

    port = serial.Serial()
    port.port = name
    port.baudrate = 115200
    port.timeout = 0.1
    port.write_timeout = 1
    cdc = is_cdc(name)
    # USB-Serial-JTAG treats "RTS=1, DTR=0" as reset, so open with DTR asserted and then lower it.
    # CDC-ACM (card-fdd) sends logs only while DTR is asserted, so keep it asserted
    port.dtr = True
    port.rts = False
    port.open()
    if not cdc:
        port.dtr = False
    try:
        if args.send:
            port.write(args.send.encode() + b"\n")
            port.flush()
            time.sleep(0.2)
            return 0
        if not cdc and not args.no_reset:
            port.rts = True
            time.sleep(0.1)
            port.rts = False
        deadline = time.monotonic() + args.seconds if args.seconds > 0 else None
        while deadline is None or time.monotonic() < deadline:
            data = port.read(1024)
            if data:
                sys.stdout.write(data.decode("utf-8", errors="replace").replace("\r", ""))
                sys.stdout.flush()
    except KeyboardInterrupt:
        pass
    except serial.SerialException as e:
        # The port disappeared (e.g. due to a reboot)
        print(f"\n[monitor] Port closed: {e}", file=sys.stderr)
    finally:
        port.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
