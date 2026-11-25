//! Screens and controls (UI application layer).
//!
//! - Status screen: the normal screen showing the inserted image, USB state, and access position
//! - Selector: a screen for browsing SD folders to choose an image
//!
//! Tab switches between the two screens. Settings are saved to `CARDFDD.CFG` at the SD root.

use core::cell::RefCell;
use core::fmt::Write;

use embassy_time::{Duration, Instant};
use esp_hal::gpio::Output;
use heapless::String;
use log::info;
use smart_leds_trait::RGB8;

use crate::board::display::Display;
use crate::board::led::StatusLed;
use crate::board::sdcard;
use crate::board::system;
use crate::config::{self, Config};
use crate::input::{Key, KeyEvent};
use crate::sound;
use crate::storage::bootsector;
use crate::storage::browser::{Browser, EntryKind, ImageSelection, StorageError};
use crate::storage::fat::BLOCK;
use crate::storage::image::{EntryId, ImageDisk, MountError};
use crate::storage::ramdisk;
use crate::ui::color;
use crate::ui::selector::{Selector, VISIBLE_ROWS};
use crate::ui::status::{Access, DiskInfo, StatusView};
use crate::usb;
use crate::usb::command::LinkState;

pub type SdBrowser = Browser<sdcard::Card>;
/// SD card shared by the UI and USB.
pub type SharedSd = RefCell<SdBrowser>;
/// Disk that presents images on the SD card over USB.
pub type SdImage = ImageDisk<SharedSd>;

/// LED color while accessing the SD card.
const ACCESS_LED: RGB8 = RGB8::new(0, 255, 0);
/// LED color when accessed over USB (like a floppy access lamp).
const USB_ACCESS_LED: RGB8 = RGB8::new(255, 160, 0);
/// Time after the last access before the LED turns off.
const ACCESS_LED_HOLD: Duration = Duration::from_millis(80);
/// Time after the last access before the status screen returns to "idle".
const ACCESS_SHOW_HOLD: Duration = Duration::from_millis(500);
/// Minimum interval for updating the access position display (to avoid eating too much SPI bandwidth).
const ACCESS_DRAW_INTERVAL: Duration = Duration::from_millis(60);
/// Time after the eject-prevented warning during which pressing again forces ejection.
const FORCE_EJECT_WINDOW: Duration = Duration::from_secs(3);
/// Blocks written per [`App::tick`] when creating a blank image (one write).
const CREATE_BLOCKS_PER_TICK: u32 = 8;
/// Blank image creation pauses to prioritize USB until this much time has passed since the last USB access.
const CREATE_YIELD_TO_USB: Duration = Duration::from_millis(200);
/// Number of blocks in a blank image (1.44MB).
const BLANK_BLOCKS: u32 = 2880;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Status,
    Selector,
}

/// Blank image being created.
struct Creating {
    file: embedded_sdmmc::RawFile,
    name: String<12>,
    next_block: u32,
}

enum MediaChange {
    Insert(ImageSelection),
    Eject,
}

pub struct App {
    lcd: Display,
    led: StatusLed,
    /// Backlight pin (kept because dropping it turns the backlight off).
    _backlight: Output<'static>,
    sd: &'static SharedSd,
    image: &'static RefCell<SdImage>,
    pub config: Config,
    screen: Screen,
    selector: Selector,
    status: StatusView,
    battery_percent: Option<u8>,
    card_ok: bool,
    /// Buffer for reading sectors (kept in App to save stack).
    sector: [u8; BLOCK],
    /// Buffer for writing blank images (8 blocks).
    chunk: [u8; BLOCK * 8],
    /// Apply the requested media change once the transfer finishes.
    pending_media_change: Option<MediaChange>,
    /// Time the warning was shown because the host prevents ejection (pressing again forces ejection).
    force_eject_armed: Option<Instant>,
    /// Show the USB diagnostics panel in the list area (toggled with the u key).
    show_usb_stats: bool,
    last_stats_draw: Instant,
    /// Time of the last USB access and its display state.
    last_access: Instant,
    last_access_draw: Instant,
    access_shown: bool,
    led_on: bool,
    link: LinkState,
    creating: Option<Creating>,
    /// SD reader mode was requested; reboot into it once no transfer is running.
    reader_requested: bool,
}

impl App {
    pub fn new(
        lcd: Display,
        backlight: Output<'static>,
        led: StatusLed,
        sd: &'static SharedSd,
        image: &'static RefCell<SdImage>,
        config: Config,
    ) -> Self {
        Self {
            lcd,
            led,
            _backlight: backlight,
            sd,
            image,
            config,
            screen: Screen::Selector,
            selector: Selector::default(),
            status: StatusView::default(),
            battery_percent: None,
            card_ok: false,
            sector: [0; BLOCK],
            chunk: [0; BLOCK * 8],
            pending_media_change: None,
            force_eject_armed: None,
            show_usb_stats: false,
            last_stats_draw: Instant::MIN,
            last_access: Instant::MIN,
            last_access_draw: Instant::MIN,
            access_shown: false,
            led_on: false,
            link: LinkState::Detached,
            creating: None,
            reader_requested: false,
        }
    }

