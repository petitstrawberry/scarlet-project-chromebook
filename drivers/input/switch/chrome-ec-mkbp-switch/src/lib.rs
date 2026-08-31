// SPDX-License-Identifier: GPL-2.0-only

#![no_std]
#![cfg_attr(not(target_os = "none"), allow(dead_code))]

//! Chrome EC MKBP lid and tablet-mode switch support.
//!
//! CoachZ exposes both posture switches through the primary Chrome EC. Scarlet
//! routes the EC GPIO interrupt through its SPI parent. This driver subscribes
//! to decoded MKBP switch events and takes one authoritative instantaneous
//! snapshot at boot to close the subscription race.
//!
//! # Provenance
//!
//! The command, request layout, event number, and switch bits follow ChromiumOS
//! EC `include/ec_commands.h` (`EC_CMD_MKBP_INFO`). Linux-compatible event codes
//! follow `include/uapi/linux/input-event-codes.h`.

extern crate alloc;

use alloc::vec::Vec;

#[cfg(target_os = "none")]
use alloc::sync::Arc;
#[cfg(target_os = "none")]
use scarlet::{
    device::{
        Device,
        input::{
            event_device::{EventDevice, INPUT_CAP_SWITCH, InputDeviceKind, InputDeviceMetadata},
            event_types::{EV_SW, EV_SYN},
            switch_codes::{SW_LID, SW_TABLET_MODE},
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
const EC_MKBP_INFO_CURRENT: u8 = 2;
const EC_MKBP_EVENT_SWITCH: u8 = 4;

const EC_MKBP_LID_OPEN: u32 = 1 << 0;
const EC_MKBP_TABLET_MODE: u32 = 1 << 1;
const REQUIRED_SWITCHES: u32 = EC_MKBP_LID_OPEN | EC_MKBP_TABLET_MODE;

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

fn parse_switch_mask(response: &[u8]) -> Result<u32, QueryError<()>> {
    let bytes: [u8; 4] = response
        .try_into()
        .map_err(|_| QueryError::MalformedResponse)?;
    Ok(u32::from_le_bytes(bytes))
}

fn query_switch_mask<C: EcCommand>(ec: &C, info_type: u8) -> Result<u32, QueryError<C::Error>> {
    let response = ec
        .command(EC_CMD_MKBP_INFO, 0, &[info_type, EC_MKBP_EVENT_SWITCH])
        .map_err(QueryError::Transport)?;
    parse_switch_mask(&response).map_err(|_| QueryError::MalformedResponse)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PostureState {
    lid_closed: bool,
    tablet_mode: bool,
}

impl PostureState {
    fn from_ec_switches(switches: u32) -> Self {
        Self {
            // EC reports positive LID_OPEN; Linux SW_LID is positive when shut.
            lid_closed: switches & EC_MKBP_LID_OPEN == 0,
            tablet_mode: switches & EC_MKBP_TABLET_MODE != 0,
        }
    }
}

fn parse_switch_event(event_type: u8, data: &[u8]) -> Result<Option<PostureState>, QueryError<()>> {
    if event_type != EC_MKBP_EVENT_SWITCH {
        return Ok(None);
    }
    parse_switch_mask(data)
        .map(PostureState::from_ec_switches)
        .map(Some)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StateOutcome {
    Snapshot(PostureState),
    Changed {
        previous: PostureState,
        current: PostureState,
    },
    Unchanged,
}

#[derive(Default)]
struct SwitchTracker {
    current: Option<PostureState>,
}

impl SwitchTracker {
    fn accept(&mut self, next: PostureState) -> StateOutcome {
        match self.current.replace(next) {
            None => StateOutcome::Snapshot(next),
            Some(previous) if previous == next => StateOutcome::Unchanged,
            Some(previous) => StateOutcome::Changed {
                previous,
                current: next,
            },
        }
    }
}

#[cfg(target_os = "none")]
mod runtime {
    use super::*;

    struct ChromeEcMkbpSwitch {
        event: Arc<EventDevice>,
        tracker: IrqSpinLock<SwitchTracker>,
    }

    impl ChromeEcMkbpSwitch {
        fn emit_snapshot(&self, state: PostureState) {
            self.event
                .push_event(EV_SW, SW_LID, i32::from(state.lid_closed));
            self.event
                .push_event(EV_SW, SW_TABLET_MODE, i32::from(state.tablet_mode));
            self.event.push_event(EV_SYN, SYN_REPORT, 0);
        }

        fn emit_change(&self, previous: PostureState, current: PostureState) {
            if previous.lid_closed != current.lid_closed {
                self.event
                    .push_event(EV_SW, SW_LID, i32::from(current.lid_closed));
            }
            if previous.tablet_mode != current.tablet_mode {
                self.event
                    .push_event(EV_SW, SW_TABLET_MODE, i32::from(current.tablet_mode));
            }
            self.event.push_event(EV_SYN, SYN_REPORT, 0);
        }

        fn accept_state(&self, state: PostureState) {
            let outcome = self.tracker.lock().accept(state);
            match outcome {
                StateOutcome::Snapshot(state) => self.emit_snapshot(state),
                StateOutcome::Changed { previous, current } => self.emit_change(previous, current),
                StateOutcome::Unchanged => {}
            }
        }
    }

    impl CrosEcEventListener for ChromeEcMkbpSwitch {
        fn on_cros_ec_event(&self, event_type: u8, data: &[u8]) {
            match parse_switch_event(event_type, data) {
                Ok(Some(state)) => self.accept_state(state),
                Ok(None) => {}
                Err(_) => {
                    early_println!("[chrome-ec-mkbp-switch] discarded malformed switch event")
                }
            }
        }
    }

    static DEVICE: IrqSpinLock<Option<Arc<ChromeEcMkbpSwitch>>> = IrqSpinLock::new(None);

    fn initialize() {
        let Some(ec) = get_primary_cros_ec_spi() else {
            early_println!("[chrome-ec-mkbp-switch] primary Chrome EC unavailable");
            return;
        };
        let supported = match query_switch_mask(ec.as_ref(), EC_MKBP_INFO_SUPPORTED) {
            Ok(supported) if supported & REQUIRED_SWITCHES == REQUIRED_SWITCHES => supported,
            Ok(supported) => {
                early_println!(
                    "[chrome-ec-mkbp-switch] required switches unavailable: supported={:#x}",
                    supported
                );
                return;
            }
            Err(_) => {
                early_println!("[chrome-ec-mkbp-switch] failed to query switch support");
                return;
            }
        };
        let metadata = match InputDeviceMetadata::new(InputDeviceKind::Switch, INPUT_CAP_SWITCH)
            .with_switch(SW_LID)
            .and_then(|metadata| metadata.with_switch(SW_TABLET_MODE))
        {
            Ok(metadata) => metadata,
            Err(error) => {
                early_println!("[chrome-ec-mkbp-switch] invalid input metadata: {}", error);
                return;
            }
        };
        let event = Arc::new(EventDevice::new_with_metadata("switch", metadata));
        let device = Arc::new(ChromeEcMkbpSwitch {
            event: event.clone(),
            tracker: IrqSpinLock::new(SwitchTracker::default()),
        });

        // Hold the listener strongly before subscribing, then re-synchronize
        // through CURRENT. An event racing the snapshot either seeds the
        // tracker first or is filtered against the snapshot as unchanged.
        *DEVICE.lock() = Some(device.clone());
        let listener: Arc<dyn CrosEcEventListener> = device.clone();
        let listener_id = ec.register_event_listener(Arc::downgrade(&listener));
        let initial = match query_switch_mask(ec.as_ref(), EC_MKBP_INFO_CURRENT) {
            Ok(switches) => PostureState::from_ec_switches(switches),
            Err(_) => {
                let _ = ec.unregister_event_listener(listener_id);
                *DEVICE.lock() = None;
                early_println!("[chrome-ec-mkbp-switch] failed to read boot posture");
                return;
            }
        };
        device.accept_state(initial);

        let name = event.get_name().into();
        let registered: Arc<dyn Device> = event;
        DeviceManager::get_manager().register_device_with_name(name, registered);
        early_println!(
            "[chrome-ec-mkbp-switch] registered switch0 supported={:#x} lid_closed={} tablet_mode={} source=EC-MKBP-IRQ",
            supported,
            initial.lid_closed,
            initial.tablet_mode,
        );
    }

    scarlet::late_initcall!(initialize);
}

#[used]
static SCARLET_DRIVER_CHROME_EC_MKBP_SWITCH_ANCHOR: fn() = force_link;

/// Keep the external driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::{collections::VecDeque, vec};
    use core::cell::RefCell;

    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct MockError;

    struct MockEc {
        responses: RefCell<VecDeque<Result<Vec<u8>, MockError>>>,
        calls: RefCell<Vec<(u16, u8, Vec<u8>)>>,
    }

    impl MockEc {
        fn new(responses: impl IntoIterator<Item = Result<Vec<u8>, MockError>>) -> Self {
            Self {
                responses: RefCell::new(responses.into_iter().collect()),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl EcCommand for MockEc {
        type Error = MockError;

        fn command(
            &self,
            command: u16,
            version: u8,
            payload: &[u8],
        ) -> Result<Vec<u8>, Self::Error> {
            self.calls
                .borrow_mut()
                .push((command, version, payload.to_vec()));
            self.responses
                .borrow_mut()
                .pop_front()
                .expect("mock response exhausted")
        }
    }

    #[test]
    fn parses_exact_little_endian_switch_mask() {
        assert_eq!(
            parse_switch_mask(&[0x03, 0x02, 0x01, 0x80]),
            Ok(0x8001_0203)
        );
        assert_eq!(
            parse_switch_mask(&[0x03, 0x02, 0x01]),
            Err(QueryError::MalformedResponse)
        );
        assert_eq!(
            parse_switch_mask(&[0x03, 0x02, 0x01, 0x00, 0xff]),
            Err(QueryError::MalformedResponse)
        );
    }

    #[test]
    fn queries_supported_and_current_with_mkbp_switch_payload() {
        let ec = MockEc::new([Ok(vec![3, 0, 0, 0]), Ok(vec![2, 0, 0, 0])]);
        assert_eq!(
            query_switch_mask(&ec, EC_MKBP_INFO_SUPPORTED),
            Ok(REQUIRED_SWITCHES)
        );
        assert_eq!(query_switch_mask(&ec, EC_MKBP_INFO_CURRENT), Ok(2));
        assert_eq!(
            ec.calls.into_inner(),
            vec![
                (
                    EC_CMD_MKBP_INFO,
                    0,
                    vec![EC_MKBP_INFO_SUPPORTED, EC_MKBP_EVENT_SWITCH]
                ),
                (
                    EC_CMD_MKBP_INFO,
                    0,
                    vec![EC_MKBP_INFO_CURRENT, EC_MKBP_EVENT_SWITCH]
                ),
            ]
        );
    }

    #[test]
    fn inverts_ec_lid_open_but_not_tablet_mode() {
        assert_eq!(
            PostureState::from_ec_switches(EC_MKBP_LID_OPEN),
            PostureState {
                lid_closed: false,
                tablet_mode: false,
            }
        );
        assert_eq!(
            PostureState::from_ec_switches(EC_MKBP_TABLET_MODE),
            PostureState {
                lid_closed: true,
                tablet_mode: true,
            }
        );
    }

    #[test]
    fn unchanged_events_do_not_produce_changes() {
        let posture = PostureState::from_ec_switches(REQUIRED_SWITCHES);
        let mut tracker = SwitchTracker::default();
        assert_eq!(tracker.accept(posture), StateOutcome::Snapshot(posture));
        assert_eq!(tracker.accept(posture), StateOutcome::Unchanged);
    }

    #[test]
    fn switch_events_parse_and_unrelated_events_are_ignored() {
        let initial = PostureState::from_ec_switches(EC_MKBP_LID_OPEN);
        assert_eq!(
            parse_switch_event(EC_MKBP_EVENT_SWITCH, &EC_MKBP_LID_OPEN.to_le_bytes()),
            Ok(Some(initial))
        );
        assert_eq!(parse_switch_event(1, &[0, 0, 0, 0]), Ok(None));
        assert_eq!(
            parse_switch_event(EC_MKBP_EVENT_SWITCH, &[0, 0, 0]),
            Err(QueryError::MalformedResponse)
        );
    }

    #[test]
    fn changed_event_reports_previous_and_current_state() {
        let initial = PostureState::from_ec_switches(EC_MKBP_LID_OPEN);
        let changed = PostureState::from_ec_switches(EC_MKBP_TABLET_MODE);
        let mut tracker = SwitchTracker::default();
        assert_eq!(tracker.accept(initial), StateOutcome::Snapshot(initial));
        assert_eq!(
            tracker.accept(changed),
            StateOutcome::Changed {
                previous: initial,
                current: changed,
            }
        );
    }

    #[test]
    fn query_surfaces_transport_errors_without_parsing() {
        let ec = MockEc::new([Err(MockError)]);
        assert_eq!(
            query_switch_mask(&ec, EC_MKBP_INFO_CURRENT),
            Err(QueryError::Transport(MockError))
        );
    }
}
