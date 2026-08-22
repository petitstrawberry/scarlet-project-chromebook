// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Chrome EC USB precision-touchpad support for detachable Chromebook bases.
//!
//! The USB identity, interface layout, report format, and stable reconnect
//! policy are intentionally kept in the Chromebook project. Scarlet's generic
//! xHCI layer only configures a claimed interrupt-IN endpoint and forwards its
//! payloads through the public USB interface-driver hook.

extern crate alloc;

use alloc::{sync::Arc, vec::Vec};

use scarlet::{
    device::{
        Device,
        input::{
            event_device::EventDevice,
            event_types::{EV_KEY, EV_REL, EV_SYN},
            key_codes::BTN_LEFT,
            key_values::{KEY_PRESS, KEY_RELEASE},
            rel_codes::{REL_X, REL_Y},
            syn_codes::SYN_REPORT,
        },
        manager::DeviceManager,
        usb::{
            UsbDeviceIdentity, UsbDeviceLocation, UsbInterruptInDriver, UsbInterruptInEndpointInfo,
            UsbInterruptInHandler,
        },
    },
    early_println,
    sync::IrqSpinLock,
};

const GOOGLE_VENDOR_ID: u16 = 0x18d1;
const ZED_PRODUCT_ID: u16 = 0x504c;
const HID_CLASS: u8 = 0x03;
const TOUCHPAD_INTERFACE: u8 = 2;
const TOUCHPAD_ENDPOINT: u8 = 0x83;

const REPORT_ID: u8 = 0x01;
const REPORT_SIZE: usize = 44;
const FINGER_COUNT: usize = 5;
const FINGER_SIZE: usize = 8;
const FINGERS_OFFSET: usize = 1;
const COUNT_BUTTON_OFFSET: usize = 41;
const POINTER_SCALE: i32 = 2;

#[derive(Default)]
struct PointerState {
    last_contact: Option<(u8, u16, u16)>,
    remainder_x: i32,
    remainder_y: i32,
    button_pressed: bool,
}

struct ChromeEcUsbTouchpad {
    event_device: Arc<EventDevice>,
    state: IrqSpinLock<PointerState>,
}

impl ChromeEcUsbTouchpad {
    fn new() -> Self {
        Self {
            event_device: Arc::new(EventDevice::new("touchpad")),
            state: IrqSpinLock::new(PointerState::default()),
        }
    }

    fn process_report(&self, report: &[u8]) -> Result<(), &'static str> {
        if report.len() < REPORT_SIZE || report[0] != REPORT_ID {
            return Err("chrome-ec-usb-touchpad: malformed input report");
        }

        let pressed = report[COUNT_BUTTON_OFFSET] & 0x80 != 0;
        let contact = contact_from_report(report);
        let (button_change, dx, dy) = {
            let mut state = self.state.lock();
            let button_change = (pressed != state.button_pressed).then_some(pressed);
            state.button_pressed = pressed;

            let (dx, dy) = if let (Some((id, x, y)), Some((last_id, last_x, last_y))) =
                (contact, state.last_contact)
                && id == last_id
            {
                let scaled_x = i32::from(x) - i32::from(last_x) + state.remainder_x;
                let scaled_y = i32::from(y) - i32::from(last_y) + state.remainder_y;
                let dx = scaled_x / POINTER_SCALE;
                let dy = scaled_y / POINTER_SCALE;
                state.remainder_x = scaled_x % POINTER_SCALE;
                state.remainder_y = scaled_y % POINTER_SCALE;
                (dx, dy)
            } else {
                state.remainder_x = 0;
                state.remainder_y = 0;
                (0, 0)
            };
            state.last_contact = contact;
            (button_change, dx, dy)
        };

        if let Some(pressed) = button_change {
            self.event_device.push_event(
                EV_KEY,
                BTN_LEFT,
                if pressed { KEY_PRESS } else { KEY_RELEASE },
            );
        }
        if dx != 0 {
            self.event_device.push_event(EV_REL, REL_X, dx);
        }
        if dy != 0 {
            self.event_device.push_event(EV_REL, REL_Y, dy);
        }
        self.event_device.push_event(EV_SYN, SYN_REPORT, 0);
        Ok(())
    }

    fn reset_on_disconnect(&self) {
        let release_button = {
            let mut state = self.state.lock();
            let release_button = state.button_pressed;
            *state = PointerState::default();
            release_button
        };
        if release_button {
            self.event_device.push_event(EV_KEY, BTN_LEFT, KEY_RELEASE);
        }
        self.event_device.push_event(EV_SYN, SYN_REPORT, 0);
    }
}

impl UsbInterruptInHandler for ChromeEcUsbTouchpad {
    fn handle_report(&self, report: &[u8]) -> Result<(), &'static str> {
        self.process_report(report)
    }

    fn disconnected(&self) {
        self.reset_on_disconnect();
    }
}

