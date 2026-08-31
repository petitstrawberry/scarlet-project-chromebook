extern crate alloc;

use alloc::{vec, vec::Vec};

const MAX_REPORT_FIELD_BITS: usize = 32;
const MAX_INPUT_REPORT_BITS: usize = 4096;
const MAX_INPUT_REPORT_FIELDS: usize = 4096;

pub(crate) fn encode_simple_command(command_register: u16, opcode: u8) -> [u8; 4] {
    let [low, high] = command_register.to_le_bytes();
    [low, high, 0, opcode]
}

pub(crate) fn decode_input_length(buffer: &[u8]) -> Option<usize> {
    Some(usize::from(u16::from_le_bytes([
        *buffer.first()?,
        *buffer.get(1)?,
    ])))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HidField {
    pub(crate) report_id: u8,
    pub(crate) bit_offset: usize,
    pub(crate) bit_size: usize,
    pub(crate) logical_minimum: i32,
    pub(crate) logical_maximum: i32,
}

#[derive(Debug)]
pub(crate) struct HidTouchLayout {
    pub(crate) report_id: u8,
    pub(crate) tips: Vec<HidField>,
    pub(crate) contact_ids: Vec<HidField>,
    pub(crate) x: Vec<HidField>,
    pub(crate) y: Vec<HidField>,
    pub(crate) pressure: Vec<HidField>,
    pub(crate) touch_major: Vec<HidField>,
    pub(crate) contact_count: Option<HidField>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HidContact {
    pub(crate) descriptor_slot: usize,
    pub(crate) hardware_id: Option<i32>,
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) pressure: Option<i32>,
    pub(crate) touch_major: Option<i32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContactIdentity {
    Hardware(i32),
    Descriptor(usize),
}

#[derive(Clone, Copy, Debug)]
struct TouchSlot {
    identity: ContactIdentity,
    tracking_id: i32,
    seen: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SlotUpdate {
    pub(crate) slot: usize,
    pub(crate) tracking_id: i32,
    pub(crate) began: bool,
    pub(crate) contact: HidContact,
}

pub(crate) struct TouchFrame {
    pub(crate) updates: Vec<SlotUpdate>,
    pub(crate) releases: Vec<usize>,
    pub(crate) primary: Option<HidContact>,
    pub(crate) any_contact: bool,
}

/// Maps controller contacts to fixed Linux type-B slots. Contact Identifier is
/// authoritative when present; descriptor positions are a stable fallback for
/// hardware which omits it.
pub(crate) struct SlotTracker {
    slots: Vec<Option<TouchSlot>>,
    next_tracking_id: i32,
}

impl SlotTracker {
    pub(crate) fn new(slot_count: usize) -> Self {
        Self {
            slots: vec![None; slot_count],
            next_tracking_id: 1,
        }
    }

    fn contact_identity(contact: HidContact) -> ContactIdentity {
        contact
            .hardware_id
            .map(ContactIdentity::Hardware)
            .unwrap_or(ContactIdentity::Descriptor(contact.descriptor_slot))
    }

    fn allocate_tracking_id(&mut self) -> i32 {
        let tracking_id = self.next_tracking_id;
        self.next_tracking_id = if tracking_id == i32::MAX {
            1
        } else {
            tracking_id + 1
        };
        tracking_id
    }

    pub(crate) fn apply(&mut self, contacts: &[HidContact]) -> TouchFrame {
        for slot in self.slots.iter_mut().flatten() {
            slot.seen = false;
        }

        let mut updates = Vec::new();
        for contact in contacts.iter().copied() {
            let mut identity = Self::contact_identity(contact);
            let existing = self
                .slots
                .iter()
                .position(|slot| slot.is_some_and(|slot| slot.identity == identity && !slot.seen));
            // A duplicate hardware Contact Identifier within a single report
            // is malformed, but treating the second record as its descriptor
            // slot prevents two records from updating one type-B slot.
            if existing.is_none()
                && self
                    .slots
                    .iter()
                    .any(|slot| slot.is_some_and(|slot| slot.identity == identity && slot.seen))
            {
                identity = ContactIdentity::Descriptor(contact.descriptor_slot);
            }
            let slot_index = existing.or_else(|| self.slots.iter().position(Option::is_none));
            let Some(slot_index) = slot_index else {
                continue;
            };
            let began = self.slots[slot_index].is_none();
            if began {
                let tracking_id = self.allocate_tracking_id();
                self.slots[slot_index] = Some(TouchSlot {
                    identity,
                    tracking_id,
                    seen: true,
                });
            } else if let Some(slot) = self.slots[slot_index].as_mut() {
                slot.seen = true;
            }
            let tracking_id = self.slots[slot_index]
                .as_ref()
                .expect("type-B slot was assigned")
                .tracking_id;
            updates.push(SlotUpdate {
                slot: slot_index,
                tracking_id,
                began,
                contact,
            });
        }

        let mut releases = Vec::new();
        for (slot_index, slot) in self.slots.iter_mut().enumerate() {
            if slot.is_some_and(|slot| !slot.seen) {
                *slot = None;
                releases.push(slot_index);
            }
        }
        TouchFrame {
            updates,
            releases,
            primary: contacts.first().copied(),
            any_contact: !contacts.is_empty(),
        }
    }
}

impl HidTouchLayout {
    pub(crate) fn contact_capacity(&self) -> usize {
        self.tips.len().min(self.x.len()).min(self.y.len())
    }
}

#[derive(Clone, Copy)]
struct HidGlobals {
    usage_page: u32,
    logical_minimum: i32,
    logical_maximum_raw: u32,
    logical_maximum_size: usize,
    report_size: usize,
    report_count: usize,
    report_id: u8,
}

impl HidGlobals {
    const fn new() -> Self {
        Self {
            usage_page: 0,
            logical_minimum: 0,
            logical_maximum_raw: 0,
            logical_maximum_size: 0,
            report_size: 0,
            report_count: 0,
            report_id: 0,
        }
    }
}

impl HidGlobals {
    fn logical_maximum(self) -> Result<i32, &'static str> {
        if self.logical_minimum < 0 {
            Ok(sign_extend_item(
                self.logical_maximum_raw,
                self.logical_maximum_size,
            ))
        } else {
            i32::try_from(self.logical_maximum_raw)
                .map_err(|_| "goodix: HID logical maximum exceeds i32")
        }
    }
}

fn little_endian_item(data: &[u8]) -> u32 {
    data.iter().enumerate().fold(0, |value, (index, byte)| {
        value | (u32::from(*byte) << (index * 8))
    })
}

fn sign_extend_item(value: u32, size: usize) -> i32 {
    match size {
        0 => 0,
        1 => i32::from(value as u8 as i8),
        2 => i32::from(value as u16 as i16),
        4 => value as i32,
        _ => unreachable!("HID short items contain at most four bytes"),
    }
}

fn sign_extend_bits(value: u32, bits: usize) -> i32 {
    debug_assert!((1..=32).contains(&bits));
    let shift = 32 - bits;
    ((value << shift) as i32) >> shift
}

pub(crate) fn parse_touch_layout(descriptor: &[u8]) -> Result<HidTouchLayout, &'static str> {
    let mut globals = HidGlobals::new();
    let mut global_stack = Vec::new();
    let mut usages = Vec::new();
    let mut usage_minimum = None;
    let mut usage_maximum = None;
    let mut input_offsets = [0usize; 256];
    let mut tips = Vec::new();
    let mut contact_ids = Vec::new();
    let mut x = Vec::new();
    let mut y = Vec::new();
    let mut pressure = Vec::new();
    let mut touch_major = Vec::new();
    let mut contact_counts = Vec::new();
    let mut touch_collections = Vec::new();
    let mut cursor = 0usize;

    while cursor < descriptor.len() {
        let prefix = descriptor[cursor];
        cursor += 1;
        if prefix == 0xfe {
            let length = usize::from(
                *descriptor
                    .get(cursor)
                    .ok_or("goodix: truncated HID long item")?,
            );
            cursor = cursor
                .checked_add(2 + length)
                .filter(|end| *end <= descriptor.len())
                .ok_or("goodix: malformed HID long item")?;
            continue;
        }
        let size = match prefix & 0x03 {
            3 => 4,
            value => usize::from(value),
        };
        let data = descriptor
            .get(cursor..cursor + size)
            .ok_or("goodix: truncated HID item")?;
        cursor += size;
        let value = little_endian_item(data);
        let item_type = (prefix >> 2) & 0x03;
        let tag = prefix >> 4;

        match (item_type, tag) {
            (1, 0) => globals.usage_page = value,
            (1, 1) => globals.logical_minimum = sign_extend_item(value, size),
            (1, 2) => {
                globals.logical_maximum_raw = value;
                globals.logical_maximum_size = size;
            }
            (1, 7) => {
                globals.report_size =
                    usize::try_from(value).map_err(|_| "goodix: invalid HID report size")?;
            }
            (1, 8) if value <= 0xff => {
                globals.report_id = value as u8;
                input_offsets[globals.report_id as usize] =
                    input_offsets[globals.report_id as usize].max(8);
            }
            (1, 9) => {
                globals.report_count =
                    usize::try_from(value).map_err(|_| "goodix: invalid HID report count")?;
            }
            (1, 10) => global_stack.push(globals),
            (1, 11) => {
                globals = global_stack
                    .pop()
                    .ok_or("goodix: unbalanced HID global pop")?
            }
            (2, 0) => usages.push(value),
            (2, 1) => usage_minimum = Some(value),
            (2, 2) => usage_maximum = Some(value),
            (0, 10) => {
                let parent_is_touch = touch_collections.last().copied().unwrap_or(false);
                let collection_is_touch = parent_is_touch
                    || (globals.usage_page == 0x0d && usages.first().copied() == Some(0x04));
                touch_collections.push(collection_is_touch);
                usages.clear();
                usage_minimum = None;
                usage_maximum = None;
            }
            (0, 12) => {
                touch_collections
                    .pop()
                    .ok_or("goodix: unbalanced HID collection")?;
                usages.clear();
                usage_minimum = None;
                usage_maximum = None;
            }
            (0, 8) => {
                let report_id = globals.report_id;
                let offset = &mut input_offsets[report_id as usize];
                if globals.report_count > MAX_INPUT_REPORT_FIELDS {
                    return Err("goodix: HID input field count exceeds limit");
                }
                let input_bits = globals
                    .report_size
                    .checked_mul(globals.report_count)
                    .and_then(|bits| offset.checked_add(bits))
                    .filter(|end| *end <= MAX_INPUT_REPORT_BITS)
                    .ok_or("goodix: HID input report exceeds limit")?;
                let is_touch_data = touch_collections.last().copied().unwrap_or(false);
                let is_variable_data = is_touch_data && value & 0x03 == 0x02;
                for index in 0..globals.report_count {
                    let usage = usages.get(index).copied().or_else(|| {
                        let minimum = usage_minimum?;
                        let maximum = usage_maximum?;
                        minimum
                            .checked_add(index as u32)
                            .filter(|usage| *usage <= maximum)
                    });
                    if is_variable_data {
                        if let Some(usage) = usage {
                            let recognized = matches!(
                                (globals.usage_page, usage),
                                (0x0d, 0x42)
                                    | (0x0d, 0x51)
                                    | (0x01, 0x30)
                                    | (0x01, 0x31)
                                    | (0x0d, 0x30)
                                    | (0x0d, 0x48)
                                    | (0x0d, 0x54)
                            );
                            if recognized {
                                if !(1..=MAX_REPORT_FIELD_BITS).contains(&globals.report_size) {
                                    return Err("goodix: HID touch field size exceeds limit");
                                }
                                let field = HidField {
                                    report_id,
                                    bit_offset: *offset,
                                    bit_size: globals.report_size,
                                    logical_minimum: globals.logical_minimum,
                                    logical_maximum: globals.logical_maximum()?,
                                };
                                match (globals.usage_page, usage) {
                                    (0x0d, 0x42) => tips.push(field),
                                    (0x0d, 0x51) => contact_ids.push(field),
                                    (0x01, 0x30) => x.push(field),
                                    (0x01, 0x31) => y.push(field),
                                    (0x0d, 0x30) => pressure.push(field),
                                    (0x0d, 0x48) => touch_major.push(field),
                                    (0x0d, 0x54) => contact_counts.push(field),
                                    _ => unreachable!(),
                                }
                            }
                        }
                    }
                    *offset = offset
                        .checked_add(globals.report_size)
                        .ok_or("goodix: HID input report is too large")?;
                }
                debug_assert_eq!(*offset, input_bits);
                usages.clear();
                usage_minimum = None;
                usage_maximum = None;
            }
            (0, _) => {
                usages.clear();
                usage_minimum = None;
                usage_maximum = None;
            }
            _ => {}
        }
    }

    let report_id = x
        .iter()
        .map(|field| field.report_id)
        .find(|id| y.iter().any(|field| field.report_id == *id))
        .ok_or("goodix: HID descriptor has no touchscreen X/Y report")?;
    tips.retain(|field| field.report_id == report_id);
    contact_ids.retain(|field| field.report_id == report_id);
    x.retain(|field| field.report_id == report_id);
    y.retain(|field| field.report_id == report_id);
    pressure.retain(|field| field.report_id == report_id);
    touch_major.retain(|field| field.report_id == report_id);
    let contact_count = contact_counts
        .into_iter()
        .find(|field| field.report_id == report_id);
    if tips.is_empty() || x.is_empty() || y.is_empty() {
        return Err("goodix: incomplete HID touchscreen report");
    }
    Ok(HidTouchLayout {
        report_id,
        tips,
        contact_ids,
        x,
        y,
        pressure,
        touch_major,
        contact_count,
    })
}

/// Decode the active contact records in one HID input report.  A descriptor is
/// allowed to omit Contact Identifier; callers can then use `descriptor_slot`
/// as a deterministic identity for type-B slot tracking.
pub(crate) fn decode_contacts(
    report: &[u8],
    layout: &HidTouchLayout,
) -> Result<Vec<HidContact>, &'static str> {
    let capacity = layout.contact_capacity();
    let reported_count = match layout.contact_count {
        Some(field) => usize::try_from(
            extract_bits(report, field).ok_or("goodix: malformed HID contact count")?,
        )
        .map_err(|_| "goodix: HID contact count exceeds usize")?,
        None => capacity,
    };
    let mut contacts = Vec::new();
    for descriptor_slot in 0..reported_count.min(capacity) {
        let tip = extract_bits(report, layout.tips[descriptor_slot])
            .ok_or("goodix: malformed HID tip field")?;
        if tip == 0 {
            continue;
        }
        let x = extract_logical_value(report, layout.x[descriptor_slot])
            .ok_or("goodix: malformed HID X field")?;
        let y = extract_logical_value(report, layout.y[descriptor_slot])
            .ok_or("goodix: malformed HID Y field")?;
        let hardware_id = match layout.contact_ids.get(descriptor_slot).copied() {
            Some(field) => Some(
                extract_logical_value(report, field)
                    .ok_or("goodix: malformed HID contact identifier")?,
            ),
            None => None,
        };
        let pressure = match layout.pressure.get(descriptor_slot).copied() {
            Some(field) => Some(
                extract_logical_value(report, field)
                    .ok_or("goodix: malformed HID pressure field")?,
            ),
            None => None,
        };
        let touch_major = match layout.touch_major.get(descriptor_slot).copied() {
            Some(field) => Some(
                extract_logical_value(report, field)
                    .ok_or("goodix: malformed HID touch-major field")?,
            ),
            None => None,
        };
        contacts.push(HidContact {
            descriptor_slot,
            hardware_id,
            x,
            y,
            pressure,
            touch_major,
        });
    }
    Ok(contacts)
}