    /// Whether a blank image is being created (the main loop spins faster meanwhile).
    pub fn is_busy(&self) -> bool {
        self.creating.is_some()
    }

    pub fn set_battery(&mut self, percent: u8) {
        self.battery_percent = Some(percent);
        match self.screen {
            Screen::Status => self.status.draw_battery(&mut self.lcd, self.battery_percent),
            Screen::Selector => self.selector.draw_battery(&mut self.lcd, self.battery_percent),
        }
    }

    // ---------------------------------------------------------------- Card and settings

    /// (Re)initialize the card, read the config file, and open the image folder.
    #[allow(
        clippy::large_stack_frames,
        reason = "The config read buffer accommodates a full FAT path and is used only on the main task"
    )]
    pub fn mount_card(&mut self) {
        if usb::HOST.is_busy() {
            self.footer("USB transfer in progress", color::ERROR);
            return;
        }
        // The card is being re-initialized, so eject the inserted image
        self.pending_media_change = None;
        self.force_eject_armed = None;
        self.image.borrow_mut().eject();
        self.config.last_image.clear();
        self.led.set(ACCESS_LED);
        let result = {
            let mut sd = self.sd.borrow_mut();
            sd.reset();
            sd.device(|card| sdcard::reset(card));
            // Config file (optional)
            let mut text = [0u8; config::MAX_LEN];
            match sd.read_root_file(config::FILE_NAME, &mut text) {
                Ok(n) => {
                    let mut text = &text[..n];
                    if n == config::MAX_LEN {
                        // The file did not fit, so drop the last line, which may be cut off
                        log::warn!("{} is larger than {} bytes; ignoring the rest", config::FILE_NAME, config::MAX_LEN);
                        let end = text.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
                        text = &text[..end];
                    }
                    self.config.apply(text);
                    info!("Read {}", config::FILE_NAME);
                }
                Err(StorageError::Fs) => info!("{} not found; using defaults", config::FILE_NAME),
                Err(e) => info!("Cannot read {}: {:?}", config::FILE_NAME, e),
            }
            let r = sd.go_to(&self.config.image_dir);
            if r.is_ok() {
                sd.device(|card| sdcard::speed_up(card));
            }
            r
        };
        self.led.off();

        self.card_ok = result.is_ok();
        self.selector.reset();
        self.image.borrow_mut().set_wp_switch(self.config.write_protect);
        sound::set_volume(self.config.volume);
        sound::set_speed_emulation(self.config.speed_emulation);
        match result {
            Ok(()) => {
                let sd = self.sd.borrow();
                info!("SD mounted: {} ({} entries)", sd.path_string(), sd.entries().len());
                drop(sd);
                self.redraw();
            }
            Err(e) => {
                info!("Cannot mount SD: {:?}", e);
                self.screen = Screen::Selector;
                self.draw_card_error(e);
            }
        }
    }

    /// Insert the previous image (when auto_mount is enabled in the settings).
    pub fn auto_insert(&mut self) {
        if !self.card_ok || !self.config.auto_mount || self.config.last_image.is_empty() {
            self.redraw();
            return;
        }
        info!("Looking for the previous image");
        self.led.set(ACCESS_LED);
        let found = self.sd.borrow_mut().find_image(&self.config.last_image);
        self.led.off();
        self.selector.reset();
        if let Ok(Some(selection)) = found {
            if let Some(i) = self.sd.borrow().entries().iter().position(|e| e.id == selection.entry.id) {
                self.selector.move_by(i as isize, i + 1);
            }
            self.redraw();
            self.request_insert(selection);
        } else {
            info!("Previous image not found");
            let image_dir = self.config.image_dir.clone();
            let _ = self.access(|b| b.go_to(&image_dir));
            self.selector.reset();
            self.screen = Screen::Selector;
            self.redraw();
        }
    }

    /// Save to the config file.
    #[inline(never)]
    #[allow(clippy::large_stack_frames, reason = "Serializes a full FAT path on the main task")]
    fn save_config(&mut self) {
        if !self.card_ok {
            return;
        }
        let mut text: String<{ config::MAX_LEN }> = String::new();
        self.config.write_to(&mut text);
        let result = self.access(|b| b.write_root_file(config::FILE_NAME, text.as_bytes()));
        if let Err(e) = result {
            log::warn!("Cannot save {}: {:?}", config::FILE_NAME, e);
        }
    }

    // ---------------------------------------------------------------- Drawing

    fn mounted_id(&self) -> Option<EntryId> {
        self.image.borrow().mounted().map(|m| m.id)
    }

    /// Redraw the current screen.
    pub fn redraw(&mut self) {
        if !self.card_ok && self.screen == Screen::Selector {
            return;
        }
        match self.screen {
            Screen::Status => {
                let image = self.image.borrow();
                let disk = image.mounted().map(|m| DiskInfo {
                    name: &m.name,
                    geometry: m.geometry,
                    write_protected: image.is_write_protected(),
                });
                self.status.draw(&mut self.lcd, disk, self.link, self.battery_percent);
                drop(image);
                self.access_shown = false;
            }
            Screen::Selector => {
                let mounted = self.mounted_id();
                let sd = self.sd.borrow();
                let path = sd.path_string();
                self.selector
                    .draw(&mut self.lcd, &path, sd.entries(), mounted, self.battery_percent);
                if sd.entries().is_empty() {
                    self.selector
                        .draw_message(&mut self.lcd, &["No image files here.", "", "n: create a blank image"], color::DIM);
                }
                let truncated = sd.truncated();
                drop(sd);
                if truncated {
                    self.footer("Too many files (showing first 128)", color::DIM);
                } else {
                    self.draw_selector_footer();
                }
            }
        }
        if self.show_usb_stats {
            self.draw_usb_stats();
        }
    }

    /// Selector footer (the inserted image).
    fn draw_selector_footer(&mut self) {
        let image = self.image.borrow();
        let mut msg: String<64> = String::new();
        let fg = match image.mounted() {
            Some(m) => {
                let _ = write!(msg, "IN: {}", m.name);
                if image.is_write_protected() {
                    let _ = msg.push_str(" [WP]");
                }
                color::ACCENT
            }
            None => {
                let _ = msg.push_str("Enter:insert Tab:status n:new");
                if image.wp_switch() {
                    let _ = msg.push_str(" [WP]");
                }
                color::DIM
            }
        };
        drop(image);
        truncate(&mut msg, crate::ui::COLS - 1);
        self.footer(&msg, fg);
    }

    fn footer(&mut self, text: &str, fg: embedded_graphics::pixelcolor::Rgb565) {
        match self.screen {
            Screen::Status => self.status.draw_footer(&mut self.lcd, text, fg),
            Screen::Selector => self.selector.draw_footer(&mut self.lcd, text, fg),
        }
    }

    fn draw_card_error(&mut self, error: StorageError) {
        self.selector
            .draw(&mut self.lcd, "(no card)", &[], None, self.battery_percent);
        let lines: &[&str] = match error {
            StorageError::NoCard => &["No SD card.", "", "Insert a FAT32 card and", "press Enter to retry."],
            StorageError::Fs => &["Cannot read the SD card.", "", "Use FAT16/FAT32 (not exFAT)", "and press Enter to retry."],
        };
        self.selector.draw_message(&mut self.lcd, lines, color::ERROR);
        self.selector.draw_footer(&mut self.lcd, "Enter: retry", color::DIM);
    }

    /// Show the USB diagnostics panel in the list area (see [`usb_stats_lines`] for the meaning of each row).
    #[allow(
        clippy::large_stack_frames,
        reason = "Uses a bit over 1KB for display strings. Only called from the main task, so there is enough headroom"
    )]
    fn draw_usb_stats(&mut self) {
        self.last_stats_draw = Instant::now();
        let lines = usb_stats_lines();
        // draw_message starts one row below the top of the list. Leave the footer clear.
        let refs: [&str; VISIBLE_ROWS - 1] = core::array::from_fn(|i| lines[i].as_str());
        self.selector.draw_message(&mut self.lcd, &refs, color::FG);
    }

    fn set_screen(&mut self, screen: Screen) {
        self.screen = screen;
        self.redraw();
    }

    // ---------------------------------------------------------------- Periodic processing

    /// Called periodically from the main loop.
    pub fn tick(&mut self) {
        usb::sense_trace::sample();
        self.poll_drive();
        self.poll_activity();
        self.poll_link();
        self.step_create();
        if self.show_usb_stats && self.last_stats_draw.elapsed() >= Duration::from_millis(500) {
            self.draw_usb_stats();
        }
    }

    /// Handle queued media changes and eject requests once no transfer can touch the disk.
    fn poll_drive(&mut self) {
        if self.reader_requested && !usb::HOST.is_busy() {
            Self::enter_reader_now();
        }
        if !usb::HOST.is_busy() {
            let io_error = self.image.borrow_mut().take_io_error();
            let host_eject = usb::HOST.take_eject_request();
            if io_error {
                self.do_eject("Ejected due to SD error");
                self.footer("SD error! Disk ejected.", color::ERROR);
            } else if host_eject {
                self.do_eject("Ejected at host request");
            } else if let Some(change) = self.pending_media_change.take() {
                match change {
                    MediaChange::Insert(selection) if self.can_insert(&selection) => {
                        // Save after do_insert returns so its path buffer and save_config's
                        // serialization buffer are never on the stack at the same time
                        if self.do_insert(&selection) {
                            self.save_config();
                        }
                    }
                    MediaChange::Insert(_) => {}
                    MediaChange::Eject => self.do_eject("Ejected (after transfer completed)"),
                }
            }
        }
        if self
            .force_eject_armed
            .is_some_and(|t| t.elapsed() >= FORCE_EJECT_WINDOW)
        {
            self.force_eject_armed = None;
            self.redraw();
        }
    }

    /// USB access lamp and access position display.
    fn poll_activity(&mut self) {
        if let Some(a) = usb::ACTIVITY.try_take() {
            self.last_access = Instant::now();
            if !self.led_on {
                self.led.set(USB_ACCESS_LED);
                self.led_on = true;
            }
            if self.screen == Screen::Status
                && !self.show_usb_stats
                && self.last_access_draw.elapsed() >= ACCESS_DRAW_INTERVAL
            {
                let geometry = self.image.borrow().mounted().map(|m| m.geometry);
                if let Some(g) = geometry {
                    let (cylinder, head, sector) = g.chs(a.lba);
                    self.status.draw_access(
                        &mut self.lcd,
                        Access {
                            cylinder,
                            head,
                            sector,
                            write: a.write,
                        },
                    );
                    self.last_access_draw = Instant::now();
                    self.access_shown = true;
                }
            }
        }
        if self.led_on && self.last_access.elapsed() > ACCESS_LED_HOLD {
            self.led.off();
            self.led_on = false;
        }
        if self.access_shown && self.last_access.elapsed() > ACCESS_SHOW_HOLD {
            self.access_shown = false;
            if self.screen == Screen::Status && !self.show_usb_stats {
                self.status.draw_idle(&mut self.lcd);
            }
        }
    }

    /// USB connection state display.
    fn poll_link(&mut self) {
        let link = usb::HOST.link_state();
        if link != self.link {
            info!("USB: {:?}", link);
            self.link = link;
            if self.screen == Screen::Status {
                self.status.draw_link(&mut self.lcd, link);
            }
        }
    }

    // ---------------------------------------------------------------- Key handling

    pub fn on_key(&mut self, event: &KeyEvent) {
        if self.creating.is_some() {
            return;
        }

        if event.mods.ctrl && event.mods.alt {
            match event.key {
                Key::Char('d') => {
                    info!("Rebooting into download mode");
                    self.footer("Rebooting to download mode...", color::ACCENT);
                    system::reboot_to_download_mode();
                }
                Key::Char('r') => {
                    info!("Rebooting");
                    system::reboot();
                }
                // Ctrl + Alt + S forms a rectangle in the key matrix (no diodes), so the scan also sees
                // a phantom Shift and the key arrives as 'S'. Accept both cases
                Key::Char('s' | 'S') => self.enter_reader(),
                _ => {}
            }
            return;
        }

        if !self.card_ok {
            if event.key == Key::Enter {
                self.mount_card();
                if self.card_ok {
                    self.auto_insert();
                }
            }
            return;
        }

        match event.key {
            Key::Tab => {
                let next = match self.screen {
                    Screen::Status => Screen::Selector,
                    Screen::Selector => Screen::Status,
                };
                return self.set_screen(next);
            }
            Key::Char('e') => return self.eject(),
            Key::Char('w') => return self.toggle_write_protect(),
            Key::Char('s') => return self.cycle_volume(),
            Key::Char('r') => return self.toggle_speed_emulation(),
            Key::Char('u') => {
                self.show_usb_stats = !self.show_usb_stats;
                if self.show_usb_stats {
                    self.draw_usb_stats();
                } else {
                    self.redraw();
                }
                return;
            }
            _ => {}
        }

        match self.screen {
            Screen::Status => {
                if event.key == Key::Enter {
                    self.set_screen(Screen::Selector);
                }
            }
            Screen::Selector => self.on_selector_key(event.key),
        }
    }

    fn on_selector_key(&mut self, key: Key) {
        let len = self.sd.borrow().entries().len();
        match key {
            Key::Char(';') | Key::Up => self.move_cursor(-1, len),
            Key::Char('.') | Key::Down => self.move_cursor(1, len),
            Key::Char(',') | Key::Left => self.move_cursor(-(VISIBLE_ROWS as isize), len),
            Key::Char('/') | Key::Right => self.move_cursor(VISIBLE_ROWS as isize, len),
            Key::Enter => self.open_selected(),
            Key::Esc => self.set_screen(Screen::Status),
            Key::Char('`') | Key::Backspace if !self.sd.borrow().is_root() => {
                let result = self.access(|b| b.leave());
                self.after_dir_change(result);
            }
            Key::Char('i') => self.show_image_info(self.selector.cursor()),
            Key::Char('n') => self.start_create(),
            _ => {}
        }
    }

    fn move_cursor(&mut self, delta: isize, len: usize) {
        let mounted = self.mounted_id();
        self.selector.move_by(delta, len);
        if self.show_usb_stats {
            return;
        }
        let sd = self.sd.borrow();
        self.selector.draw_list(&mut self.lcd, sd.entries(), mounted);
    }

    fn open_selected(&mut self) {
        let index = self.selector.cursor();
        let (kind, mountable) = match self.sd.borrow().entries().get(index) {
            Some(e) => (e.kind, e.is_mountable()),
            None => return,
        };
        match kind {
            EntryKind::Parent | EntryKind::Dir => {
                let result = self.access(|b| b.enter(index));
                self.after_dir_change(result);
            }
            EntryKind::Image(_) if mountable => self.insert(index),
            EntryKind::Image(_) => self.show_image_info(index),
        }
    }

    fn after_dir_change(&mut self, result: Result<(), StorageError>) {
        match result {
            Ok(()) => {
                self.selector.reset();
                self.redraw();
            }
            Err(e) => self.card_failed(e),
        }
    }

    /// The card stopped responding or cannot be read. Eject (after the current transfer) and show the error.
    fn card_failed(&mut self, error: StorageError) {
        self.card_ok = false;
        self.pending_media_change = if usb::HOST.is_busy() {
            Some(MediaChange::Eject)
        } else {
            self.image.borrow_mut().eject();
            None
        };
        self.screen = Screen::Selector;
        self.draw_card_error(error);
    }

    // ---------------------------------------------------------------- Insert and eject

    /// Request insertion of the `index`-th image in the current listing.
    fn insert(&mut self, index: usize) {
        let selection = {
            let sd = self.sd.borrow();
            let Some(entry) = sd.entries().get(index).cloned() else { return };
            sd.select_image(entry)
        };
        self.request_insert(selection);
    }

    fn can_insert(&mut self, selection: &ImageSelection) -> bool {
        if !self.card_ok || !selection.entry.is_mountable() {
            self.footer("Cannot insert this file", color::ERROR);
            return false;
        }
        if usb::HOST.prevent_removal() && self.image.borrow().mounted().is_some() {
            self.footer("PC is using it! Eject first", color::ERROR);
            return false;
        }
        true
    }

    fn request_insert(&mut self, selection: ImageSelection) {
        if !self.can_insert(&selection) {
            return;
        }
        self.pending_media_change = Some(MediaChange::Insert(selection));
        if usb::HOST.is_busy() {
            self.footer("Insert after USB transfer", color::ACCENT);
        } else {
            self.poll_drive();
        }
    }

    /// Insert the selected image now. Returns true when `last_image` changed and needs saving.
    #[inline(never)]
    #[allow(clippy::large_stack_frames, reason = "Builds the selected FAT path on the main task")]
    fn do_insert(&mut self, selection: &ImageSelection) -> bool {
        let entry = &selection.entry;
        let mut path = config::ImagePath::new();
        if let Err(e) = self.access(|b| b.image_path(selection, &mut path)) {
            info!("Cannot resolve image path: {} ({:?})", entry.name, e);
            match e {
                StorageError::NoCard => self.card_failed(e),
                StorageError::Fs => self.footer("Cannot find selected image", color::ERROR),
            }
            return false;
        }
        self.led.set(ACCESS_LED);
        let result = self.image.borrow_mut().mount(entry, entry.id);
        self.led.off();
        match result {
            Ok(()) => {
                let image = self.image.borrow();
                if let Some(m) = image.mounted() {
                    info!(
                        "Inserted: {} ({} extents{})",
                        m.name,
                        m.extent_count(),
                        if m.is_contiguous() { ", contiguous" } else { "" }
                    );
                }
                drop(image);
                sound::post(sound::Event::Insert);

                self.force_eject_armed = None;
                self.set_screen(Screen::Status);
                if self.config.last_image != path {
                    self.config.last_image = path;
                    return true;
                }
            }
            Err(e) => {
                info!("Cannot insert: {} ({:?})", entry.name, e);
                let msg = match e {
                    MountError::NotMountable => "Cannot insert this file",
                    MountError::Io => "SD read error",
                    MountError::Fat(_) => "Unsupported file layout",
                };
                self.footer(msg, color::ERROR);
            }
        }
        false
    }

    /// Eject key. Warns when the host prevents ejection; pressing again forces ejection.
    fn eject(&mut self) {
        if self.image.borrow().mounted().is_none() {
            return;
        }
        let armed = self
            .force_eject_armed
            .take()
            .is_some_and(|t| t.elapsed() < FORCE_EJECT_WINDOW);
        if usb::HOST.prevent_removal() && !armed {
            info!("Host is preventing medium removal");
            self.force_eject_armed = Some(Instant::now());
            self.footer("PC is using it! e again: force eject", color::ERROR);
            return;
        }
        if usb::HOST.is_busy() {
            // A read/write is in progress, so eject after it finishes
            self.pending_media_change = Some(MediaChange::Eject);
            self.footer("Ejecting...", color::ACCENT);
        } else {
            self.do_eject("Ejected");
        }
    }

    fn do_eject(&mut self, reason: &str) {
        self.pending_media_change = None;
        self.force_eject_armed = None;
        if self.image.borrow().mounted().is_none() {
            return;
        }
        info!("{}", reason);
        self.image.borrow_mut().eject();
        sound::post(sound::Event::Eject);
        // After ejecting, insert nothing on the next boot
        if !self.config.last_image.is_empty() {
            self.config.last_image.clear();
            self.save_config();
        }
        self.redraw();
    }

    /// Ctrl+Alt+S: reboot into SD reader mode (the whole card is exposed over USB).
    ///
    /// Nothing needs writing first: floppy writes go straight to the card and an inserted image never
    /// changes the FAT, so the reboot just drops it. The config keeps the last image, so it is inserted
    /// again after coming back. If the host prevents removal of the inserted floppy, warn first;
    /// pressing again within the window goes anyway. Not while a blank image is being written.
    fn enter_reader(&mut self) {
        if self.reader_requested || self.creating.is_some() {
            return;
        }
        let armed = self
            .force_eject_armed
            .take()
            .is_some_and(|t| t.elapsed() < FORCE_EJECT_WINDOW);
        if self.image.borrow().mounted().is_some() && usb::HOST.prevent_removal() && !armed {
            info!("Host is preventing medium removal");
            self.force_eject_armed = Some(Instant::now());
            self.footer("PC is using it! Ctrl+Alt+S again: force", color::ERROR);
            return;
        }
        self.footer("Starting SD reader mode...", color::ACCENT);
        // A read/write in progress finishes first (poll_drive reboots once it is done)
        self.reader_requested = true;
        if !usb::HOST.is_busy() {
            Self::enter_reader_now();
        }
    }

    fn enter_reader_now() -> ! {
        info!("Rebooting into SD reader mode");
        system::reboot_to_reader()
    }

    /// Cycle the volume 0 -> 1 -> 2 -> 3 -> 0.
    fn cycle_volume(&mut self) {
        let volume = (sound::volume() + 1) % (sound::MAX_VOLUME + 1);
        sound::set_volume(volume);
        self.config.volume = volume;
        info!("Volume: {}", volume);
        let mut msg: String<32> = String::new();
        let _ = write!(msg, "Sound volume: {}{}", volume, if volume == 0 { " (mute)" } else { "" });
        self.footer(&msg, color::ACCENT);
        self.save_config();
    }

    /// Toggle realistic speed mode.
    fn toggle_speed_emulation(&mut self) {
        let on = !sound::speed_emulation();
        sound::set_speed_emulation(on);
        self.config.speed_emulation = on;
        info!("Realistic speed mode: {}", on);
        self.footer(if on { "Real drive speed: ON" } else { "Real drive speed: OFF" }, color::ACCENT);
        self.save_config();
    }

    fn toggle_write_protect(&mut self) {
        let mut image = self.image.borrow_mut();
        let wp = !image.wp_switch();
        image.set_wp_switch(wp);
        drop(image);
        info!("Write-protect switch: {}", wp);
        self.config.write_protect = wp;
        self.save_config();
        self.redraw();
    }

    // ---------------------------------------------------------------- Info display

    /// Read the image's first sector and show a summary in the footer.
    #[allow(
        clippy::large_stack_frames,
        reason = "Uses a bit over 1KB for log formatting and temporary strings. Only called from the main task, so there is enough headroom"
    )]
    fn show_image_info(&mut self, index: usize) {
        let (name, size, mountable, format, id) = match self.sd.borrow().entries().get(index) {
            Some(e) => {
                let format = match e.kind {
                    EntryKind::Image(f) => f,
                    _ => return,
                };
                (e.name.clone(), e.size, e.is_mountable(), format, e.id)
            }
            None => return,
        };
        info!("Info: {} ({} bytes)", name, size);

        self.led.set(ACCESS_LED);
        let result = self.image.borrow().read_file_head(id, &mut self.sector);
        self.led.off();

        let mut msg: String<64> = String::new();
        let fg = match result {
            Ok(()) => match bootsector::parse(&self.sector) {
                Some(boot) if boot.signature => {
                    let _ = write!(
                        msg,
                        "{} {}B x{} {}",
                        boot.oem_str(),
                        boot.bytes_per_sector,
                        boot.total_sectors,
                        if mountable { "OK" } else { "(unsupported size)" }
                    );
                    if mountable { color::FG } else { color::DIM }
                }
                _ => {
                    let _ = msg.push_str("No FAT boot sector");
                    color::DIM
                }
            },
            Err(e) => {
                let _ = write!(msg, "Read error: {:?}", e);
                color::ERROR
            }
        };
        if format.is_none() {
            msg.clear();
            let _ = msg.push_str("Unknown image size");
        }
        self.footer(&msg, fg);
    }

    // ---------------------------------------------------------------- Blank image creation

    /// Start creating a blank 1.44MB image (NEWnnn.IMG) in the current folder.
    fn start_create(&mut self) {
        let mut name: String<12> = String::new();
        let exists = |sd: &SdBrowser, n: &str| {
            sd.entries().iter().any(|e| {
                let mut short: String<16> = String::new();
                let _ = write!(short, "{}", e.short);
                short.eq_ignore_ascii_case(n)
            })
        };
        let found = (1..1000).any(|i| {
            name.clear();
            let _ = write!(name, "NEW{:03}.IMG", i);
            !exists(&self.sd.borrow(), &name)
        });
        if !found {
            return self.footer("No free name (NEW001-999)", color::ERROR);
        }
        match self.sd.borrow_mut().create_file(&name) {
            Ok(file) => {
                info!("Creating blank image: {}", name);
                self.creating = Some(Creating {
                    file,
                    name,
                    next_block: 0,
                });
            }
            Err(e) => {
                log::warn!("Cannot create blank image: {:?}", e);
                self.footer("Cannot create the file", color::ERROR);
            }
        }
    }

    /// Write the blank image a little at a time (limiting each write so USB is not stalled).
    fn step_create(&mut self) {
        if self.creating.is_none() {
            return;
        }
        // Pause during and right after USB transfers (so creation does not hog the SD card and stall USB)
        if usb::HOST.is_busy() || self.last_access.elapsed() < CREATE_YIELD_TO_USB {
            return;
        }
        let Some(c) = self.creating.as_mut() else {
            return;
        };
        let file = c.file;
        let end = (c.next_block + CREATE_BLOCKS_PER_TICK).min(BLANK_BLOCKS);
        let mut block = c.next_block;
        let mut result = Ok(());
        self.led.set(ACCESS_LED);
        while block < end && result.is_ok() {
            let n = (end - block).min(8);
            for i in 0..n {
                let range = i as usize * BLOCK..(i as usize + 1) * BLOCK;
                ramdisk::blank_sector(block + i, &mut self.chunk[range]);
            }
            result = self.sd.borrow_mut().write_open_file(file, &self.chunk[..n as usize * BLOCK]);
            block += n;
        }
        self.led.off();

        let c = self.creating.as_mut().expect("blank image creation in progress");
        c.next_block = block;
        let percent = block * 100 / BLANK_BLOCKS;
        let mut msg: String<40> = String::new();
        let _ = write!(msg, "Creating {} {}%", c.name, percent);
        let done = block >= BLANK_BLOCKS;
        if result.is_err() || done {
            let c = self.creating.take().expect("blank image creation in progress");
            let closed = self.sd.borrow_mut().close_created_file(c.file);
            match result.and(closed) {
                Ok(()) => info!("Created blank image: {}", c.name),
                Err(e) => log::warn!("Failed to create blank image: {:?}", e),
            }
            self.selector.reset();
            self.redraw();
        } else {
            self.footer(&msg, color::ACCENT);
        }
    }

    // ---------------------------------------------------------------- Development

    /// Commands received from the development CDC (for testing).
    #[cfg(feature = "dev-cdc")]
    #[allow(
        clippy::large_stack_frames,
        reason = "The cfg command serializes a full FAT path on the main task"
    )]
    pub fn dev_command(&mut self, line: &str) {
        let (cmd, arg) = line.split_once(' ').unwrap_or((line, ""));
        match cmd {
            "ls" => {
                let sd = self.sd.borrow();
                info!("{}", sd.path_string());
                for e in sd.entries() {
                    info!("  {:?} {}", e.kind == EntryKind::Dir, e.name);
                }
            }
            "insert" => {
                let found = self
                    .sd
                    .borrow()
                    .entries()
                    .iter()
                    .position(|e| e.name.eq_ignore_ascii_case(arg));
                match found {
                    Some(i) => self.insert(i),
                    None => info!("Not found: {}", arg),
                }
            }
            "eject" => self.eject(),
            "stats" => {
                for line in usb_stats_lines() {
                    info!("{}", line);
                }
            }
            "wp" => {
                let want = arg == "on";
                if self.image.borrow().wp_switch() != want {
                    self.toggle_write_protect();
                }
            }
            "newimg" => self.start_create(),
            "reader" => self.enter_reader(),
            "perf" => match arg {
                "reset" => usb::perf::PERF.reset(),
                _ => usb::perf::PERF.log(),
            },
            "snd" => info!(
                "volume {} / realistic speed {} / audio underruns {}",
                sound::volume(),
                sound::speed_emulation(),
                crate::board::audio::UNDERRUNS.load(core::sync::atomic::Ordering::Relaxed)
            ),
            "key" => {
                // Substitute for key input (tab / enter / esc / single character)
                let key = match arg {
                    "tab" => Key::Tab,
                    "enter" => Key::Enter,
                    "esc" => Key::Esc,
                    s => match s.chars().next() {
                        Some(c) => Key::Char(c),
                        None => return,
                    },
                };
                self.on_key(&KeyEvent {
                    pos: crate::input::KeyPos { row: 0, col: 0 },
                    key,
                    pressed: true,
                    mods: crate::input::Modifiers::default(),
                });
            }
            "cfg" => {
                info!("board: {}", crate::board::model().name());
                let mut text: String<{ config::MAX_LEN }> = String::new();
                self.config.write_to(&mut text);
                for l in text.lines() {
                    info!("{}", l);
                }
            }
            _ => info!("Unknown command: {}", line),
        }
    }

    /// Turn on the access lamp only while accessing the SD card.
    fn access<R>(&mut self, f: impl FnOnce(&mut SdBrowser) -> R) -> R {
        self.led.set(ACCESS_LED);
        let r = f(&mut self.sd.borrow_mut());
        self.led.off();
        r
    }
}

