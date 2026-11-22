//! Reboot.

use esp_hal::peripherals::{LPWR, USB_DEVICE, USB_WRAP};

/// Normal reboot.
pub fn reboot() -> ! {
    restore_usb_serial_jtag();
    esp_hal::system::software_reset()
}

/// Reboot into download mode (the ROM's serial flashing mode).
///
/// Once USB is used as OTG (mass storage), espflash's automatic reset no longer works, so
/// allow entering the flash-wait state via device keys or a CDC command.
pub fn reboot_to_download_mode() -> ! {
    restore_usb_serial_jtag();
    LPWR::regs()
        .option1()
        .modify(|_, w| w.force_download_boot().set_bit());
    esp_hal::system::software_reset()
}

/// Return USB to USB-Serial-JTAG (the state at boot).
///
/// Neither the PHY selection (RTC) nor the USB-Serial-JTAG pull-up setting is restored by a
/// software reset. Without restoring them, the ROM download mode after reboot runs on the OTG side and
/// espflash cannot flash, or the pull-up stays off and the host cannot see the device.
///
/// Before switching, remove the OTG-side pull-up and wait a while so the host sees a "disconnect"
/// (otherwise the host assumes the OTG device is still connected).
fn restore_usb_serial_jtag() {
    const DETACH_MS: u32 = 200;

    USB_WRAP::regs().otg_conf().modify(|_, w| {
        w.pad_pull_override().set_bit();
        w.dp_pullup().clear_bit();
        w.dm_pullup().clear_bit();
        w.dp_pulldown().set_bit();
        w.dm_pulldown().set_bit()
    });
    esp_hal::delay::Delay::new().delay_millis(DETACH_MS);

    LPWR::regs().usb_conf().modify(|_, w| {
        w.sw_hw_usb_phy_sel().clear_bit();
        w.sw_usb_phy_sel().clear_bit()
    });
    USB_DEVICE::regs().conf0().modify(|_, w| {
        w.pad_pull_override().clear_bit();
        w.dp_pulldown().clear_bit();
        w.dm_pulldown().clear_bit();
        w.dp_pullup().set_bit();
        w.dm_pullup().clear_bit()
    });
}
