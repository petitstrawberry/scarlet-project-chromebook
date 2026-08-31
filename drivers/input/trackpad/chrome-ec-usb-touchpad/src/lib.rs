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
            abs_codes::{
                ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_PRESSURE, ABS_MT_SLOT,
                ABS_MT_TOUCH_MAJOR, ABS_MT_TRACKING_ID,
            },
            event_device::{
                EventDevice, INPUT_CAP_ABS, INPUT_CAP_INTERNAL, INPUT_CAP_KEY, INPUT_CAP_MT,
                INPUT_CAP_REL, InputDeviceKind, InputDeviceMetadata,
            },
            event_types::{EV_ABS, EV_KEY, EV_REL, EV_SYN},
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

// Linux input-event ABI values. The EC report supplies five contacts with a
// stable 4-bit contact identifier, 12-bit coordinates, and 9-bit pressure.
const BTN_TOUCH: u16 = 0x14a;
const MT_TRACKING_ID_RELEASE: i32 = -1;
const COORDINATE_MAX: i32 = 0x0fff;
const PRESSURE_MAX: i32 = 0x01ff;
const TOUCH_MAJOR_MAX: i32 = 0x0fff;

type RawEvent = (u16, u16, i32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Contact {
    id: u8,
    active: bool,
    pressure: u16,
    width: u16,
    height: u16,
    x: u16,
    y: u16,
}

struct TouchReport {
    contacts: Vec<Contact>,
    button_pressed: bool,
}

#[derive(Default)]
struct PointerState {
    last_contact: Option<(u8, u16, u16)>,
    remainder_x: i32,
    remainder_y: i32,
    button_pressed: bool,
    slots: [Option<u8>; FINGER_COUNT],
}

struct ChromeEcUsbTouchpad {
    event_device: Arc<EventDevice>,
    state: IrqSpinLock<PointerState>,
}

impl ChromeEcUsbTouchpad {
    fn new() -> Result<Self, &'static str> {
        let metadata = InputDeviceMetadata::new(
            InputDeviceKind::Touchpad,
            INPUT_CAP_KEY | INPUT_CAP_REL | INPUT_CAP_ABS | INPUT_CAP_MT | INPUT_CAP_INTERNAL,
        )
        .with_multitouch_slots(FINGER_COUNT)?
        .with_absolute_axis(ABS_MT_SLOT, 0, (FINGER_COUNT - 1) as i32)?
        .with_absolute_axis(ABS_MT_POSITION_X, 0, COORDINATE_MAX)?
        .with_absolute_axis(ABS_MT_POSITION_Y, 0, COORDINATE_MAX)?
        .with_absolute_axis(ABS_MT_TRACKING_ID, 0, 0x0f)?
        .with_absolute_axis(ABS_MT_PRESSURE, 0, PRESSURE_MAX)?
        .with_absolute_axis(ABS_MT_TOUCH_MAJOR, 0, TOUCH_MAJOR_MAX)?;
        Ok(Self {
            event_device: Arc::new(EventDevice::new_with_metadata("touchpad", metadata)),
            state: IrqSpinLock::new(PointerState::default()),
        })
    }

    fn process_report(&self, report: &[u8]) -> Result<(), &'static str> {
        let decoded = decode_touch_report(report)?;
        let events = {
            let mut state = self.state.lock();
            events_for_report(&mut state, &decoded)?
        };
        for (type_, code, value) in events {
            self.event_device.push_event(type_, code, value);
        }
        Ok(())
    }

    fn reset_on_disconnect(&self) {
        let events = {
            let mut state = self.state.lock();
            events_for_disconnect(&mut state)
        };
        for (type_, code, value) in events {
            self.event_device.push_event(type_, code, value);
        }
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

        let touchpad = Arc::new(ChromeEcUsbTouchpad::new()?);
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

fn decode_touch_report(report: &[u8]) -> Result<TouchReport, &'static str> {
    if !has_touchpad_report_header(report) {
        return Err("chrome-ec-usb-touchpad: malformed input report");
    }

    let count = usize::from(report[COUNT_BUTTON_OFFSET] & 0x7f);
    if count > FINGER_COUNT {
        return Err("chrome-ec-usb-touchpad: invalid contact count");
    }

    let mut contacts = Vec::with_capacity(count);
    let mut ids_seen = [false; 16];
    for index in 0..count {
        let offset = FINGERS_OFFSET + index * FINGER_SIZE;
        let bytes = report
            .get(offset..offset + FINGER_SIZE)
            .ok_or("chrome-ec-usb-touchpad: truncated contact")?;
        let finger = u64::from_le_bytes(
            bytes
                .try_into()
                .map_err(|_| "chrome-ec-usb-touchpad: truncated contact")?,
        );
        let confidence = finger & 1 != 0;
        let tip = finger & (1 << 1) != 0;
        let in_range = finger & (1 << 2) != 0;
        let id = ((finger >> 3) & 0x0f) as u8;
        if ids_seen[usize::from(id)] {
            return Err("chrome-ec-usb-touchpad: duplicate contact identifier");
        }
        ids_seen[usize::from(id)] = true;
        contacts.push(Contact {
            id,
            active: confidence && tip && in_range,
            pressure: ((finger >> 7) & 0x01ff) as u16,
            width: ((finger >> 16) & 0x0fff) as u16,
            height: ((finger >> 28) & 0x0fff) as u16,
            x: ((finger >> 40) & 0x0fff) as u16,
            y: ((finger >> 52) & 0x0fff) as u16,
        });
    }
    Ok(TouchReport {
        contacts,
        button_pressed: report[COUNT_BUTTON_OFFSET] & 0x80 != 0,
    })
}

