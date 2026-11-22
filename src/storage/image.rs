//! Present an image file on the SD card as a USB "floppy disk".
//!
//! On insert, the image's physical layout (a list of [`fat::Extent`]) is computed, and reads/writes go directly
//! to SD physical blocks (write-through). The file size never changes, so the FAT is not rewritten.

use heapless::{String, Vec};

use super::browser::{Entry, EntryKind, NAME_LEN};
use super::fat::{self, BLOCK, BlockReader, Extent, FatError, MAX_EXTENTS};
use super::geometry::Geometry;
use super::{Disk, DiskError};

/// Raw SD block reads/writes.
pub trait RawBlocks {
    fn read_block(&self, lba: u32, buf: &mut [u8; BLOCK]) -> Result<(), DiskError>;
    fn write_block(&self, lba: u32, buf: &[u8; BLOCK]) -> Result<(), DiskError>;
}

/// Position of an entry in a directory. Used to identify the inserted image in the listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryId {
    pub dir_block: u32,
    pub dir_offset: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountError {
    /// File that cannot be inserted (folder, unsupported size, etc.).
    NotMountable,
    /// SD read failed.
    Io,
    /// Unexpected file system or cluster chain.
    Fat(FatError),
}

/// The inserted image.
pub struct MountedImage {
    pub name: String<NAME_LEN>,
    pub id: EntryId,
    pub geometry: Geometry,
    /// The file has the read-only attribute.
    pub read_only: bool,
    extents: Vec<Extent, MAX_EXTENTS>,
}

impl MountedImage {
    /// Whether it is physically contiguous (not fragmented).
    pub fn is_contiguous(&self) -> bool {
        self.extents.len() <= 1
    }

    pub fn extent_count(&self) -> usize {
        self.extents.len()
    }
}

pub struct ImageDisk<R: RawBlocks + 'static> {
    sd: &'static R,
    mounted: Option<MountedImage>,
    /// Incremented on every insert/eject. The USB side uses this to detect media changes.
    generation: u32,
    /// Write-protect switch (a drive-side setting; kept across reinsertion).
    wp_switch: bool,
    /// An SD error occurred during read/write (the UI ejects and shows the error).
    io_error: bool,
}

impl<R: RawBlocks + 'static> ImageDisk<R> {
    pub const fn new(sd: &'static R) -> Self {
        Self {
            sd,
            mounted: None,
            generation: 0,
            wp_switch: false,
            io_error: false,
        }
    }

    /// Return true if an SD error occurred, and clear the record.
    pub fn take_io_error(&mut self) -> bool {
        core::mem::take(&mut self.io_error)
    }

    /// Read the first block of a file that is not inserted (for the info display).
    ///
    /// Bypasses embedded-sdmmc's block cache, so the latest contents can be read even right after
    /// being rewritten over USB.
    pub fn read_file_head(&self, id: EntryId, buf: &mut [u8; BLOCK]) -> Result<(), MountError> {
        let reader = Reader(self.sd);
        let mut dir = [0u8; BLOCK];
        reader.read_block(id.dir_block, &mut dir).map_err(|_| MountError::Io)?;
        let o = id.dir_offset as usize;
        let raw = dir.get(o..o + 32).ok_or(MountError::Fat(FatError::BrokenChain))?;
        let cluster = fat::first_cluster(raw);
        let layout = fat::read_layout(&reader).map_err(MountError::Fat)?;
        if cluster < 2 || cluster >= layout.cluster_count + 2 {
            return Err(MountError::Fat(FatError::BrokenChain));
        }
        let block = layout.data_start + (cluster - 2) * layout.sectors_per_cluster;
        reader.read_block(block, buf).map_err(|_| MountError::Io)
    }

    pub fn mounted(&self) -> Option<&MountedImage> {
        self.mounted.as_ref()
    }

    /// Whether it is actually write-protected (switch, or the file's read-only attribute).
    pub fn is_write_protected(&self) -> bool {
        self.wp_switch || self.mounted.as_ref().is_some_and(|m| m.read_only)
    }

    pub fn wp_switch(&self) -> bool {
        self.wp_switch
    }

    /// Toggle the write-protect switch.
    pub fn set_wp_switch(&mut self, wp: bool) {
        if self.wp_switch != wp {
            self.wp_switch = wp;
            if self.mounted.is_some() {
                // If an image is inserted, report a media change so the host re-reads the state
                self.generation = self.generation.wrapping_add(1);
            }
        }
    }

    /// Insert an entry from the listing. Replaces any currently inserted image.
    pub fn mount(&mut self, entry: &Entry, id: EntryId) -> Result<(), MountError> {
        let EntryKind::Image(Some(format)) = entry.kind else {
            return Err(MountError::NotMountable);
        };
        if !format.supported {
            return Err(MountError::NotMountable);
        }

        // Read the directory entry directly to get the starting cluster
        let reader = Reader(self.sd);
        let mut dir = [0u8; BLOCK];
        reader.read_block(id.dir_block, &mut dir).map_err(|_| MountError::Io)?;
        let o = id.dir_offset as usize;
        let raw = dir.get(o..o + 32).ok_or(MountError::Fat(FatError::BrokenChain))?;
        let cluster = fat::first_cluster(raw);

        let layout = fat::read_layout(&reader).map_err(MountError::Fat)?;
        let extents = fat::file_extents(&reader, &layout, cluster, entry.size).map_err(MountError::Fat)?;

        self.mounted = Some(MountedImage {
            name: entry.name.clone(),
            id,
            geometry: format.geometry,
            read_only: entry.read_only,
            extents,
        });
        self.generation = self.generation.wrapping_add(1);
        Ok(())
    }

    /// Eject.
    pub fn eject(&mut self) {
        if self.mounted.take().is_some() {
            self.generation = self.generation.wrapping_add(1);
        }
    }

    /// Physical location of the `k`-th block (512-byte units) of image sector `lba`.
    fn physical(&self, lba: u32, k: u32) -> Result<(u32, &MountedImage), DiskError> {
        let m = self.mounted.as_ref().ok_or(DiskError::NoMedium)?;
        let per_sector = u32::from(m.geometry.bytes_per_sector) / BLOCK as u32;
        let block = fat::map_block(&m.extents, lba * per_sector + k).ok_or(DiskError::Read)?;
        Ok((block, m))
    }
}

