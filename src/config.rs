//! Reading and writing the config file `CARDFDD.CFG` (at the SD root) (hardware independent).
//!
//! Text with one `key=value` per line. Everything from `#` to the end of the line is a comment. Unknown keys are ignored.
//! If the file is missing, defaults are used, and the file is created when a setting changes.

use core::fmt::Write;

use heapless::String;

use crate::storage::browser::MAX_DEPTH;

/// Config file name (8.3 format).
pub const FILE_NAME: &str = "CARDFDD.CFG";

/// Maximum UTF-8 length of a FAT long name (255 UTF-16 code units).
pub const MAX_LFN_BYTES: usize = 255 * 3;
pub const IMAGE_PATH_PREFIX: &str = "v1:";
// Escaping a single ASCII character takes at most as much space as one UTF-16 unit in UTF-8.
const MAX_IMAGE_PATH: usize = IMAGE_PATH_PREFIX.len() + (MAX_DEPTH + 1) * (1 + MAX_LFN_BYTES);
pub type ImagePath = String<MAX_IMAGE_PATH>;

/// Maximum config file size.
pub const MAX_LEN: usize = MAX_IMAGE_PATH + 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Folder containing the images.
    pub image_dir: String<64>,
    /// Image inserted last time (escaped paths start with `v1:`; legacy paths are plain text).
    pub last_image: ImagePath,
    /// Insert the previous image at startup.
    pub auto_mount: bool,
    /// Write-protect switch.
    pub write_protect: bool,
    /// Access sound volume (0 = mute .. 3).
    pub volume: u8,
    /// Realistic speed mode (slow down to the speed of a real floppy drive).
    pub speed_emulation: bool,
    /// USB mass storage mode ("ufi-cbi" / "ufi-bot" / "scsi").
    pub usb_mode: String<16>,
    pub usb_vid: u16,
    pub usb_pid: u16,
    pub usb_manufacturer: String<32>,
    pub usb_product: String<32>,
}

/// The default USB VID/PID etc. are decided by the caller (build-time settings).
pub struct Defaults<'a> {
    pub usb_mode: &'a str,
    pub usb_vid: u16,
    pub usb_pid: u16,
    pub usb_manufacturer: &'a str,
    pub usb_product: &'a str,
}

impl Config {
    pub fn new(d: &Defaults) -> Self {
        Self {
            image_dir: s("/FLOPPY"),
            last_image: String::new(),
            auto_mount: true,
            write_protect: false,
            volume: 2,
            speed_emulation: false,
            usb_mode: s(d.usb_mode),
            usb_vid: d.usb_vid,
            usb_pid: d.usb_pid,
            usb_manufacturer: s(d.usb_manufacturer),
            usb_product: s(d.usb_product),
        }
    }

    /// Read the config file contents and override the defaults. Unreadable values are ignored and logged.
    ///
    /// Each line is decoded as UTF-8 on its own, so a line saved in another encoding (or cut off at
    /// [`MAX_LEN`]) only loses that line instead of every setting.
    pub fn apply(&mut self, text: &[u8]) {
        for (n, raw) in text.split(|&b| b == b'\n').enumerate() {
            let Ok(raw) = core::str::from_utf8(raw) else {
                log::warn!("{} line {}: not valid UTF-8; ignored", FILE_NAME, n + 1);
                continue;
            };
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                log::warn!("{} line {}: missing '='", FILE_NAME, n + 1);
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            let ok = match key {
                "image_dir" => set_str(&mut self.image_dir, value),
                "last_image" => set_str(&mut self.last_image, value),
                "auto_mount" => set_bool(&mut self.auto_mount, value),
                "write_protect" => set_bool(&mut self.write_protect, value),
                "volume" => value.parse::<u8>().map(|v| self.volume = v.min(3)).is_ok(),
                "speed_emulation" => set_bool(&mut self.speed_emulation, value),
                "usb_mode" => set_str(&mut self.usb_mode, value),
                "usb_vid" => set_hex(&mut self.usb_vid, value),
                "usb_pid" => set_hex(&mut self.usb_pid, value),
                "usb_manufacturer" => set_str(&mut self.usb_manufacturer, value),
                "usb_product" => set_str(&mut self.usb_product, value),
                _ => {
                    log::info!("{}: ignoring unknown setting {}", FILE_NAME, key);
                    true
                }
            };
            if !ok {
                log::warn!("{}: invalid value for {}: '{}'", FILE_NAME, key, value);
            }
        }
    }