fn truncate<const N: usize>(s: &mut String<N>, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
}

/// Rows of the USB diagnostics display (used by the diagnostics panel and the development command `stats`).
///
/// ```text
/// st:wait C27 F11 S2 now+15.2s        stage / commands / failures / ADSC stalls / time since configured
/// rst1 cfg1 enum:120ms 1st:+1         bus resets / configurations / last reset-to-configuration time / time to first command
/// E0 R0 J0 last+0.9s fail:5a!3a00     disk errors / CBI resets / rejected / last command / last failure
/// pend int:0 blk:0 disc int:0 blk:0   unread IN (now) / discarded count
/// 12 23 12 25! 03                     recent commands (oldest first, ! = failed)
/// sense blk:2/2 int:0/2               REQUEST SENSE IN (observed drained / queued)
/// RS 062800:++ 023a00:+r 023a00:+p    recent sense key/ASC/ASCQ and Bulk/Interrupt state
/// susp1@-0.1s res0 di:0080/2090       suspend count and last time / resumes / DIEPINT
/// 03>00:2005 ...                      200ms gaps of 200ms or more (previous > next : ms)
/// ```
#[allow(
    clippy::large_stack_frames,
    reason = "Uses a bit over 1KB for display strings. Only called from the main task, so there is enough headroom"
)]
pub fn usb_stats_lines() -> [String<40>; 9] {
    use crate::usb::command::{HISTORY_LEN, Stage};
    use core::sync::atomic::Ordering::Relaxed;
    let st = &usb::HOST.stats;

    let cfg = st.configured_ms.load(Relaxed);
    let now = st.now();
    let mut l1: String<40> = String::new();
    let stage = match Stage::from_u32(st.stage.load(Relaxed)) {
        Stage::WaitCommand => "wait",
        Stage::Data => "data",
        Stage::Status => "stat",
    };
    let since = now.wrapping_sub(cfg);
    let _ = write!(
        l1,
        "st:{} C{} F{} S{} now+{}.{}s",
        stage,
        st.commands.load(Relaxed),
        st.failed.load(Relaxed),
        st.adsc_stalls.load(Relaxed),
        since / 1000,
        since % 1000 / 100
    );

    let mut l2: String<40> = String::new();
    let first = st.first_command_ms.load(Relaxed);
    let _ = write!(
        l2,
        "rst{} cfg{} enum:{}ms 1st:+{}",
        st.bus_resets.load(Relaxed),
        st.configurations.load(Relaxed),
        st.enumeration_ms.load(Relaxed),
        if first != 0 { first.wrapping_sub(cfg) } else { 0 }
    );

    let mut l3: String<40> = String::new();
    let _ = write!(
        l3,
        "E{} R{} J{} ",
        st.disk_errors.load(Relaxed),
        st.resets.load(Relaxed),
        st.rejected.load(Relaxed)
    );
    let last_cmd = st.last_command_ms.load(Relaxed);
    if last_cmd != 0 {
        let t = last_cmd.wrapping_sub(cfg);
        let _ = write!(l3, "last+{}.{}s", t / 1000, t % 1000 / 100);
    }
    let lf = st.last_failure.load(Relaxed);
    if lf != 0 {
        let _ = write!(l3, " fail:{:02x}!{:02x}{:02x}", lf >> 24, (lf >> 8) & 0xFF, lf & 0xFF);
    }
    let unknown = st.last_unknown.load(Relaxed);
    if unknown != 0 {
        let _ = write!(l3, " U{:02x}", unknown - 1);
    }

    let mut l4: String<40> = String::new();
    let _ = write!(
        l4,
        "pend int:{} blk:{} disc int:{} blk:{}",
        usb::in_pending(st.int_ep.load(Relaxed) as usize) as u8,
        usb::in_pending(st.bulk_in_ep.load(Relaxed) as usize) as u8,
        st.discarded_int.load(Relaxed),
        st.discarded_bulk.load(Relaxed)
    );

    let mut l5: String<40> = String::new();
    let pos = st.history_pos.load(Relaxed) as usize;
    for i in 0..HISTORY_LEN {
        let v = st.history[(pos + i) % HISTORY_LEN].load(Relaxed);
        if v & 0x8000_0000 != 0 {
            let mark = if v & 0x8000 != 0 { "!" } else { "" };
            let _ = write!(l5, "{:02x}{} ", (v >> 16) & 0xFF, mark);
        }
    }

    let mut l6: String<40> = String::new();
    let susp_rel = st.last_suspend_ms.load(Relaxed) as i64 - cfg as i64;
    let _ = write!(
        l6,
        "susp{}@{}.{}s res{} di:{:04x}/{:04x}",
        st.suspends.load(Relaxed),
        susp_rel / 1000,
        (susp_rel.abs() % 1000) / 100,
        st.resumes.load(Relaxed),
        usb::in_interrupt_bits(st.int_ep.load(Relaxed) as usize),
        usb::in_interrupt_bits(st.bulk_in_ep.load(Relaxed) as usize)
    );

    let mut l7: String<40> = String::new();
    let gpos = st.gap_pos.load(Relaxed) as usize;
    for i in 0..HISTORY_LEN {
        let v = st.gaps[(gpos + i) % HISTORY_LEN].load(Relaxed);
        if v != 0 {
            let _ = write!(l7, "{:02x}>{:02x}:{} ", v >> 24, (v >> 16) & 0xFF, v & 0xFFFF);
        }
    }

    let mut sense = String::new();
    let (bulk_drained, bulk_queued) = usb::sense_trace::BULK.counts();
    let (interrupt_drained, interrupt_queued) = usb::sense_trace::INTERRUPT.counts();
    let _ = write!(sense, "sense blk:{bulk_drained}/{bulk_queued} int:{interrupt_drained}/{interrupt_queued}");

    let mut recent_sense = String::new();
    let _ = recent_sense.push_str("RS");
    for (code, bulk, interrupt) in usb::sense_trace::recent() {
        let _ = write!(recent_sense, " {code:06x}:{bulk}{interrupt}");
    }

    [l1, l2, l3, l4, l5, sense, recent_sense, l6, l7]
}
