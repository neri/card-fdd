//! Storage layer: files on the SD card and floppy images.

pub mod bootsector;
pub mod browser;
pub mod fat;
pub mod geometry;
pub mod image;
pub mod ramdisk;
pub mod reader;

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

/// Size of the medium in blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    pub blocks: u32,
    pub block_size: u16,
}

/// The medium seen over USB (a floppy image, or the whole SD card). Read and written in sectors.
pub trait Disk {
    /// Floppy geometry of the medium. `None` if no medium is present or the medium has no CHS layout.
    fn geometry(&self) -> Option<Geometry>;

    /// Size of the medium. `None` if no medium is present.
    ///
    /// Defaults to the size given by the geometry. Disks without a CHS geometry override this.
    fn capacity(&self) -> Option<Capacity> {
        self.geometry().map(|g| Capacity {
            blocks: g.total_sectors(),
            block_size: g.bytes_per_sector,
        })
    }

    /// Whether it is write-protected.
    fn write_protected(&self) -> bool;

    /// Medium generation number. Changes whenever the medium changes, e.g. on insert or eject.
    /// The USB side watches this to report a media change (UNIT ATTENTION) to the host.
    fn media_generation(&self) -> u32 {
        0
    }

    /// Read sector `lba` into `buf` (one sector long).
    fn read(&mut self, lba: u32, buf: &mut [u8]) -> Result<(), DiskError>;

    /// Most sectors [`Disk::read_blocks`] should be given at once. 1 (the default) means reading
    /// sector by sector is just as fast, so the USB side reads one sector at a time.
    fn max_read_blocks(&self) -> u32 {
        1
    }

    /// Read consecutive sectors from `lba` into `buf` (a whole number of sectors long).
    /// On error, any part of `buf` may have been overwritten.
    fn read_blocks(&mut self, lba: u32, buf: &mut [u8], block_size: usize) -> Result<(), DiskError> {
        for (i, chunk) in buf.chunks_exact_mut(block_size).enumerate() {
            self.read(lba + i as u32, chunk)?;
        }
        Ok(())
    }

    /// Write `buf` (one sector long) to sector `lba`.
    fn write(&mut self, lba: u32, buf: &[u8]) -> Result<(), DiskError>;

    /// Most sectors [`Disk::write_blocks`] should be given at once (see [`Disk::max_read_blocks`]).
    fn max_write_blocks(&self) -> u32 {
        1
    }

    /// Write consecutive sectors from `buf` (a whole number of sectors long) starting at `lba`.
    /// On error, any of the sectors may or may not have been written.
    fn write_blocks(&mut self, lba: u32, buf: &[u8], block_size: usize) -> Result<(), DiskError> {
        for (i, chunk) in buf.chunks_exact(block_size).enumerate() {
            self.write(lba + i as u32, chunk)?;
        }
        Ok(())
    }
}

/// Disk shared between the UI and USB. Borrowed on each call.
///
/// The UI borrows it only briefly on insert/eject and never awaits meanwhile, so borrows never overlap.
pub struct SharedDisk<'a, T: Disk>(pub &'a RefCell<T>);

impl<T: Disk> Disk for SharedDisk<'_, T> {
    fn geometry(&self) -> Option<Geometry> {
        self.0.try_borrow().ok().and_then(|d| d.geometry())
    }

    fn capacity(&self) -> Option<Capacity> {
        self.0.try_borrow().ok().and_then(|d| d.capacity())
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

    fn max_read_blocks(&self) -> u32 {
        self.0.try_borrow().map(|d| d.max_read_blocks()).unwrap_or(1)
    }

    fn read_blocks(&mut self, lba: u32, buf: &mut [u8], block_size: usize) -> Result<(), DiskError> {
        self.0
            .try_borrow_mut()
            .map_err(|_| DiskError::Read)?
            .read_blocks(lba, buf, block_size)
    }

    fn write(&mut self, lba: u32, buf: &[u8]) -> Result<(), DiskError> {
        self.0.try_borrow_mut().map_err(|_| DiskError::Write)?.write(lba, buf)
    }

    fn max_write_blocks(&self) -> u32 {
        self.0.try_borrow().map(|d| d.max_write_blocks()).unwrap_or(1)
    }

    fn write_blocks(&mut self, lba: u32, buf: &[u8], block_size: usize) -> Result<(), DiskError> {
        self.0
            .try_borrow_mut()
            .map_err(|_| DiskError::Write)?
            .write_blocks(lba, buf, block_size)
    }
}