    /// Write out the config file contents.
    pub fn write_to(&self, out: &mut String<MAX_LEN>) {
        out.clear();
        let _ = write!(
            out,
            "# card-fdd settings\n\
             # Folder that holds floppy images\n\
             image_dir={}\n\
             # Image inserted last time (set automatically, v1 paths use percent escapes)\n\
             last_image={}\n\
             # Insert the last image at startup (1/0)\n\
             auto_mount={}\n\
             # Write protect switch (1/0)\n\
             write_protect={}\n\
             # Drive sound volume: 0 (mute) to 3\n\
             volume={}\n\
             # Emulate the speed of a real floppy drive (1/0)\n\
             speed_emulation={}\n\
             # USB mode: ufi-cbi (USB floppy) / ufi-bot / scsi (removable disk)\n\
             # Changes to usb_* take effect after a restart.\n\
             usb_mode={}\n\
             # Windows recognizes a UFI floppy only with a known floppy VID/PID.\n\
             usb_vid=0x{:04x}\n\
             usb_pid=0x{:04x}\n\
             usb_manufacturer={}\n\
             usb_product={}\n",
            self.image_dir,
            self.last_image,
            self.auto_mount as u8,
            self.write_protect as u8,
            self.volume,
            self.speed_emulation as u8,
            self.usb_mode,
            self.usb_vid,
            self.usb_pid,
            self.usb_manufacturer,
            self.usb_product,
        );
    }
}

pub fn append_image_name(path: &mut ImagePath, name: &str) -> core::fmt::Result {
    path.push('/').map_err(|_| core::fmt::Error)?;
    for c in name.chars() {
        if c.is_ascii_control() || matches!(c, ' ' | '#' | '%') {
            write!(path, "%{:02X}", c as u8)?;
        } else {
            path.push(c).map_err(|_| core::fmt::Error)?;
        }
    }
    Ok(())
}

pub fn decode_image_name(value: &str) -> Option<String<MAX_LFN_BYTES>> {
    let mut name = String::new();
    let mut parts = value.split('%');
    name.push_str(parts.next()?).ok()?;
    for part in parts {
        let byte = u8::from_str_radix(part.get(..2)?, 16).ok()?;
        if !byte.is_ascii() || matches!(byte, b'/' | 0) {
            return None;
        }
        name.push(char::from(byte)).ok()?;
        name.push_str(&part[2..]).ok()?;
    }
    Some(name)
}

fn s<const N: usize>(v: &str) -> String<N> {
    let mut out = String::new();
    for c in v.chars() {
        if out.push(c).is_err() {
            break;
        }
    }
    out
}

fn set_str<const N: usize>(dst: &mut String<N>, value: &str) -> bool {
    if value.len() > N {
        return false;
    }
    *dst = s(value);
    true
}

fn set_bool(dst: &mut bool, value: &str) -> bool {
    match value {
        "1" | "true" | "yes" | "on" => *dst = true,
        "0" | "false" | "no" | "off" => *dst = false,
        _ => return false,
    }
    true
}

fn set_hex(dst: &mut u16, value: &str) -> bool {
    let digits = value.trim_start_matches("0x").trim_start_matches("0X");
    match u16::from_str_radix(digits, 16) {
        Ok(v) => {
            *dst = v;
            true
        }
        Err(_) => false,
    }
}
