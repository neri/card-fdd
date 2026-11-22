//! Floppy geometry (hardware independent).
//!
//! Raw images have no header, so the format is determined from the file size.
//! Sector size is treated as variable (PC-98 1.25MB uses 1024 bytes); 512 is not assumed.

/// Cylinder / head / sector counts and sector size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub cylinders: u16,
    pub heads: u8,
    pub sectors_per_track: u8,
    pub bytes_per_sector: u16,
}

impl Geometry {
    pub const fn total_sectors(&self) -> u32 {
        self.cylinders as u32 * self.heads as u32 * self.sectors_per_track as u32
    }

    pub const fn total_bytes(&self) -> u32 {
        self.total_sectors() * self.bytes_per_sector as u32
    }

    /// Convert an LBA to (cylinder, head, sector). Sectors are 1-based.
    pub const fn chs(&self, lba: u32) -> (u16, u8, u8) {
        let spt = self.sectors_per_track as u32;
        let heads = self.heads as u32;
        let cylinder = lba / (spt * heads);
        let head = (lba / spt) % heads;
        let sector = lba % spt + 1;
        (cylinder as u16, head as u8, sector as u8)
    }
}

/// Floppy format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    /// Display name of the capacity ("1.44M" etc.).
    pub label: &'static str,
    /// Display name of the type ("2HD" etc.).
    pub kind: &'static str,
    pub geometry: Geometry,
    /// Whether the current firmware supports it.
    pub supported: bool,
}

const fn fmt(
    label: &'static str,
    kind: &'static str,
    cylinders: u16,
    heads: u8,
    sectors_per_track: u8,
    bytes_per_sector: u16,
    supported: bool,
) -> Format {
    Format {
        label,
        kind,
        geometry: Geometry {
            cylinders,
            heads,
            sectors_per_track,
            bytes_per_sector,
        },
        supported,
    }
}

/// List of recognized formats. Initially only 1.44MB is supported (plan section 3.4).
pub const FORMATS: [Format; 6] = [
    fmt("1.44M", "2HD", 80, 2, 18, 512, true),
    fmt("360K", "2D", 40, 2, 9, 512, false),
    fmt("720K", "2DD", 80, 2, 9, 512, false),
    fmt("1.2M", "2HC", 80, 2, 15, 512, false),
    fmt("1.25M", "PC98", 77, 2, 8, 1024, false),
    fmt("2.88M", "ED", 80, 2, 36, 512, false),
];

/// Determine the format from the file size. `None` if the size is not in the list.
pub fn detect(size: u32) -> Option<&'static Format> {
    FORMATS.iter().find(|f| f.geometry.total_bytes() == size)
}