struct RegisteredTouchpad {
    location: UsbDeviceLocation,
    device: Arc<ChromeEcUsbTouchpad>,
}

struct ChromeEcUsbTouchpadDriver;

impl UsbInterruptInDriver for ChromeEcUsbTouchpadDriver {
    fn name(&self) -> &'static str {
        "chrome-ec-usb-touchpad"
    }

    fn matches(&self, device: &UsbDeviceIdentity, endpoint: &UsbInterruptInEndpointInfo) -> bool {
        device.vendor_id == GOOGLE_VENDOR_ID
            && device.product_id == ZED_PRODUCT_ID
            && endpoint.interface_number == TOUCHPAD_INTERFACE
            && endpoint.alternate_setting == 0
            && endpoint.interface_class == HID_CLASS
            && endpoint.interface_subclass == 0
            && endpoint.interface_protocol == 0
            && endpoint.endpoint_address == TOUCHPAD_ENDPOINT
            && usize::from(endpoint.max_packet_size) >= REPORT_SIZE
    }

    fn bind(
        &self,
        device: &UsbDeviceIdentity,
        endpoint: &UsbInterruptInEndpointInfo,
        location: UsbDeviceLocation,
    ) -> Result<Arc<dyn UsbInterruptInHandler>, &'static str> {
        if !self.matches(device, endpoint) {
            return Err("chrome-ec-usb-touchpad: descriptor mismatch during bind");
        }

        let mut registered = REGISTERED_TOUCHPADS.lock();
        if let Some(existing) = registered
            .iter()
            .find(|existing| existing.location == location)
        {
            early_println!(
                "[chrome-ec-usb-touchpad] reusing {} at host={} root-port={} route={:#x}",
                existing.device.event_device.get_name(),
                location.host_id,
                location.root_port_id,
                location.route_string
            );
            let handler: Arc<dyn UsbInterruptInHandler> = existing.device.clone();
            return Ok(handler);
        }

        let touchpad = Arc::new(ChromeEcUsbTouchpad::new());
        let event_device = touchpad.event_device.clone();
        let name = event_device.get_name().into();
        let registered_device: Arc<dyn Device> = event_device;
        DeviceManager::get_manager().register_device_with_name(name, registered_device);
        registered.push(RegisteredTouchpad {
            location,
            device: touchpad.clone(),
        });
        early_println!(
            "[chrome-ec-usb-touchpad] registered {} at host={} root-port={} route={:#x}",
            touchpad.event_device.get_name(),
            location.host_id,
            location.root_port_id,
            location.route_string
        );
        let handler: Arc<dyn UsbInterruptInHandler> = touchpad;
        Ok(handler)
    }
}

fn contact_from_report(report: &[u8]) -> Option<(u8, u16, u16)> {
    if report.len() < REPORT_SIZE || report[0] != REPORT_ID {
        return None;
    }

    for index in 0..FINGER_COUNT {
        let offset = FINGERS_OFFSET + index * FINGER_SIZE;
        let finger = u64::from_le_bytes(report.get(offset..offset + FINGER_SIZE)?.try_into().ok()?);
        let confidence = finger & 1 != 0;
        let tip = finger & (1 << 1) != 0;
        let in_range = finger & (1 << 2) != 0;
        if confidence && tip && in_range {
            let id = ((finger >> 3) & 0x0f) as u8;
            let x = ((finger >> 40) & 0x0fff) as u16;
            let y = ((finger >> 52) & 0x0fff) as u16;
            return Some((id, x, y));
        }
    }
    None
}

fn register_driver() {
    DeviceManager::get_manager()
        .register_usb_interrupt_in_driver(Arc::new(ChromeEcUsbTouchpadDriver));
}

scarlet::driver_initcall!(register_driver);

static REGISTERED_TOUCHPADS: IrqSpinLock<Vec<RegisteredTouchpad>> = IrqSpinLock::new(Vec::new());

#[used]
static SCARLET_DRIVER_CHROME_EC_USB_TOUCHPAD_ANCHOR: fn() = force_link;

/// Keep this external driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chrome_ec_contact_layout() {
        let mut report = [0u8; REPORT_SIZE];
        report[0] = REPORT_ID;
        report[COUNT_BUTTON_OFFSET] = 1;
        let finger = 1u64
            | (1 << 1)
            | (1 << 2)
            | (7 << 3)
            | (123 << 7)
            | (0x456 << 16)
            | (0x789 << 28)
            | (0xabc << 40)
            | (0xdef << 52);
        report[FINGERS_OFFSET..FINGERS_OFFSET + FINGER_SIZE].copy_from_slice(&finger.to_le_bytes());

        assert_eq!(contact_from_report(&report), Some((7, 0xabc, 0xdef)));
    }
}
