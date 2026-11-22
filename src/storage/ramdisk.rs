//! Virtual floppy for development: a formatted, empty 1.44MB FAT12 disk.
//!
//! There isn't enough RAM to hold the whole disk, so the empty disk contents (boot sector, start of FAT, volume label)
//! are generated on the fly, and only written sectors are kept in RAM, up to [`OVERLAY_SECTORS`].
//! Used to test USB mass storage without an SD card.

use super::geometry::{FORMATS, Geometry};
use super::{Disk, DiskError};

/// Number of sectors that can hold writes (32KB).
pub const OVERLAY_SECTORS: usize = 64;
const SECTOR: usize = 512;

const GEOMETRY: Geometry = FORMATS[0].geometry; // 1.44MB
const VOLUME_LABEL: &[u8; 11] = b"CARDFDD    ";

// 1.44MB FAT12 layout
const RESERVED: u32 = 1;
const SECTORS_PER_FAT: u32 = 9;
const FAT1: u32 = RESERVED;
const FAT2: u32 = FAT1 + SECTORS_PER_FAT;
const ROOT_DIR: u32 = FAT2 + SECTORS_PER_FAT;

pub struct VirtualFloppy {
    lbas: [u32; OVERLAY_SECTORS],
    data: [[u8; SECTOR]; OVERLAY_SECTORS],
    used: usize,
    write_protected: bool,
    inserted: bool,
}

impl Default for VirtualFloppy {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualFloppy {
    pub const fn new() -> Self {
        Self {
            lbas: [0; OVERLAY_SECTORS],
            data: [[0; SECTOR]; OVERLAY_SECTORS],
            used: 0,
            write_protected: false,
            inserted: true,
        }
    }

    pub fn set_write_protected(&mut self, wp: bool) {
        self.write_protected = wp;
    }

    /// Insert / eject. Ejecting discards the written contents.
    pub fn set_inserted(&mut self, inserted: bool) {
        self.inserted = inserted;
        if !inserted {
            self.used = 0;
        }
    }

    pub fn is_inserted(&self) -> bool {
        self.inserted
    }

    /// Number of sectors holding writes.
    pub fn overlay_used(&self) -> usize {
        self.used
    }

    fn find(&self, lba: u32) -> Option<usize> {
        self.lbas[..self.used].iter().position(|&l| l == lba)
    }
}

/// Generate the contents of sector `lba` of a formatted, empty 1.44MB FAT12 disk.
pub fn blank_sector(lba: u32, buf: &mut [u8]) {
    buf.fill(0);
    match lba {
        0 => boot_sector(buf),
        FAT1 | FAT2 => buf[..3].copy_from_slice(&[0xF0, 0xFF, 0xFF]),
        ROOT_DIR => {
            buf[..11].copy_from_slice(VOLUME_LABEL);
            buf[11] = 0x08; // Volume label attribute
        }
        _ => {}
    }
}

fn boot_sector(b: &mut [u8]) {
    b[0..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
    b[3..11].copy_from_slice(b"CARDFDD ");
    b[11..13].copy_from_slice(&512u16.to_le_bytes()); // Bytes per sector
    b[13] = 1; // Sectors per cluster
    b[14..16].copy_from_slice(&(RESERVED as u16).to_le_bytes());
    b[16] = 2; // Number of FATs
    b[17..19].copy_from_slice(&224u16.to_le_bytes()); // Root directory entries
    b[19..21].copy_from_slice(&(GEOMETRY.total_sectors() as u16).to_le_bytes());
    b[21] = 0xF0; // Media descriptor
    b[22..24].copy_from_slice(&(SECTORS_PER_FAT as u16).to_le_bytes());
    b[24..26].copy_from_slice(&u16::from(GEOMETRY.sectors_per_track).to_le_bytes());
    b[26..28].copy_from_slice(&u16::from(GEOMETRY.heads).to_le_bytes());
    b[36] = 0x00; // Drive number
    b[38] = 0x29; // Extended boot signature
    b[39..43].copy_from_slice(&0x2026_1121u32.to_le_bytes()); // Volume ID
    b[43..54].copy_from_slice(VOLUME_LABEL);
    b[54..62].copy_from_slice(b"FAT12   ");
    b[510] = 0x55;
    b[511] = 0xAA;
}

impl Disk for VirtualFloppy {
    fn geometry(&self) -> Option<Geometry> {
        self.inserted.then_some(GEOMETRY)
    }

    fn write_protected(&self) -> bool {
        self.write_protected
    }

    fn read(&mut self, lba: u32, buf: &mut [u8]) -> Result<(), DiskError> {
        if !self.inserted {
            return Err(DiskError::NoMedium);
        }
        let buf = &mut buf[..SECTOR];
        match self.find(lba) {
            Some(i) => buf.copy_from_slice(&self.data[i]),
            None => blank_sector(lba, buf),
        }
        Ok(())
    }

    fn write(&mut self, lba: u32, buf: &[u8]) -> Result<(), DiskError> {
        if !self.inserted {
            return Err(DiskError::NoMedium);
        }
        let i = match self.find(lba) {
            Some(i) => i,
            None if self.used < OVERLAY_SECTORS => {
                self.used += 1;
                self.lbas[self.used - 1] = lba;
                self.used - 1
            }
            None => {
                log::warn!("Virtual floppy write area is full (LBA {})", lba);
                return Err(DiskError::Write);
            }
        };
        self.data[i].copy_from_slice(&buf[..SECTOR]);
        Ok(())
    }
}
