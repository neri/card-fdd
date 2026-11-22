//! Read the SD card's FAT directly to find a file's physical layout (hardware independent).
//!
//! When exposing an image file over USB, going through the FAT layer for every sector is slow, so
//! the cluster chain is walked once on open and turned into a list of contiguous ranges ([`Extent`]).
//! After that, SD physical blocks are computed from this list and read/written directly.
//!
//! Only FAT16 / FAT32 are supported (FAT12 is practically never used on SD cards).

use heapless::Vec;

/// SD block size.
pub const BLOCK: usize = 512;

/// Maximum number of contiguous ranges per file. Files more fragmented than this are not opened.
pub const MAX_EXTENTS: usize = 64;

/// Block-level reads.
pub trait BlockReader {
    fn read_block(&self, lba: u32, buf: &mut [u8; BLOCK]) -> Result<(), FatError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatError {
    /// SD read failed.
    Io,
    /// Cannot be interpreted as FAT16 / FAT32.
    UnsupportedFs,
    /// The cluster chain ends early, points out of range, or revisits a cluster.
    BrokenChain,
    /// Too fragmented (exceeds [`MAX_EXTENTS`]).
    TooFragmented,
}

/// Layout of a FAT volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FatLayout {
    /// First block of the (first) FAT.
    pub fat_start: u32,
    /// First block of the data area (cluster 2).
    pub data_start: u32,
    pub sectors_per_cluster: u32,
    pub cluster_count: u32,
    pub fat32: bool,
}

/// A range of contiguous SD physical blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub first_block: u32,
    pub blocks: u32,
}

/// Read the layout of the first MBR partition (the volume embedded-sdmmc opens as VolumeIdx(0)).
///
/// Like embedded-sdmmc 0.10, a card without a partition table (BPB in block 0) is not supported.
pub fn read_layout(dev: &impl BlockReader) -> Result<FatLayout, FatError> {
    let mut b = [0u8; BLOCK];
    dev.read_block(0, &mut b)?;
    if b[510] != 0x55 || b[511] != 0xAA {
        return Err(FatError::UnsupportedFs);
    }

    // Same partition types as embedded-sdmmc accepts (FAT16 / FAT32)
    let entry = &b[0x1BE..0x1CE];
    let part_start = match entry[4] {
        0x04 | 0x06 | 0x0B | 0x0C | 0x0E => le32(&entry[8..]),
        _ => return Err(FatError::UnsupportedFs),
    };
    if part_start == 0 {
        return Err(FatError::UnsupportedFs);
    }
    dev.read_block(part_start, &mut b)?;
    if !is_bpb(&b) {
        return Err(FatError::UnsupportedFs);
    }
    parse_bpb(part_start, &b)
}

fn is_bpb(b: &[u8; BLOCK]) -> bool {
    (b[0] == 0xEB || b[0] == 0xE9) && le16(&b[11..]) == BLOCK as u16 && b[13] != 0 && b[16] != 0
}

fn parse_bpb(part_start: u32, b: &[u8; BLOCK]) -> Result<FatLayout, FatError> {
    let spc = u32::from(b[13]);
    let reserved = u32::from(le16(&b[14..]));
    let num_fats = u32::from(b[16]);
    let root_entries = u32::from(le16(&b[17..]));
    let total = match le16(&b[19..]) {
        0 => le32(&b[32..]),
        n => u32::from(n),
    };
    let fat_size = match le16(&b[22..]) {
        0 => le32(&b[36..]),
        n => u32::from(n),
    };
    let root_dir_blocks = (root_entries * 32).div_ceil(BLOCK as u32);
    let meta = reserved + num_fats * fat_size + root_dir_blocks;
    if spc == 0 || fat_size == 0 || total <= meta {
        return Err(FatError::UnsupportedFs);
    }
    let cluster_count = (total - meta) / spc;
    if cluster_count < 4085 {
        return Err(FatError::UnsupportedFs); // FAT12
    }
    Ok(FatLayout {
        fat_start: part_start + reserved,
        data_start: part_start + meta,
        sectors_per_cluster: spc,
        cluster_count,
        fat32: cluster_count >= 65525,
    })
}

/// Extract the starting cluster from a directory entry (32 bytes).
pub fn first_cluster(entry: &[u8]) -> u32 {
    (u32::from(le16(&entry[20..])) << 16) | u32::from(le16(&entry[26..]))
}

/// Find the physical layout of a `size`-byte file starting at `first_cluster`.
pub fn file_extents(
    dev: &impl BlockReader,
    layout: &FatLayout,
    first_cluster: u32,
    size: u32,
) -> Result<Vec<Extent, MAX_EXTENTS>, FatError> {
    let cluster_bytes = layout.sectors_per_cluster * BLOCK as u32;
    let needed = size.div_ceil(cluster_bytes);
    let mut extents: Vec<Extent, MAX_EXTENTS> = Vec::new();
    if needed == 0 {
        return Ok(extents);
    }

    let mut fat_cache_block = u32::MAX;
    let mut fat = [0u8; BLOCK];
    let mut cluster = first_cluster;
    for i in 0..needed {
        if cluster < 2 || cluster >= layout.cluster_count + 2 {
            return Err(FatError::BrokenChain);
        }
        let block = layout.data_start + (cluster - 2) * layout.sectors_per_cluster;
        let end = block.checked_add(layout.sectors_per_cluster).ok_or(FatError::BrokenChain)?;
        if extents.iter().any(|e| block < e.first_block + e.blocks && e.first_block < end) {
            return Err(FatError::BrokenChain);
        }
        match extents.last_mut() {
            Some(e) if e.first_block + e.blocks == block => e.blocks += layout.sectors_per_cluster,
            _ => extents
                .push(Extent {
                    first_block: block,
                    blocks: layout.sectors_per_cluster,
                })
                .map_err(|_| FatError::TooFragmented)?,
        }
        if i + 1 == needed {
            break;
        }

        // Read the next cluster from the FAT
        let entry_size = if layout.fat32 { 4 } else { 2 };
        let offset = cluster * entry_size;
        let fat_block = layout.fat_start + offset / BLOCK as u32;
        if fat_block != fat_cache_block {
            dev.read_block(fat_block, &mut fat)?;
            fat_cache_block = fat_block;
        }
        let o = (offset % BLOCK as u32) as usize;
        cluster = if layout.fat32 {
            le32(&fat[o..]) & 0x0FFF_FFFF
        } else {
            u32::from(le16(&fat[o..]))
        };
        let end_of_chain = if layout.fat32 { 0x0FFF_FFF8 } else { 0xFFF8 };
        if cluster >= end_of_chain {
            return Err(FatError::BrokenChain);
        }
    }
    Ok(extents)
}

/// Convert a block number within the file (512-byte units) to an SD physical block.
pub fn map_block(extents: &[Extent], mut index: u32) -> Option<u32> {
    for e in extents {
        if index < e.blocks {
            return Some(e.first_block + index);
        }
        index -= e.blocks;
    }
    None
}

fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
