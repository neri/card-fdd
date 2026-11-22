//! Walk folders on the SD card and build a list of image files.

use core::fmt::Write;
use core::ops::ControlFlow;

use core::cell::RefCell;

use embedded_sdmmc::{
    Block, BlockDevice, BlockIdx, DirEntry, LfnBuffer, Mode, RawDirectory, RawFile, RawVolume,
    ShortFileName, TimeSource, Timestamp, VolumeIdx, VolumeManager,
};
use heapless::{String, Vec};

use crate::config::{self, IMAGE_PATH_PREFIX, ImagePath, MAX_LFN_BYTES};

use super::DiskError;
use super::fat::BLOCK;
use super::geometry::{self, Format};
use super::image::{EntryId, RawBlocks};

/// Maximum number of entries handled per folder. Extra entries are not shown.
pub const MAX_ENTRIES: usize = 128;
/// Maximum display name length in bytes.
pub const NAME_LEN: usize = 64;
/// Maximum folder depth that can be navigated.
pub const MAX_DEPTH: usize = 8;

/// Entry kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// The parent folder ("..").
    Parent,
    Dir,
    /// Image file. `None` if the size is not in the format list.
    Image(Option<&'static Format>),
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub kind: EntryKind,
    /// Display name (LFN; non-ASCII characters become '?').
    pub name: String<NAME_LEN>,
    /// 8.3 name (used to open files and folders).
    pub short: ShortFileName,
    pub size: u32,
    /// Read-only attribute. The image is write-protected when inserted.
    pub read_only: bool,
    /// Position of the entry in the directory (0 for "..").
    pub id: EntryId,
}