impl<R: RawBlocks + 'static> Disk for ImageDisk<R> {
    fn geometry(&self) -> Option<Geometry> {
        self.mounted.as_ref().map(|m| m.geometry)
    }

    fn write_protected(&self) -> bool {
        self.is_write_protected()
    }

    fn media_generation(&self) -> u32 {
        self.generation
    }

    fn read(&mut self, lba: u32, buf: &mut [u8]) -> Result<(), DiskError> {
        let (_, m) = self.physical(lba, 0)?;
        let blocks = u32::from(m.geometry.bytes_per_sector) / BLOCK as u32;
        for k in 0..blocks {
            let (phys, _) = self.physical(lba, k)?;
            let chunk: &mut [u8; BLOCK] = (&mut buf[k as usize * BLOCK..(k as usize + 1) * BLOCK])
                .try_into()
                .map_err(|_| DiskError::Read)?;
            if let Err(e) = self.sd.read_block(phys, chunk) {
                self.io_error = true;
                return Err(e);
            }
        }
        Ok(())
    }

    fn write(&mut self, lba: u32, buf: &[u8]) -> Result<(), DiskError> {
        if self.is_write_protected() {
            return Err(DiskError::Write);
        }
        let (_, m) = self.physical(lba, 0)?;
        let blocks = u32::from(m.geometry.bytes_per_sector) / BLOCK as u32;
        for k in 0..blocks {
            let (phys, _) = self.physical(lba, k)?;
            let chunk: &[u8; BLOCK] = (&buf[k as usize * BLOCK..(k as usize + 1) * BLOCK])
                .try_into()
                .map_err(|_| DiskError::Write)?;
            if let Err(e) = self.sd.write_block(phys, chunk) {
                self.io_error = true;
                return Err(e);
            }
        }
        Ok(())
    }
}

/// Use [`RawBlocks`] as a [`BlockReader`] for FAT parsing.
struct Reader<'a, R: RawBlocks>(&'a R);

impl<R: RawBlocks> BlockReader for Reader<'_, R> {
    fn read_block(&self, lba: u32, buf: &mut [u8; BLOCK]) -> Result<(), FatError> {
        self.0.read_block(lba, buf).map_err(|_| FatError::Io)
    }
}
