// SPDX-License-Identifier: GPL-2.0-only

#![no_std]
#![cfg_attr(not(target_os = "none"), allow(dead_code))]

//! Chrome EC MKBP non-matrix power and volume button support.
//!
//! The Chrome EC sends `EC_MKBP_EVENT_BUTTON` as the complete, authoritative
//! state of its non-matrix button bitmap.  This driver converts the known
//! power and volume bits into state transitions and deliberately never
//! synthesizes repeat events: repeat policy belongs in userspace.
//!
//! # Provenance
//!
//! Event numbers, query layout, and bitmap bits follow ChromiumOS EC
//! `include/ec_commands.h`.  The event processing model follows Linux
//! `drivers/input/keyboard/cros_ec_keyb.c`: supported bits are queried with
//! `EC_CMD_MKBP_INFO`, while button state is driven exclusively by MKBP events
//! because button presses are transient.

extern crate alloc;

use alloc::vec::Vec;

#[cfg(target_os = "none")]
use alloc::sync::Arc;
#[cfg(target_os = "none")]
use scarlet::{
    device::{
        Device,
        input::{
            event_device::{EventDevice, INPUT_CAP_KEY, InputDeviceKind, InputDeviceMetadata},
            event_types::{EV_KEY, EV_SYN},
            key_codes::{KEY_POWER, KEY_VOLUMEDOWN, KEY_VOLUMEUP},
            syn_codes::SYN_REPORT,
        },
        manager::DeviceManager,
    },
    early_println,
    sync::IrqSpinLock,
};
#[cfg(target_os = "none")]
use scarlet_driver_cros_ec_spi::{
    CrosEcError, CrosEcEventListener, CrosEcSpi, get_primary_cros_ec_spi,
};

const EC_CMD_MKBP_INFO: u16 = 0x0061;
const EC_MKBP_INFO_SUPPORTED: u8 = 1;
const EC_MKBP_EVENT_BUTTON: u8 = 3;

const EC_MKBP_POWER_BUTTON: u32 = 1 << 0;
const EC_MKBP_VOL_UP: u32 = 1 << 1;
const EC_MKBP_VOL_DOWN: u32 = 1 << 2;
const KNOWN_BUTTONS: u32 = EC_MKBP_POWER_BUTTON | EC_MKBP_VOL_UP | EC_MKBP_VOL_DOWN;

trait EcCommand {
    type Error;

    fn command(&self, command: u16, version: u8, payload: &[u8]) -> Result<Vec<u8>, Self::Error>;
}

#[cfg(target_os = "none")]
impl EcCommand for CrosEcSpi {
    type Error = CrosEcError;