fn events_for_report(
    state: &mut PointerState,
    report: &TouchReport,
) -> Result<Vec<RawEvent>, &'static str> {
    let mut events = Vec::new();
    let was_touching = state.slots.iter().any(Option::is_some);

    // Releases run first so a frame that replaces all five contacts can reuse
    // a slot immediately. Contact IDs, rather than report array positions, are
    // the identity carried across frames.
    for slot in 0..FINGER_COUNT {
        if let Some(id) = state.slots[slot]
            && !report
                .contacts
                .iter()
                .any(|contact| contact.active && contact.id == id)
        {
            events.push((EV_ABS, ABS_MT_SLOT, slot as i32));
            events.push((EV_ABS, ABS_MT_TRACKING_ID, MT_TRACKING_ID_RELEASE));
            state.slots[slot] = None;
        }
    }

    for contact in report.contacts.iter().filter(|contact| contact.active) {
        let existing_slot = state.slots.iter().position(|id| *id == Some(contact.id));
        let (slot, is_new) = match existing_slot {
            Some(slot) => (slot, false),
            None => (
                state
                    .slots
                    .iter()
                    .position(Option::is_none)
                    .ok_or("chrome-ec-usb-touchpad: no free multitouch slot")?,
                true,
            ),
        };
        events.push((EV_ABS, ABS_MT_SLOT, slot as i32));
        if is_new {
            state.slots[slot] = Some(contact.id);
            events.push((EV_ABS, ABS_MT_TRACKING_ID, i32::from(contact.id)));
        }
        events.push((EV_ABS, ABS_MT_POSITION_X, i32::from(contact.x)));
        events.push((EV_ABS, ABS_MT_POSITION_Y, i32::from(contact.y)));
        events.push((EV_ABS, ABS_MT_PRESSURE, i32::from(contact.pressure)));
        events.push((
            EV_ABS,
            ABS_MT_TOUCH_MAJOR,
            i32::from(contact.width.max(contact.height)),
        ));
    }

    let is_touching = state.slots.iter().any(Option::is_some);
    if is_touching != was_touching {
        events.push((
            EV_KEY,
            BTN_TOUCH,
            if is_touching { KEY_PRESS } else { KEY_RELEASE },
        ));
    }
    if report.button_pressed != state.button_pressed {
        state.button_pressed = report.button_pressed;
        events.push((
            EV_KEY,
            BTN_LEFT,
            if report.button_pressed {
                KEY_PRESS
            } else {
                KEY_RELEASE
            },
        ));
    }

    let contact = report
        .contacts
        .iter()
        .find(|contact| contact.active)
        .map(|contact| (contact.id, contact.x, contact.y));
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
    if dx != 0 {
        events.push((EV_REL, REL_X, dx));
    }
    if dy != 0 {
        events.push((EV_REL, REL_Y, dy));
    }
    events.push((EV_SYN, SYN_REPORT, 0));
    Ok(events)
}