pub(crate) fn extract_bits(report: &[u8], field: HidField) -> Option<u32> {
    if field.bit_size == 0 || field.bit_size > 32 {
        return None;
    }
    let mut value = 0u32;
    for bit in 0..field.bit_size {
        let source = field.bit_offset.checked_add(bit)?;
        let byte = *report.get(source / 8)?;
        value |= u32::from((byte >> (source % 8)) & 1) << bit;
    }
    Some(value)
}

pub(crate) fn extract_logical_value(report: &[u8], field: HidField) -> Option<i32> {
    let raw = extract_bits(report, field)?;
    if field.logical_minimum < 0 {
        Some(sign_extend_bits(raw, field.bit_size))
    } else {
        i32::try_from(raw).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repeated_contact_descriptor() -> Vec<u8> {
        let contact = [
            0x09, 0x22, 0xa1, 0x02, // Finger logical collection
            0x09, 0x42, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0x75, 0x07,
            0x95, 0x01, 0x81, 0x03, // Tip Switch padding
            0x09, 0x51, 0x15, 0x00, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, 0x05, 0x01,
            0x09, 0x30, 0x09, 0x31, 0x16, 0x00, 0x00, 0x26, 0xff, 0x0f, 0x75, 0x10, 0x95, 0x02,
            0x81, 0x02, // X/Y
            0x05, 0x0d, 0x09, 0x30, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x01, 0x81,
            0x02, // Tip Pressure
            0x09, 0x48, 0x15, 0x00, 0x25, 0xff, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, 0xc0,
        ];
        let mut descriptor = vec![0x05, 0x0d, 0x09, 0x04, 0xa1, 0x01];
        descriptor.extend_from_slice(&contact);
        descriptor.extend_from_slice(&contact);
        descriptor.extend_from_slice(&[
            0x09, 0x54, 0x15, 0x00, 0x25, 0x02, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, 0xc0,
        ]);
        descriptor
    }

    #[test]
    fn parses_and_decodes_repeated_contacts_with_identifiers() {
        let layout =
            parse_touch_layout(&repeated_contact_descriptor()).expect("valid touch layout");
        assert_eq!(layout.contact_capacity(), 2);
        assert_eq!(layout.contact_ids.len(), 2);
        assert_eq!(layout.pressure.len(), 2);
        assert_eq!(layout.touch_major.len(), 2);

        let report = [
            0x01, 0x0b, 0x34, 0x02, 0x78, 0x01, 0x1e, 0x06, // id 11
            0x01, 0x15, 0xcd, 0x03, 0x56, 0x04, 0x2a, 0x08, // id 21
            0x02,
        ];
        let contacts = decode_contacts(&report, &layout).expect("complete report");
        assert_eq!(contacts.len(), 2);
        assert_eq!(contacts[0].hardware_id, Some(11));
        assert_eq!(contacts[0].x, 0x234);
        assert_eq!(contacts[0].y, 0x178);
        assert_eq!(contacts[0].pressure, Some(30));
        assert_eq!(contacts[0].touch_major, Some(6));
        assert_eq!(contacts[1].hardware_id, Some(21));
        assert_eq!(contacts[1].x, 0x3cd);
        assert_eq!(contacts[1].y, 0x456);
    }

    #[test]
    fn rejects_malformed_contact_records_and_falls_back_without_ids() {
        let mut layout =
            parse_touch_layout(&repeated_contact_descriptor()).expect("valid touch layout");
        let report = [
            0x01, 0x0b, 0x34, 0x02, 0x78, 0x01, 0x1e, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x01,
        ];
        layout.contact_count = None;
        assert_eq!(
            decode_contacts(&report[..3], &layout),
            Err("goodix: malformed HID X field")
        );
        layout.contact_ids.clear();
        let contacts = decode_contacts(&report, &layout).expect("contact IDs are optional");
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].hardware_id, None);
        assert_eq!(contacts[0].descriptor_slot, 0);
    }

    fn contact(descriptor_slot: usize, hardware_id: Option<i32>, x: i32, y: i32) -> HidContact {
        HidContact {
            descriptor_slot,
            hardware_id,
            x,
            y,
            pressure: Some(10),
            touch_major: None,
        }
    }

    #[test]
    fn type_b_slots_keep_hardware_ids_across_move_lift_and_reuse() {
        let mut tracker = SlotTracker::new(2);
        let first = tracker.apply(&[
            contact(0, Some(41), 100, 200),
            contact(1, Some(77), 300, 400),
        ]);
        assert_eq!(first.updates.len(), 2);
        assert_eq!(first.updates[0].slot, 0);
        assert_eq!(first.updates[0].tracking_id, 1);
        assert_eq!(first.updates[0].contact.x, 100);
        assert!(first.updates[0].began);
        assert_eq!(first.updates[1].slot, 1);
        assert_eq!(first.updates[1].tracking_id, 2);
        assert!(first.releases.is_empty());
        assert!(first.any_contact);

        // The controller changed descriptor order, but IDs retain their slots.
        let moved = tracker.apply(&[
            contact(0, Some(77), 305, 405),
            contact(1, Some(41), 105, 205),
        ]);
        assert_eq!(moved.updates[0].slot, 1);
        assert_eq!(moved.updates[1].slot, 0);
        assert!(!moved.updates[0].began);
        assert!(!moved.updates[1].began);

        let one_lifted = tracker.apply(&[contact(0, Some(77), 310, 410)]);
        assert_eq!(one_lifted.updates[0].slot, 1);
        assert_eq!(one_lifted.releases, vec![0]);

        let reused = tracker.apply(&[
            contact(0, Some(77), 315, 415),
            contact(1, Some(99), 500, 600),
        ]);
        assert_eq!(reused.updates[0].slot, 1);
        assert!(!reused.updates[0].began);
        assert_eq!(reused.updates[1].slot, 0);
        assert!(reused.updates[1].began);
        assert_eq!(reused.updates[1].tracking_id, 3);
    }

    #[test]
    fn descriptor_slots_are_deterministic_without_hardware_contact_ids() {
        let mut tracker = SlotTracker::new(2);
        let first = tracker.apply(&[contact(0, None, 10, 20), contact(1, None, 30, 40)]);
        assert_eq!(first.updates[0].slot, 0);
        assert_eq!(first.updates[1].slot, 1);

        let moved = tracker.apply(&[contact(0, None, 11, 21), contact(1, None, 31, 41)]);
        assert_eq!(moved.updates[0].slot, 0);
        assert_eq!(moved.updates[1].slot, 1);
        assert!(!moved.updates[0].began);
        assert!(!moved.updates[1].began);
    }

    #[test]
    fn no_contact_frame_releases_every_slot_and_clears_touch_state() {
        let mut tracker = SlotTracker::new(2);
        tracker.apply(&[contact(0, Some(1), 10, 20), contact(1, Some(2), 30, 40)]);
        let released = tracker.apply(&[]);
        assert!(released.updates.is_empty());
        assert_eq!(released.releases, vec![0, 1]);
        assert_eq!(released.primary, None);
        assert!(!released.any_contact);
    }

    #[test]
    fn parses_touch_fields_from_hid_report_descriptor() {
        let descriptor = [
            0x05, 0x0d, 0x09, 0x04, 0xa1, 0x01, 0x85, 0x01, 0x09, 0x42, 0x15, 0x00, 0x25, 0x01,
            0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0x75, 0x07, 0x95, 0x01, 0x81, 0x03, 0x05, 0x01,
            0x09, 0x30, 0x09, 0x31, 0x16, 0x00, 0x00, 0x26, 0xff, 0x0f, 0x75, 0x10, 0x95, 0x02,
            0x81, 0x02, 0xc0,
        ];
        let layout = parse_touch_layout(&descriptor).expect("valid touch layout");
        assert_eq!(layout.report_id, 1);
        assert_eq!(layout.tips[0].bit_offset, 8);
        assert_eq!(layout.x[0].bit_offset, 16);
        assert_eq!(layout.y[0].bit_offset, 32);
        assert_eq!(layout.x[0].logical_minimum, 0);
        assert_eq!(layout.x[0].logical_maximum, 4095);
    }

    #[test]
    fn parses_signed_and_nonzero_logical_ranges() {
        let descriptor = [
            0x05, 0x0d, 0x09, 0x04, 0xa1, 0x01, 0x09, 0x42, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01,
            0x95, 0x01, 0x81, 0x02, 0x75, 0x07, 0x95, 0x01, 0x81, 0x03, 0x05, 0x01, 0x09, 0x30,
            0x16, 0x00, 0xff, 0x26, 0xff, 0x00, 0x75, 0x10, 0x95, 0x01, 0x81, 0x02, 0x09, 0x31,
            0x15, 0x0a, 0x25, 0xff, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, 0xc0,
        ];
        let layout = parse_touch_layout(&descriptor).expect("valid signed touch layout");
        assert_eq!(layout.x[0].logical_minimum, -256);
        assert_eq!(layout.x[0].logical_maximum, 255);
        assert_eq!(layout.y[0].logical_minimum, 10);
        assert_eq!(layout.y[0].logical_maximum, 255);
    }

    #[test]
    fn extracts_unaligned_hid_fields() {
        let report = [0b1010_1100, 0b0000_0011];
        let field = HidField {
            report_id: 0,
            bit_offset: 3,
            bit_size: 7,
            logical_minimum: 0,
            logical_maximum: 0x7f,
        };
        assert_eq!(extract_bits(&report, field), Some(0x75));
    }

    #[test]
    fn raw_bit_extraction_stays_unsigned_but_logical_values_are_signed() {
        let field = HidField {
            report_id: 0,
            bit_offset: 0,
            bit_size: 16,
            logical_minimum: -256,
            logical_maximum: 255,
        };
        let report = 0xff80u16.to_le_bytes();
        assert_eq!(extract_bits(&report, field), Some(0xff80));
        assert_eq!(extract_logical_value(&report, field), Some(-128));
    }

    #[test]
    fn rejects_legacy_or_non_touch_hid_descriptors() {
        assert!(parse_touch_layout(&[0x05, 0x01, 0x09, 0x02]).is_err());
        let pen_descriptor = [
            0x05, 0x0d, 0x09, 0x02, 0xa1, 0x01, // Digitizer Pen application
            0x85, 0x07, 0x09, 0x42, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0x75, 0x07, 0x95, 0x01,
            0x81, 0x03, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02,
            0xc0,
        ];
        assert!(parse_touch_layout(&pen_descriptor).is_err());
    }

    #[test]
    fn encodes_i2c_hid_power_and_reset_commands_little_endian() {
        assert_eq!(
            encode_simple_command(0x1234, 0x08),
            [0x34, 0x12, 0x00, 0x08]
        );
        assert_eq!(
            encode_simple_command(0x1234, 0x01),
            [0x34, 0x12, 0x00, 0x01]
        );
    }

    #[test]
    fn recognizes_zero_length_reset_completion() {
        assert_eq!(decode_input_length(&[0x00, 0x00, 0xaa, 0xbb]), Some(0));
        assert_eq!(decode_input_length(&[0x06, 0x00, 1, 2, 3, 4]), Some(6));
        assert_eq!(decode_input_length(&[0x00]), None);
    }

    #[test]
    fn rejects_oversized_report_fields_and_total_reports() {
        let oversized_touch_size = [
            0x05, 0x0d, 0x09, 0x04, 0xa1, 0x01, 0x09, 0x42, 0x75, 33, 0x95, 0x01, 0x81, 0x02,
        ];
        assert_eq!(
            parse_touch_layout(&oversized_touch_size).map(|_| ()),
            Err("goodix: HID touch field size exceeds limit")
        );
        let oversized_count = [
            0x05, 0x0d, 0x09, 0x04, 0xa1, 0x01, 0x75, 0x00, 0x96, 0x01, 0x10, 0x81, 0x03,
        ];
        assert_eq!(
            parse_touch_layout(&oversized_count).map(|_| ()),
            Err("goodix: HID input field count exceeds limit")
        );
        let oversized_report = [
            0x05, 0x0d, 0x09, 0x04, 0xa1, 0x01, 0x75, 0x20, 0x95, 0x81, 0x09, 0x42, 0x81, 0x02,
        ];
        assert_eq!(
            parse_touch_layout(&oversized_report).map(|_| ()),
            Err("goodix: HID input report exceeds limit")
        );
        let logical_maximum_above_i32 = [
            0x05, 0x0d, 0x09, 0x04, 0xa1, 0x01, 0x09, 0x42, 0x15, 0x00, 0x27, 0xff, 0xff, 0xff,
            0xff, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0xc0,
        ];
        assert_eq!(
            parse_touch_layout(&logical_maximum_above_i32).map(|_| ()),
            Err("goodix: HID logical maximum exceeds i32")
        );
    }

    #[test]
    fn skips_large_feature_and_non_touch_fields_before_valid_touch_report() {
        let descriptor = [
            // A 128-bit vendor Feature must not constrain touch extraction.
            0x06, 0x00, 0xff, 0x09, 0x01, 0xa1, 0x01, 0x75, 0x80, 0x95, 0x01, 0x09, 0x02, 0xb1,
            0x02, 0xc0,
            // A bounded 64-bit vendor Input contributes to report ID 1's
            // offset, but is not itself materialized as an extractable field.
            0x06, 0x00, 0xff, 0x09, 0x03, 0xa1, 0x01, 0x85, 0x01, 0x75, 0x40, 0x95, 0x01, 0x09,
            0x04, 0x81, 0x02, 0xc0,
            // Valid touchscreen fields follow in the same report.
            0x05, 0x0d, 0x09, 0x04, 0xa1, 0x01, 0x85, 0x01, 0x09, 0x42, 0x15, 0x00, 0x25, 0x01,
            0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0x75, 0x07, 0x95, 0x01, 0x81, 0x03, 0x05, 0x01,
            0x09, 0x30, 0x09, 0x31, 0x16, 0x00, 0x00, 0x26, 0xff, 0x0f, 0x75, 0x10, 0x95, 0x02,
            0x81, 0x02, 0xc0,
        ];
        let layout = parse_touch_layout(&descriptor).expect("valid touch after vendor fields");
        assert_eq!(layout.report_id, 1);
        assert_eq!(layout.tips[0].bit_offset, 72);
        assert_eq!(layout.x[0].bit_offset, 80);
        assert_eq!(layout.y[0].bit_offset, 96);
        assert_eq!(layout.x[0].bit_size, 16);
    }
}
