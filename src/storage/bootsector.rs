//! Simple parsing of the FAT boot sector (BPB) (hardware independent).
//! Used as a hint for whether an image contains a FAT-formatted floppy.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootInfo {
    /// OEM name (offset 3, 8 bytes). Non-printable characters become '?'.
    pub oem: [u8; 8],
    pub bytes_per_sector: u16,
    pub sectors_per_track: u16,
    pub heads: u16,
    /// Total sector count (16-bit field; if 0, the 32-bit field is used).
    pub total_sectors: u32,
    /// Whether it ends with 0x55 0xAA.
    pub signature: bool,
}

/// Parse the first sector. `None` if shorter than 512 bytes.
pub fn parse(sector: &[u8]) -> Option<BootInfo> {
    if sector.len() < 512 {
        return None;
    }
    let u16_at = |o: usize| u16::from_le_bytes([sector[o], sector[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes([sector[o], sector[o + 1], sector[o + 2], sector[o + 3]]);

    let mut oem = [b'?'; 8];
    for (dst, &src) in oem.iter_mut().zip(&sector[3..11]) {
        if src.is_ascii_graphic() || src == b' ' {
            *dst = src;
        }
    }
    let total16 = u16_at(19);
    Some(BootInfo {
        oem,
        bytes_per_sector: u16_at(11),
        sectors_per_track: u16_at(24),
        heads: u16_at(26),
        total_sectors: if total16 != 0 { u32::from(total16) } else { u32_at(32) },
        signature: sector[510] == 0x55 && sector[511] == 0xAA,
    })
}

impl BootInfo {
    pub fn oem_str(&self) -> &str {
        core::str::from_utf8(&self.oem).unwrap_or("?").trim_end()
    }
}
