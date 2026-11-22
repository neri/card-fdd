//! Storage layer: files on the SD card and floppy images.

pub mod bootsector;
pub mod browser;
pub mod fat;
pub mod geometry;
pub mod image;
pub mod ramdisk;

use core::cell::RefCell;

use geometry::Geometry;

/// Read/write failure of the disk (the medium exposed over USB).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskError {
    /// Read failed.
    Read,
    /// Write failed.
    Write,
    /// No medium present (removed midway).
    NoMedium,
}

/// The "floppy disk" seen over USB. Read and written in sectors.
pub trait Disk {
    /// Geometry of the medium. `None` if no medium is present.
    fn geometry(&self) -> Option<Geometry>;

    /// Whether it is write-protected.
    fn write_protected(&self) -> bool;

    /// Medium generation number. Changes whenever the medium changes, e.g. on insert or eject.
    /// The USB side watches this to report a media change (UNIT ATTENTION) to the host.
    fn media_generation(&self) -> u32 {
        0
    }

    /// Read sector `lba` into `buf` (one sector long).
    fn read(&mut self, lba: u32, buf: &mut [u8]) -> Result<(), DiskError>;

    /// Write `buf` (one sector long) to sector `lba`.
    fn write(&mut self, lba: u32, buf: &[u8]) -> Result<(), DiskError>;
}

/// Disk shared between the UI and USB. Borrowed on each call.
///
/// The UI borrows it only briefly on insert/eject and never awaits meanwhile, so borrows never overlap.
pub struct SharedDisk<'a, T: Disk>(pub &'a RefCell<T>);

impl<T: Disk> Disk for SharedDisk<'_, T> {
    fn geometry(&self) -> Option<Geometry> {
        self.0.try_borrow().ok().and_then(|d| d.geometry())
    }

    fn write_protected(&self) -> bool {
        self.0.try_borrow().map(|d| d.write_protected()).unwrap_or(true)
    }

    fn media_generation(&self) -> u32 {
        self.0.try_borrow().map(|d| d.media_generation()).unwrap_or(0)
    }

    fn read(&mut self, lba: u32, buf: &mut [u8]) -> Result<(), DiskError> {
        self.0.try_borrow_mut().map_err(|_| DiskError::Read)?.read(lba, buf)
    }

    fn write(&mut self, lba: u32, buf: &[u8]) -> Result<(), DiskError> {
        self.0.try_borrow_mut().map_err(|_| DiskError::Write)?.write(lba, buf)
    }
}