    fn command(&self, command: u16, version: u8, payload: &[u8]) -> Result<Vec<u8>, Self::Error> {
        CrosEcSpi::command(self, command, version, payload)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum QueryError<E> {
    Transport(E),
    MalformedResponse,
}

/// Decode the EC's exact four-byte little-endian button bitmap.
fn parse_button_mask(response: &[u8]) -> Result<u32, QueryError<()>> {
    let bytes: [u8; 4] = response
        .try_into()
        .map_err(|_| QueryError::MalformedResponse)?;
    Ok(u32::from_le_bytes(bytes))
}

/// Query which non-matrix buttons this EC reports.
fn query_supported_buttons<C: EcCommand>(ec: &C) -> Result<u32, QueryError<C::Error>> {
    let response = ec
        .command(
            EC_CMD_MKBP_INFO,
            0,
            &[EC_MKBP_INFO_SUPPORTED, EC_MKBP_EVENT_BUTTON],
        )
        .map_err(QueryError::Transport)?;
    parse_button_mask(&response).map_err(|_| QueryError::MalformedResponse)
}

/// Discard EC button bits that Scarlet has no stable input-key mapping for.
const fn known_supported_buttons(supported: u32) -> u32 {
    supported & KNOWN_BUTTONS
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Button {
    Power,
    VolumeUp,
    VolumeDown,
}

impl Button {
    const ALL: [Self; 3] = [Self::Power, Self::VolumeUp, Self::VolumeDown];

    const fn bit(self) -> u32 {
        match self {
            Self::Power => EC_MKBP_POWER_BUTTON,
            Self::VolumeUp => EC_MKBP_VOL_UP,
            Self::VolumeDown => EC_MKBP_VOL_DOWN,
        }
    }

    #[cfg(target_os = "none")]
    const fn key_code(self) -> u16 {
        match self {
            Self::Power => KEY_POWER,
            Self::VolumeUp => KEY_VOLUMEUP,
            Self::VolumeDown => KEY_VOLUMEDOWN,
        }
    }
}

fn parse_button_event(event_type: u8, data: &[u8]) -> Result<Option<u32>, QueryError<()>> {
    if event_type != EC_MKBP_EVENT_BUTTON {
        return Ok(None);
    }
    parse_button_mask(data).map(Some)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StateOutcome {
    Initial { current: u32 },
    Changed { previous: u32, current: u32 },
    Unchanged,
}

#[derive(Default)]
struct ButtonTracker {
    current: Option<u32>,
}

impl ButtonTracker {
    /// Accept a complete state snapshot from an EC event.
    fn accept(&mut self, current: u32) -> StateOutcome {
        match self.current.replace(current) {
            None => StateOutcome::Initial { current },
            Some(previous) if previous == current => StateOutcome::Unchanged,
            Some(previous) => StateOutcome::Changed { previous, current },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct KeyTransition {
    button: Button,
    pressed: bool,
}

#[derive(Debug, Eq, PartialEq)]
struct ButtonFrame {
    transitions: Vec<KeyTransition>,
    syn_report: bool,
}

/// Produce one input frame for a state transition.
///
/// The first authoritative state establishes every supported key. Subsequent
/// frames contain only changed keys. `Some` means callers must append exactly
/// one `SYN_REPORT`; `None` means no state changed and no input frame exists.
fn frame_for_outcome(outcome: StateOutcome, supported: u32) -> Option<ButtonFrame> {
    let (previous, current, initial) = match outcome {
        StateOutcome::Initial { current } => (0, current, true),
        StateOutcome::Changed { previous, current } => (previous, current, false),
        StateOutcome::Unchanged => return None,
    };
    let mut transitions = Vec::new();
    for button in Button::ALL {
        let bit = button.bit();
        if supported & bit != 0 && (initial || (previous ^ current) & bit != 0) {
            transitions.push(KeyTransition {
                button,
                pressed: current & bit != 0,
            });
        }
    }
    Some(ButtonFrame {
        transitions,
        syn_report: true,
    })
}

#[cfg(target_os = "none")]
mod runtime {
    use super::*;

    struct ChromeEcMkbpButtons {
        event: Arc<EventDevice>,
        supported: u32,
        tracker: IrqSpinLock<ButtonTracker>,
    }

    impl ChromeEcMkbpButtons {
        fn accept_state(&self, raw_state: u32) {
            let state = raw_state & self.supported;
            let outcome = self.tracker.lock().accept(state);
            let Some(frame) = frame_for_outcome(outcome, self.supported) else {
                return;
            };
            for transition in frame.transitions {
                self.event.push_event(
                    EV_KEY,
                    transition.button.key_code(),
                    i32::from(transition.pressed),
                );
            }
            if frame.syn_report {
                self.event.push_event(EV_SYN, SYN_REPORT, 0);
            }
        }
    }

    impl CrosEcEventListener for ChromeEcMkbpButtons {
        fn on_cros_ec_event(&self, event_type: u8, data: &[u8]) -> bool {
            match parse_button_event(event_type, data) {
                Ok(Some(state)) => self.accept_state(state),
                Ok(None) => {}
                Err(_) => {
                    early_println!("[chrome-ec-mkbp-button] discarded malformed button event")
                }
            }
            true
        }
    }

    static DEVICE: IrqSpinLock<Option<Arc<ChromeEcMkbpButtons>>> = IrqSpinLock::new(None);

    fn initialize() {
        let Some(ec) = get_primary_cros_ec_spi() else {
            early_println!("[chrome-ec-mkbp-button] primary Chrome EC unavailable");
            return;
        };
        let supported = match query_supported_buttons(ec.as_ref()) {
            Ok(supported) => known_supported_buttons(supported),
            Err(_) => {
                early_println!("[chrome-ec-mkbp-button] failed to query button support");
                return;
            }
        };
        if supported == 0 {
            early_println!("[chrome-ec-mkbp-button] no known EC buttons supported");
            return;
        }

        let metadata = InputDeviceMetadata::new(InputDeviceKind::Keyboard, INPUT_CAP_KEY);
        let event = Arc::new(EventDevice::new_with_metadata("keyboard", metadata));
        let device = Arc::new(ChromeEcMkbpButtons {
            event: event.clone(),
            supported,
            tracker: IrqSpinLock::new(ButtonTracker::default()),
        });

        // Do not issue MKBP_INFO_CURRENT here: unlike posture switches,
        // button state is transient. The next authoritative EC event seeds
        // the tracker and is emitted as one complete input frame.
        *DEVICE.lock() = Some(device.clone());
        let listener: Arc<dyn CrosEcEventListener> = device;
        ec.register_event_listener(Arc::downgrade(&listener));

        let name = event.get_name().into();
        let registered: Arc<dyn Device> = event;
        DeviceManager::get_manager().register_device_with_name(name, registered);
        early_println!(
            "[chrome-ec-mkbp-button] registered keyboard buttons supported={:#x} source=EC-MKBP-IRQ",
            supported,
        );
    }

    scarlet::late_initcall!(initialize);
}

#[used]
static SCARLET_DRIVER_CHROME_EC_MKBP_BUTTON_ANCHOR: fn() = force_link;

/// Keep this external driver linked into generated Scarlet module builds.
#[inline(never)]
pub fn force_link() {}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use core::cell::RefCell;

    #[derive(Default)]
    struct FakeEc {
        requests: RefCell<Vec<(u16, u8, Vec<u8>)>>,
        response: Vec<u8>,
    }

    impl EcCommand for FakeEc {
        type Error = ();

        fn command(
            &self,
            command: u16,
            version: u8,
            payload: &[u8],
        ) -> Result<Vec<u8>, Self::Error> {
            self.requests
                .borrow_mut()
                .push((command, version, payload.into()));
            Ok(self.response.clone())
        }
    }

    #[test]
    fn parses_exact_little_endian_button_payload() {
        assert_eq!(
            parse_button_event(EC_MKBP_EVENT_BUTTON, &[0x05, 0, 0, 0]),
            Ok(Some(EC_MKBP_POWER_BUTTON | EC_MKBP_VOL_DOWN))
        );
        assert_eq!(
            parse_button_event(EC_MKBP_EVENT_BUTTON, &[0; 3]),
            Err(QueryError::MalformedResponse)
        );
        assert_eq!(
            parse_button_event(EC_MKBP_EVENT_BUTTON, &[0; 5]),
            Err(QueryError::MalformedResponse)
        );
    }

    #[test]
    fn supported_query_uses_mkbp_info_v0_button_payload() {
        let ec = FakeEc {
            response: (EC_MKBP_POWER_BUTTON | EC_MKBP_VOL_UP)
                .to_le_bytes()
                .to_vec(),
            ..Default::default()
        };
        assert_eq!(
            query_supported_buttons(&ec),
            Ok(EC_MKBP_POWER_BUTTON | EC_MKBP_VOL_UP)
        );
        assert_eq!(
            ec.requests.into_inner(),
            vec![(
                EC_CMD_MKBP_INFO,
                0,
                vec![EC_MKBP_INFO_SUPPORTED, EC_MKBP_EVENT_BUTTON],
            )]
        );
    }

    #[test]
    fn first_authoritative_event_reports_known_supported_state_once() {
        let supported = known_supported_buttons(u32::MAX);
        let mut tracker = ButtonTracker::default();
        let frame = frame_for_outcome(
            tracker.accept(EC_MKBP_POWER_BUTTON | EC_MKBP_VOL_DOWN),
            supported,
        );
        assert_eq!(
            frame,
            Some(ButtonFrame {
                transitions: vec![
                    KeyTransition {
                        button: Button::Power,
                        pressed: true,
                    },
                    KeyTransition {
                        button: Button::VolumeUp,
                        pressed: false,
                    },
                    KeyTransition {
                        button: Button::VolumeDown,
                        pressed: true,
                    },
                ],
                syn_report: true,
            })
        );
    }

    #[test]
    fn later_events_emit_only_pressed_and_released_transitions() {
        let supported = known_supported_buttons(u32::MAX);
        let mut tracker = ButtonTracker::default();
        let _ = tracker.accept(EC_MKBP_POWER_BUTTON | EC_MKBP_VOL_DOWN);
        assert_eq!(
            frame_for_outcome(tracker.accept(EC_MKBP_VOL_UP | EC_MKBP_VOL_DOWN), supported),
            Some(ButtonFrame {
                transitions: vec![
                    KeyTransition {
                        button: Button::Power,
                        pressed: false,
                    },
                    KeyTransition {
                        button: Button::VolumeUp,
                        pressed: true,
                    },
                ],
                syn_report: true,
            })
        );
        assert_eq!(
            frame_for_outcome(tracker.accept(EC_MKBP_VOL_UP | EC_MKBP_VOL_DOWN), supported),
            None
        );
    }

    #[test]
    fn ignores_unrelated_events_and_unknown_supported_bits() {
        assert_eq!(parse_button_event(4, &[0; 4]), Ok(None));
        assert_eq!(known_supported_buttons(1 << 31), 0);
        assert_eq!(
            known_supported_buttons(EC_MKBP_VOL_UP | (1 << 31)),
            EC_MKBP_VOL_UP
        );
    }
}
