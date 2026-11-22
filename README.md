# card-fdd

Firmware that turns an M5Stack Cardputer into a USB floppy drive.

Tested on the Cardputer v1.1. Compatibility with other Cardputer revisions is unverified.

## Quick start

1. Use a MicroSD card with an MBR partition table and a FAT16 or FAT32 first partition. Copy raw, headerless 1.44 MB floppy images (1,474,560 bytes each, with an `.img` or `.ima` extension) into a `FLOPPY` folder on that partition.
2. Build and flash (see below).
3. Connect to the PC, press `Tab`, pick an image, and press `Enter`.

## Keys

| Key | Action |
|-----|--------|
| `Tab` | Switch between the status screen and the file selector |
| `;` `.` / `,` `/` | Move up / down, page up / down |
| `Enter` / `` ` `` | Open a folder or insert an image / go up |
| `e` / `w` | Eject (press twice to force) / toggle write protection |
| `s` / `r` | Volume / realistic speed mode |
| `n` / `i` | Create a blank image / show boot sector info |
| `Ctrl+Alt+D` / `Ctrl+Alt+R` | Reboot into download mode / reboot |

## Configuration

Settings live in `CARDFDD.CFG` (`key=value`) at the root of the SD card's first partition. If the file is missing, the firmware starts with defaults and creates the file when it saves a change, such as a new volume setting. You can also create the file manually.
Main keys: `image_dir`, `auto_mount`, `volume`, `usb_mode` (`ufi-cbi` / `ufi-bot` / `scsi`), and `usb_vid` / `usb_pid`.

To use it as `A:` on Windows, use the normal build (without `dev-cdc`). Set `usb_mode=ufi-cbi` in `CARDFDD.CFG` and use the VID and PID of a USB floppy drive listed in `usbstor.inf`. The following settings were tested with Windows 11 using the TEAC drive IDs.

```ini
usb_mode=ufi-cbi
usb_vid=0x0644
usb_pid=0x0000
```

Restart the device to apply the settings.

After the fix for Windows USB resets with no disk inserted, the device stayed configured for the full 30.7-second hardware check. The results are in section 8.4 of [PLAN.md](docs/PLAN.md).

## Build

Requires the `esp` toolchain ([espup](https://github.com/esp-rs/espup)), [espflash](https://github.com/esp-rs/espflash), and Python 3 + pyserial.

```bash
source ~/export-esp.sh
```

For the first flash, switch the Cardputer off, then hold `BtnG0` while plugging in the USB cable to enter download mode.

Normal / Windows build:

```bash
cargo run --release
```

Development build (adds CDC-ACM for logs and the reboot command):

```bash
cargo run --release --features dev-cdc
```

`cargo run` flashes through [`tools/flash.sh`](tools/flash.sh) and then shows logs (via CDC-ACM in the development build). If the Cardputer is already running card-fdd without CDC-ACM, press `Ctrl+Alt+D` on the device to enter download mode before flashing.

## Known issues

- The default VID/PID (`1209:0001`) is for testing only.

See [docs/](docs/) (Japanese) for design details.

## License

MIT License