fn events_for_disconnect(state: &mut PointerState) -> Vec<RawEvent> {
    let mut events = Vec::new();
    for (slot, contact) in state.slots.iter().enumerate() {
        if contact.is_some() {
            events.push((EV_ABS, ABS_MT_SLOT, slot as i32));
            events.push((EV_ABS, ABS_MT_TRACKING_ID, MT_TRACKING_ID_RELEASE));
        }
    }
    if state.slots.iter().any(Option::is_some) {
        events.push((EV_KEY, BTN_TOUCH, KEY_RELEASE));
    }
    if state.button_pressed {
        events.push((EV_KEY, BTN_LEFT, KEY_RELEASE));
    }
    *state = PointerState::default();
    events.push((EV_SYN, SYN_REPORT, 0));
    events
}

fn has_touchpad_report_header(report: &[u8]) -> bool {
    report.len() >= REPORT_SIZE && report.first() == Some(&REPORT_ID)
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
    use alloc::vec;

    fn contact_word(id: u8, active: bool, x: u16, y: u16, pressure: u16) -> u64 {
        u64::from(active)
            | (u64::from(active) << 1)
            | (u64::from(active) << 2)
            | (u64::from(id) << 3)
            | (u64::from(pressure) << 7)
            | (0x45 << 16)
            | (0x32 << 28)
            | (u64::from(x) << 40)
            | (u64::from(y) << 52)
    }

    fn report(contacts: &[(u8, bool, u16, u16, u16)], button: bool) -> [u8; REPORT_SIZE] {
        let mut report = [0u8; REPORT_SIZE];
        report[0] = REPORT_ID;
        report[COUNT_BUTTON_OFFSET] = contacts.len() as u8 | if button { 0x80 } else { 0 };
        for (index, &(id, active, x, y, pressure)) in contacts.iter().enumerate() {
            let offset = FINGERS_OFFSET + index * FINGER_SIZE;
            report[offset..offset + FINGER_SIZE]
                .copy_from_slice(&contact_word(id, active, x, y, pressure).to_le_bytes());
        }
        report
    }

    fn frame_events(state: &mut PointerState, bytes: &[u8]) -> Vec<RawEvent> {
        let decoded = decode_touch_report(bytes).expect("test report should decode");
        events_for_report(state, &decoded).expect("test frame should have enough slots")
    }

    #[test]
    fn reports_two_fingers_with_stable_hardware_ids() {
        let mut state = PointerState::default();
        let events = frame_events(
            &mut state,
            &report(
                &[(7, true, 0xabc, 0xdef, 123), (2, true, 400, 500, 81)],
                true,
            ),
        );

        assert_eq!(state.slots, [Some(7), Some(2), None, None, None]);
        assert!(events.contains(&(EV_ABS, ABS_MT_SLOT, 0)));
        assert!(events.contains(&(EV_ABS, ABS_MT_TRACKING_ID, 7)));
        assert!(events.contains(&(EV_ABS, ABS_MT_POSITION_X, 0xabc)));
        assert!(events.contains(&(EV_ABS, ABS_MT_POSITION_Y, 0xdef)));
        assert!(events.contains(&(EV_ABS, ABS_MT_PRESSURE, 123)));
        assert!(events.contains(&(EV_ABS, ABS_MT_SLOT, 1)));
        assert!(events.contains(&(EV_ABS, ABS_MT_TRACKING_ID, 2)));
        assert!(events.contains(&(EV_KEY, BTN_TOUCH, KEY_PRESS)));
        assert!(events.contains(&(EV_KEY, BTN_LEFT, KEY_PRESS)));
        assert_eq!(events.last(), Some(&(EV_SYN, SYN_REPORT, 0)));
        assert_eq!(
            events
                .iter()
                .filter(|event| **event == (EV_SYN, SYN_REPORT, 0))
                .count(),
            1
        );
    }

    #[test]
    fn motion_reuses_slots_and_keeps_legacy_relative_motion() {
        let mut state = PointerState::default();
        frame_events(
            &mut state,
            &report(&[(7, true, 100, 200, 40), (2, true, 300, 400, 50)], false),
        );
        let events = frame_events(
            &mut state,
            &report(&[(7, true, 106, 196, 41), (2, true, 303, 405, 52)], false),
        );

        assert!(!events.iter().any(|event| event.1 == ABS_MT_TRACKING_ID));
        assert!(events.contains(&(EV_ABS, ABS_MT_SLOT, 0)));
        assert!(events.contains(&(EV_ABS, ABS_MT_POSITION_X, 106)));
        assert!(events.contains(&(EV_REL, REL_X, 3)));
        assert!(events.contains(&(EV_REL, REL_Y, -2)));
        assert_eq!(state.slots, [Some(7), Some(2), None, None, None]);
        assert_eq!(events.last(), Some(&(EV_SYN, SYN_REPORT, 0)));
    }

    #[test]
    fn lift_releases_only_the_matching_contact_slot() {
        let mut state = PointerState::default();
        frame_events(
            &mut state,
            &report(&[(7, true, 100, 200, 40), (2, true, 300, 400, 50)], false),
        );
        // The EC compacts active contacts and appends a one-frame lift record.
        let events = frame_events(
            &mut state,
            &report(&[(2, true, 305, 405, 51), (7, false, 0, 0, 0)], false),
        );

        assert_eq!(state.slots, [None, Some(2), None, None, None]);
        assert_eq!(
            &events[..2],
            &[
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, MT_TRACKING_ID_RELEASE),
            ]
        );
        assert!(!events.contains(&(EV_KEY, BTN_TOUCH, KEY_RELEASE)));
        assert!(!events.contains(&(EV_ABS, ABS_MT_TRACKING_ID, 2)));
        assert_eq!(events.last(), Some(&(EV_SYN, SYN_REPORT, 0)));
    }

    #[test]
    fn disconnect_releases_all_slots_touch_and_click() {
        let mut state = PointerState::default();
        frame_events(
            &mut state,
            &report(&[(7, true, 100, 200, 40), (2, true, 300, 400, 50)], true),
        );
        let events = events_for_disconnect(&mut state);

        assert_eq!(state.slots, [None; FINGER_COUNT]);
        assert!(!state.button_pressed);
        assert_eq!(
            events,
            vec![
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, MT_TRACKING_ID_RELEASE),
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, MT_TRACKING_ID_RELEASE),
                (EV_KEY, BTN_TOUCH, KEY_RELEASE),
                (EV_KEY, BTN_LEFT, KEY_RELEASE),
                (EV_SYN, SYN_REPORT, 0),
            ]
        );
    }

    #[test]
    fn rejects_malformed_reports_and_accepts_idle_frame() {
        let idle = report(&[], false);
        let mut wrong_id = idle;
        wrong_id[0] = REPORT_ID + 1;
        let mut invalid_count = idle;
        invalid_count[COUNT_BUTTON_OFFSET] = (FINGER_COUNT + 1) as u8;

        assert!(decode_touch_report(&idle[..REPORT_SIZE - 1]).is_err());
        assert!(decode_touch_report(&wrong_id).is_err());
        assert!(decode_touch_report(&invalid_count).is_err());

        let mut state = PointerState::default();
        assert_eq!(
            frame_events(&mut state, &idle),
            vec![(EV_SYN, SYN_REPORT, 0)]
        );
    }
}
