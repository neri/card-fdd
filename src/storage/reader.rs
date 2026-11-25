//! SD reader mode: present the whole SD card over USB as a removable disk.
//!
//! Reads and writes go straight to the card's blocks (LBA = card block, MBR included). Nothing on the
//! device side touches the file system while this runs, so the host owns the card completely.
//! On a host eject or a card error the medium disappears for good; leaving reader mode reboots.

use embedded_sdmmc::{Block, BlockDevice, BlockIdx};

use super::geometry::Geometry;
use super::{Capacity, Disk, DiskError};

/// Card block size [bytes].
const BLOCK_SIZE: u16 = 512;
/// Most blocks read with one multi-block read command (CMD18) or written with one multi-block
/// write command (CMD25). Matches the USB side's buffer.
pub const MAX_CHUNK_BLOCKS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderState {
    /// The host can read and write the card.
    Ready,
    /// The host ejected the card (START STOP UNIT). Safe to leave.
    Ejected,
    /// The card could not be read or written (or was not found at startup).
    Failed,
}

pub struct ReaderDisk<D: BlockDevice> {
    device: D,
    blocks: u32,
    state: ReaderState,
    /// Blocks for multi-block reads and writes (embedded-sdmmc works on `Block`s).
    scratch: [Block; MAX_CHUNK_BLOCKS],
}

impl<D: BlockDevice> ReaderDisk<D> {
    /// `blocks` is the card size in 512-byte blocks; `None` means the card could not be read.
    pub fn new(device: D, blocks: Option<u32>) -> Self {
        Self {
            device,
            blocks: blocks.unwrap_or(0),
            state: if blocks.is_some() { ReaderState::Ready } else { ReaderState::Failed },
            scratch: core::array::from_fn(|_| Block::new()),
        }
    }

    pub fn state(&self) -> ReaderState {
        self.state
    }

    /// Card size [bytes].
    pub fn total_bytes(&self) -> u64 {
        u64::from(self.blocks) * u64::from(BLOCK_SIZE)
    }

    /// The host ejected the card. Call only outside a USB transfer.
    pub fn eject(&mut self) {
        if self.state == ReaderState::Ready {
            self.state = ReaderState::Ejected;
        }
    }

    fn check_ready(&self) -> Result<(), DiskError> {
        match self.state {
            ReaderState::Ready => Ok(()),
            _ => Err(DiskError::NoMedium),
        }
    }
}

impl<D: BlockDevice> Disk for ReaderDisk<D> {
    fn geometry(&self) -> Option<Geometry> {
        None
    }

    fn capacity(&self) -> Option<Capacity> {
        (self.state == ReaderState::Ready).then_some(Capacity {
            blocks: self.blocks,
            block_size: BLOCK_SIZE,
        })
    }

    fn write_protected(&self) -> bool {
        false
    }

    fn read(&mut self, lba: u32, buf: &mut [u8]) -> Result<(), DiskError> {
        self.check_ready()?;
        let mut block = [Block::new()];
        if let Err(e) = self.device.read(&mut block, BlockIdx(lba)) {
            log::warn!("SD read error (block {}): {:?}", lba, e);
            self.state = ReaderState::Failed;
            return Err(DiskError::Read);
        }
        buf.copy_from_slice(&block[0].contents);
        Ok(())
    }

    fn max_read_blocks(&self) -> u32 {
        MAX_CHUNK_BLOCKS as u32
    }

    /// Reads up to [`MAX_CHUNK_BLOCKS`] at a time with one multi-block read command (CMD18), which
    /// saves the per-block command and wait of single-block reads.
    fn read_blocks(&mut self, lba: u32, buf: &mut [u8], _block_size: usize) -> Result<(), DiskError> {
        self.check_ready()?;
        let size = usize::from(BLOCK_SIZE);
        let mut lba = lba;
        for chunk in buf.chunks_mut(size * MAX_CHUNK_BLOCKS) {
            let n = chunk.len() / size;
            let blocks = &mut self.scratch[..n];
            if let Err(e) = self.device.read(blocks, BlockIdx(lba)) {
                log::warn!("SD read error (blocks {}+{}): {:?}", lba, n, e);
                self.state = ReaderState::Failed;
                return Err(DiskError::Read);
            }
            for (dst, block) in chunk.chunks_exact_mut(size).zip(blocks.iter()) {
                dst.copy_from_slice(&block.contents);
            }
            lba += n as u32;
        }
        Ok(())
    }

    fn write(&mut self, lba: u32, buf: &[u8]) -> Result<(), DiskError> {
        self.check_ready()?;
        let mut block = [Block::new()];
        block[0].contents.copy_from_slice(buf);
        if let Err(e) = self.device.write(&block, BlockIdx(lba)) {
            log::warn!("SD write error (block {}): {:?}", lba, e);
            self.state = ReaderState::Failed;
            return Err(DiskError::Write);
        }
        Ok(())
    }

    fn max_write_blocks(&self) -> u32 {
        MAX_CHUNK_BLOCKS as u32
    }

    /// Writes up to [`MAX_CHUNK_BLOCKS`] at a time with one multi-block write command (CMD25, after
    /// ACMD23 pre-erase), so the card programs them together instead of once per block.
    fn write_blocks(&mut self, lba: u32, buf: &[u8], _block_size: usize) -> Result<(), DiskError> {
        self.check_ready()?;
        let size = usize::from(BLOCK_SIZE);
        let mut lba = lba;
        for chunk in buf.chunks(size * MAX_CHUNK_BLOCKS) {
            let n = chunk.len() / size;
            let blocks = &mut self.scratch[..n];
            for (block, src) in blocks.iter_mut().zip(chunk.chunks_exact(size)) {
                block.contents.copy_from_slice(src);
            }
            if let Err(e) = self.device.write(blocks, BlockIdx(lba)) {
                log::warn!("SD write error (blocks {}+{}): {:?}", lba, n, e);
                self.state = ReaderState::Failed;
                return Err(DiskError::Write);
            }
            lba += n as u32;
        }
        Ok(())
    }
}