impl Entry {
    /// Whether it is an insertable image.
    pub fn is_mountable(&self) -> bool {
        matches!(self.kind, EntryKind::Image(Some(f)) if f.supported)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageError {
    /// No card, or the card does not respond.
    NoCard,
    /// Cannot be read as FAT, or a file or folder was not found.
    Fs,
}

impl<E: core::error::Error> From<embedded_sdmmc::Error<E>> for StorageError {
    fn from(e: embedded_sdmmc::Error<E>) -> Self {
        log::warn!("SD error: {:?}", e);
        match e {
            embedded_sdmmc::Error::DeviceError(_) => StorageError::NoCard,
            _ => StorageError::Fs,
        }
    }
}

/// There is no RTC, so file timestamps are fixed (2026-01-01 00:00).
pub struct FixedTime;

impl TimeSource for FixedTime {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp {
            year_since_1970: 56,
            zero_indexed_month: 0,
            zero_indexed_day: 0,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}

/// A folder opened along the way (one level of the path).
struct PathItem {
    short: ShortFileName,
    name: String<NAME_LEN>,
}

/// An image selected in a particular folder, even if the UI later navigates elsewhere.
pub struct ImageSelection {
    pub entry: Entry,
    folders: Vec<ShortFileName, MAX_DEPTH>,
}

pub struct Browser<D: BlockDevice> {
    vm: VolumeManager<D, FixedTime>,
    volume: Option<RawVolume>,
    path: Vec<PathItem, MAX_DEPTH>,
    entries: Vec<Entry, MAX_ENTRIES>,
    /// Whether some entries were not shown because the limit was exceeded.
    truncated: bool,
}

impl<D: BlockDevice> Browser<D>
where
    D::Error: core::error::Error,
{
    pub fn new(device: D) -> Self {
        Self {
            vm: VolumeManager::new(device, FixedTime),
            volume: None,
            path: Vec::new(),
            entries: Vec::new(),
            truncated: false,
        }
    }

    /// Access the block device (SD card).
    pub fn device<R>(&self, f: impl FnOnce(&mut D) -> R) -> R {
        self.vm.device(f)
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn is_root(&self) -> bool {
        self.path.is_empty()
    }

    /// Current path ("/FLOPPY/GAMES" etc.).
    pub fn path_string(&self) -> String<128> {
        let mut s = String::new();
        if self.path.is_empty() {
            let _ = s.push('/');
        }
        for item in &self.path {
            let _ = write!(s, "/{}", item.name);
        }
        s
    }

    pub fn select_image(&self, entry: Entry) -> ImageSelection {
        ImageSelection { entry, folders: self.folders(self.path.len()) }
    }

    /// Read the original names of the selected file and its parent folders for persistence.
    pub fn image_path(&mut self, selection: &ImageSelection, path: &mut ImagePath) -> Result<(), StorageError> {
        path.clear();
        path.push_str(IMAGE_PATH_PREFIX).map_err(|_| StorageError::Fs)?;
        for (depth, short) in selection.folders.iter().enumerate() {
            self.append_path_name(&selection.folders[..depth], path, |de| {
                de.attributes.is_directory() && de.name == *short
            })?;
        }
        self.append_path_name(&selection.folders, path, |de| {
            !de.attributes.is_directory()
                && de.name == selection.entry.short
                && de.entry_block.0 == selection.entry.id.dir_block
                && de.entry_offset == selection.entry.id.dir_offset
        })
    }

    fn append_path_name(
        &mut self,
        folders: &[ShortFileName],
        path: &mut ImagePath,
        matches: impl Fn(&DirEntry) -> bool,
    ) -> Result<(), StorageError> {
        let found = self.scan_names(folders, |de, name, _| {
            matches(de).then(|| config::append_image_name(path, name))
        })?;
        found.ok_or(StorageError::Fs)?.map_err(|_| StorageError::Fs)
    }

    /// Resolve every component of a saved image path, using the current entry for the image.
    pub fn find_image(&mut self, path: &str) -> Result<Option<ImageSelection>, StorageError> {
        let (encoded, path) = match path.strip_prefix(IMAGE_PATH_PREFIX) {
            Some(path) => (true, path),
            None => (false, path),
        };
        let mut components = path.strip_prefix('/').unwrap_or(path).split('/').peekable();
        let mut folders = Vec::<ShortFileName, MAX_DEPTH>::new();
        let mut ui_path = Vec::<PathItem, MAX_DEPTH>::new();
        while let Some(part) = components.next() {
            let decoded;
            let name = if encoded {
                let Some(value) = config::decode_image_name(part) else { return Ok(None) };
                decoded = value;
                decoded.as_str()
            } else {
                part
            };
            if name.is_empty() {
                return Ok(None);
            }
            let folder = components.peek().is_some();
            let entry = self.scan_names(&folders, |de, original, long| {
                if de.attributes.is_directory() != folder {
                    return None;
                }
                if original.eq_ignore_ascii_case(name) {
                    return to_entry(de, long);
                }
                if encoded {
                    return None;
                }
                // Legacy saves used the displayed name and accepted short names for directories.
                to_entry(de, long).filter(|entry| {
                    entry.name.eq_ignore_ascii_case(name)
                        || (folder && short_name_matches(&entry.short, name))
                })
            })?;
            let Some(entry) = entry else { return Ok(None) };
            if !folder {
                if !entry.is_mountable() {
                    return Ok(None);
                }
                self.path = ui_path;
                self.refresh()?;
                return Ok(Some(ImageSelection { entry, folders }));
            }
            if folders.push(entry.short).is_err() {
                return Ok(None);
            }
            let _ = ui_path.push(PathItem { short: entry.short, name: entry.name });
        }
        Ok(None)
    }

    /// Find a unique match throughout the directory, including entries beyond the UI list limit.
    fn scan_names<T>(
        &mut self,
        folders: &[ShortFileName],
        mut select: impl FnMut(&DirEntry, &str, Option<&str>) -> Option<T>,
    ) -> Result<Option<T>, StorageError> {
        let mut storage = [0; MAX_LFN_BYTES];
        let mut lfn = LfnBuffer::new(&mut storage);
        let mut found = None;
        self.with_folders(folders, |vm, dir| {
            vm.iterate_dir_lfn(dir, &mut lfn, |de, long| {
                let mut short = String::<24>::new();
                let name = long.unwrap_or_else(|| {
                    let _ = write!(short, "{}", de.name);
                    short.as_str()
                });
                if let Some(value) = select(de, name, long)
                    && found.replace(value).is_some()
                {
                    found = None;
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(())
            })
        })?;
        Ok(found)
    }

    /// Reopen the card (e.g. after reinsertion). The path returns to the root.
    pub fn reset(&mut self) {
        if let Some(v) = self.volume.take() {
            let _ = self.vm.close_volume(v);
        }
        self.path.clear();
        self.entries.clear();
    }

    /// Move to a path like "/FLOPPY/GAMES". Stops where a component is not found.
    /// Names may match by LFN or 8.3 and are compared case-insensitively.
    pub fn go_to(&mut self, path: &str) -> Result<(), StorageError> {
        self.path.clear();
        self.refresh()?;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            let found = self.entries.iter().position(|e| {
                e.kind == EntryKind::Dir
                    && (e.name.eq_ignore_ascii_case(part)
                        || short_name_matches(&e.short, part))
            });
            match found {
                Some(i) => self.enter(i)?,
                None => {
                    log::info!("Folder {} not found", part);
                    break;
                }
            }
        }
        Ok(())
    }

    /// Enter the `index`-th entry of the listing (into a folder, or up for "..").
    pub fn enter(&mut self, index: usize) -> Result<(), StorageError> {
        let Some(entry) = self.entries.get(index) else {
            return Ok(());
        };
        match entry.kind {
            EntryKind::Parent => self.leave(),
            EntryKind::Dir => {
                let item = PathItem {
                    short: entry.short,
                    name: entry.name.clone(),
                };
                if self.path.push(item).is_err() {
                    log::warn!("Folder nesting too deep");
                    return Ok(());
                }
                let result = self.refresh();
                if result.is_err() {
                    // Remove the folder that could not be entered from the path
                    self.path.pop();
                }
                result
            }
            EntryKind::Image(_) => Ok(()),
        }
    }

    /// Go back to the parent folder.
    pub fn leave(&mut self) -> Result<(), StorageError> {
        if self.path.pop().is_some() {
            self.refresh()?;
        }
        Ok(())
    }

    /// Reload the current folder.
    pub fn refresh(&mut self) -> Result<(), StorageError> {
        let mut entries: Vec<Entry, MAX_ENTRIES> = Vec::new();
        let mut truncated = false;
        if !self.path.is_empty() {
            let _ = entries.push(Entry {
                kind: EntryKind::Parent,
                name: String::try_from("..").unwrap_or_default(),
                short: ShortFileName::parent_dir(),
                size: 0,
                read_only: false,
                id: EntryId {
                    dir_block: 0,
                    dir_offset: 0,
                },
            });
        }

        let mut lfn_storage = [0u8; MAX_LFN_BYTES];
        let mut lfn = LfnBuffer::new(&mut lfn_storage);
        self.with_current_dir(|vm, dir| {
            vm.iterate_dir_lfn(dir, &mut lfn, |de, long| {
                if let Some(entry) = to_entry(de, long)
                    && entries.push(entry).is_err()
                {
                    truncated = true;
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(())
            })
        })?;

        entries.sort_unstable_by(|a, b| sort_key(a).cmp(&sort_key(b)).then_with(|| cmp_name(a, b)));
        self.entries = entries;
        self.truncated = truncated;
        Ok(())
    }

    /// Read the file `name` (8.3 format) at the SD root into `buf`. Returns the number of bytes read.
    pub fn read_root_file(&mut self, name: &str, buf: &mut [u8]) -> Result<usize, StorageError> {
        self.with_dir(0, |vm, dir| {
            let file = vm.open_file_in_dir(dir, name, Mode::ReadOnly)?;
            let mut total = 0;
            let result = loop {
                match vm.read(file, &mut buf[total..]) {
                    Ok(0) => break Ok(total),
                    Ok(n) => {
                        total += n;
                        if total == buf.len() {
                            break Ok(total);
                        }
                    }
                    Err(e) => break Err(e),
                }
            };
            let _ = vm.close_file(file);
            result
        })
    }

    /// Recreate the file `name` (8.3 format) at the SD root and write `data`.
    pub fn write_root_file(&mut self, name: &str, data: &[u8]) -> Result<(), StorageError> {
        self.with_dir(0, |vm, dir| {
            let file = vm.open_file_in_dir(dir, name, Mode::ReadWriteCreateOrTruncate)?;
            let result = vm.write(file, data);
            let closed = vm.close_file(file);
            result.and(closed)
        })
    }

    /// Create a new file `name` (8.3 format) in the current folder and open it for writing.
    /// Write to it incrementally with [`Self::write_open_file`] and close it with [`Self::close_created_file`].
    pub fn create_file(&mut self, name: &str) -> Result<RawFile, StorageError> {
        let depth = self.path.len();
        self.with_dir(depth, |vm, dir| vm.open_file_in_dir(dir, name, Mode::ReadWriteCreate))
    }

    /// Append to the file opened with [`Self::create_file`].
    pub fn write_open_file(&mut self, file: RawFile, data: &[u8]) -> Result<(), StorageError> {
        Ok(self.vm.write(file, data)?)
    }

    /// Close the file opened with [`Self::create_file`] and reload the listing.
    pub fn close_created_file(&mut self, file: RawFile) -> Result<(), StorageError> {
        self.vm.close_file(file)?;
        self.refresh()
    }

    /// Open the current folder, call `f`, then close what was opened.
    fn with_current_dir<R>(
        &mut self,
        f: impl FnOnce(
            &VolumeManager<D, FixedTime>,
            RawDirectory,
        ) -> Result<R, embedded_sdmmc::Error<D::Error>>,
    ) -> Result<R, StorageError> {
        let depth = self.path.len();
        self.with_dir(depth, f)
    }

    /// Open the folder at level `depth` of the current path (0 = root), call `f`, then close what was opened.
    fn with_dir<R>(
        &mut self,
        depth: usize,
        f: impl FnOnce(
            &VolumeManager<D, FixedTime>,
            RawDirectory,
        ) -> Result<R, embedded_sdmmc::Error<D::Error>>,
    ) -> Result<R, StorageError> {
        let folders = self.folders(depth);
        self.with_folders(&folders, f)
    }

    fn folders(&self, depth: usize) -> Vec<ShortFileName, MAX_DEPTH> {
        self.path.iter().take(depth).map(|item| item.short).collect()
    }

    fn with_folders<R>(
        &mut self,
        folders: &[ShortFileName],
        f: impl FnOnce(
            &VolumeManager<D, FixedTime>,
            RawDirectory,
        ) -> Result<R, embedded_sdmmc::Error<D::Error>>,
    ) -> Result<R, StorageError> {
        let volume = match self.volume {
            Some(v) => v,
            None => {
                let v = self.vm.open_raw_volume(VolumeIdx(0))?;
                self.volume = Some(v);
                v
            }
        };

        let result = (|| {
            let mut dir = self.vm.open_root_dir(volume)?;
            for short in folders {
                let child = self.vm.open_dir(dir, short);
                let _ = self.vm.close_dir(dir);
                dir = child?;
            }
            let r = f(&self.vm, dir);
            let _ = self.vm.close_dir(dir);
            r
        })();

        if let Err(embedded_sdmmc::Error::DeviceError(_)) = &result {
            // The card may have been removed. Reopen from the volume next time.
            if let Some(v) = self.volume.take() {
                let _ = self.vm.close_volume(v);
            }
        }
        Ok(result?)
    }
}

/// Convert a directory entry to a listing entry. `None` for entries that are not shown.
fn to_entry(de: &DirEntry, long: Option<&str>) -> Option<Entry> {
    let attr = de.attributes;
    if attr.is_volume() || attr.is_hidden() || attr.is_system() {
        return None;
    }
    let base = de.name.base_name();
    if base == b"." || base == b".." {
        return None;
    }
    // Hide dot files (such as macOS "._xxx")
    if long.is_some_and(|n| n.starts_with('.')) {
        return None;
    }

    let kind = if attr.is_directory() {
        EntryKind::Dir
    } else {
        let ext = de.name.extension();
        if !(ext.eq_ignore_ascii_case(b"IMG") || ext.eq_ignore_ascii_case(b"IMA")) {
            return None;
        }
        EntryKind::Image(geometry::detect(de.size))
    };

    let mut name: String<NAME_LEN> = String::new();
    match long {
        Some(n) => {
            for c in n.chars() {
                if name.push(if c.is_ascii() && !c.is_ascii_control() { c } else { '?' }).is_err() {
                    break;
                }
            }
        }
        None => {
            let _ = write!(name, "{}", de.name);
        }
    }

    Some(Entry {
        kind,
        name,
        short: de.name,
        size: de.size,
        read_only: attr.is_read_only(),
        id: EntryId {
            dir_block: de.entry_block.0,
            dir_offset: de.entry_offset,
        },
    })
}

/// Sort order: ".." -> folders -> files.
fn sort_key(e: &Entry) -> u8 {
    match e.kind {
        EntryKind::Parent => 0,
        EntryKind::Dir => 1,
        EntryKind::Image(_) => 2,
    }
}

/// Name comparison (case-insensitive).
fn cmp_name(a: &Entry, b: &Entry) -> core::cmp::Ordering {
    let a = a.name.bytes().map(|c| c.to_ascii_lowercase());
    let b = b.name.bytes().map(|c| c.to_ascii_lowercase());
    a.cmp(b)
}

fn short_name_matches(short: &ShortFileName, name: &str) -> bool {
    let mut s: String<16> = String::new();
    let _ = write!(s, "{}", short);
    s.eq_ignore_ascii_case(name)
}

/// Use the SD card shared by the UI and USB for raw block reads/writes.
///
/// Both run on the same executor and never await while touching the SD card, so borrows never overlap.
impl<D: BlockDevice> RawBlocks for RefCell<Browser<D>>
where
    D::Error: core::error::Error,
{
    fn read_block(&self, lba: u32, buf: &mut [u8; BLOCK]) -> Result<(), DiskError> {
        let browser = self.try_borrow().map_err(|_| DiskError::Read)?;
        browser.device(|dev| {
            let mut block = [Block::new()];
            dev.read(&mut block, BlockIdx(lba)).map_err(|e| {
                log::warn!("SD read error (block {}): {:?}", lba, e);
                DiskError::Read
            })?;
            buf.copy_from_slice(&block[0].contents);
            Ok(())
        })
    }

    fn write_block(&self, lba: u32, buf: &[u8; BLOCK]) -> Result<(), DiskError> {
        let browser = self.try_borrow().map_err(|_| DiskError::Write)?;
        browser.device(|dev| {
            let mut block = [Block::new()];
            block[0].contents.copy_from_slice(buf);
            dev.write(&block, BlockIdx(lba)).map_err(|e| {
                log::warn!("SD write error (block {}): {:?}", lba, e);
                DiskError::Write
            })
        })
    }
}
