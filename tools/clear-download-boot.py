#!/usr/bin/env python3
"""Clear RTC_CNTL_OPTION1.FORCE_DOWNLOAD_BOOT on the ESP32-S3 and reset into normal boot.

The firmware's "reboot into download mode" (Ctrl+Alt+D) sets this bit and reboots.
This bit is not cleared by a reset over USB, so otherwise the chip would boot into download
mode every time, even after flashing. esptool clears it itself on reset, but espflash does not,
so clear it with this script after flashing.

Usage: clear-download-boot.py [PORT]   (default /dev/ttyACM0)

Use while the chip is running the ROM download mode or the espflash stub.
"""

import struct
import sys
import time

import serial

RTC_CNTL_OPTION1_REG = 0x6000812C
FORCE_DOWNLOAD_BOOT_MASK = 0x1

# RTC watchdog (the ROM on the USB-OTG side cannot be reset via RTS, so reset with this)
RTC_CNTL_WDTCONFIG0_REG = 0x60008098
RTC_CNTL_WDTCONFIG1_REG = 0x6000809C
RTC_CNTL_WDTWPROTECT_REG = 0x600080B0
RTC_CNTL_WDT_WKEY = 0x50D83AA1

CMD_WRITE_REG = 0x09
CMD_SYNC = 0x08


def slip_encode(data: bytes) -> bytes:
    out = bytearray(b"\xc0")
    for b in data:
        if b == 0xC0:
            out += b"\xdb\xdc"
        elif b == 0xDB:
            out += b"\xdb\xdd"
        else:
            out.append(b)
    out += b"\xc0"
    return bytes(out)


def read_frame(port: serial.Serial, timeout: float) -> bytes | None:
    """Read one SLIP frame. Returns None on timeout."""
    deadline = time.monotonic() + timeout
    frame = bytearray()
    in_frame = False
    escape = False
    while time.monotonic() < deadline:
        b = port.read(1)
        if not b:
            continue
        c = b[0]
        if not in_frame:
            if c == 0xC0:
                in_frame = True
                frame.clear()
            continue
        if c == 0xC0:
            if frame:
                return bytes(frame)
            continue
        if escape:
            frame.append({0xDC: 0xC0, 0xDD: 0xDB}.get(c, c))
            escape = False
        elif c == 0xDB:
            escape = True
        else:
            frame.append(c)
    return None


def command(port: serial.Serial, op: int, data: bytes, timeout: float = 0.5) -> bytes | None:
    """Send a command and return the body (including status) of the response with the same op."""
    try:
        port.write(slip_encode(struct.pack("<BBHI", 0x00, op, len(data), 0) + data))
    except serial.SerialTimeoutException:
        # If the app is running, received data is not read and writes get stuck
        return None
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        frame = read_frame(port, deadline - time.monotonic())
        if frame is None:
            return None
        if len(frame) >= 8 and frame[0] == 0x01 and frame[1] == op:
            return frame[8:]
    return None


def sync(port: serial.Serial) -> bool:
    payload = b"\x07\x07\x12\x20" + b"\x55" * 32
    for _ in range(10):
        if command(port, CMD_SYNC, payload, timeout=0.2) is not None:
            # SYNC returns multiple responses, so read and discard them
            time.sleep(0.05)
            port.reset_input_buffer()
            return True
    return False


def write_reg(port: serial.Serial, addr: int, value: int, mask: int = 0xFFFFFFFF) -> bool:
    resp = command(port, CMD_WRITE_REG, struct.pack("<IIII", addr, value, mask, 0))
    return resp is not None and len(resp) >= 2 and resp[0] == 0


def rtc_wdt_reset(port: serial.Serial) -> None:
    """Reset the chip with the RTC watchdog (same procedure as esptool).

    Works over both USB-Serial-JTAG and USB-OTG. An RTC watchdog reset does not
    clear FORCE_DOWNLOAD_BOOT, so clear it first.
    """
    write_reg(port, RTC_CNTL_WDTWPROTECT_REG, RTC_CNTL_WDT_WKEY)
    write_reg(port, RTC_CNTL_WDTCONFIG1_REG, 5000)
    write_reg(port, RTC_CNTL_WDTCONFIG0_REG, (1 << 31) | (5 << 28) | (1 << 8) | 2)
    write_reg(port, RTC_CNTL_WDTWPROTECT_REG, 0)


def main() -> int:
    name = sys.argv[1] if len(sys.argv) > 1 else "/dev/ttyACM0"
    port = serial.Serial()
    port.port = name
    port.baudrate = 115200
    port.timeout = 0.05
    port.write_timeout = 0.5
    # USB-Serial-JTAG treats "RTS=1, DTR=0" as reset. pyserial sets DTR then RTS when opening,
    # so open with DTR asserted, lower RTS first, then lower DTR
    port.dtr = True
    port.rts = False
    port.open()
    port.dtr = False
    try:
        if not sync(port):
            # Neither the ROM nor the stub responds = the bit is not set and the app booted normally
            print("Not in download mode (already booted normally).")
            return 0
        if not write_reg(port, RTC_CNTL_OPTION1_REG, 0, FORCE_DOWNLOAD_BOOT_MASK):
            print("WRITE_REG failed", file=sys.stderr)
            return 1
        print("Cleared FORCE_DOWNLOAD_BOOT. Resetting.")
        rtc_wdt_reset(port)
        return 0
    finally:
        port.close()


if __name__ == "__main__":
    sys.exit(main())
