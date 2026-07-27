//! XTEST extension wire decoding/encoding.
//!
//! XTEST 2.2 has four requests: `GetVersion`, `CompareCursor`,
//! `FakeInput`, `GrabControl`. No events, no errors of its own.

use super::{
    ClientByteOrder, SequenceNumber,
    wire::{write_u16, write_u32},
};

pub const GET_VERSION: u8 = 0;
pub const COMPARE_CURSOR: u8 = 1;
pub const FAKE_INPUT: u8 = 2;
pub const GRAB_CONTROL: u8 = 3;

pub const MAJOR_VERSION: u16 = 2;
pub const MINOR_VERSION: u16 = 2;

/// `type` field values inside `FakeInput`. They mirror core X11 event codes.
pub const FAKE_KEY_PRESS: u8 = 2;
pub const FAKE_KEY_RELEASE: u8 = 3;
pub const FAKE_BUTTON_PRESS: u8 = 4;
pub const FAKE_BUTTON_RELEASE: u8 = 5;
pub const FAKE_MOTION_NOTIFY: u8 = 6;

fn read_u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn read_i16_le(bytes: &[u8]) -> i16 {
    i16::from_le_bytes([bytes[0], bytes[1]])
}

/// Parse a `GetVersion` request body. Returns `(client_major, client_minor)`.
///
/// Wire body (4 bytes after the 4-byte request header):
/// - u8 client_major
/// - u8 pad
/// - u16 client_minor
#[must_use]
pub fn parse_get_version(body: &[u8]) -> Option<(u8, u16)> {
    if body.len() < 4 {
        return None;
    }
    let major = body[0];
    let minor = u16::from_le_bytes([body[2], body[3]]);
    Some((major, minor))
}

/// Encode the `GetVersion` reply. The server announces the version it speaks.
///
/// Reply layout: standard 32-byte X reply with `major_version` in the
/// data16 field (byte offset 8) and `minor_version` in bytes 8..10.
#[must_use]
pub fn encode_get_version_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    major_version: u8,
    minor_version: u16,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1); // reply
    out.push(major_version);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0); // reply length = 0
    write_u16(byte_order, &mut out, minor_version);
    out.extend_from_slice(&[0u8; 22]);
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encode a `CompareCursor` reply. `same` is 1 if the window's cursor matches
/// the comparison cursor.
#[must_use]
pub fn encode_compare_cursor_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    same: bool,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1); // reply
    out.push(u8::from(same));
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0);
    out.extend_from_slice(&[0u8; 24]);
    debug_assert_eq!(out.len(), 32);
    out
}

/// One `FakeInput` event. `root_x`/`root_y` are absolute root coordinates
/// for `MotionNotify` with `detail==0`, or deltas with `detail==1`. For key
/// and button events `detail` is the keycode / button number and the
/// coordinates are ignored by clients in practice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FakeInput {
    pub event_type: u8,
    pub detail: u8,
    pub time: u32,
    pub root: u32,
    pub root_x: i16,
    pub root_y: i16,
}

/// Parse a `FakeInput` body (28 bytes after the 4-byte request header).
///
/// Layout (offsets within `body`):
/// ```text
///  0  u8  type
///  1  u8  detail
///  2  u16 pad
///  4  u32 time (ms; 0 = process now, CurrentTime)
///  8  u32 root (0 = None / current screen)
/// 12  u32 pad1
/// 16  u32 pad2
/// 20  i16 rootX
/// 22  i16 rootY
/// 24  u32 pad3
/// ```
#[must_use]
pub fn parse_fake_input(body: &[u8]) -> Option<FakeInput> {
    if body.len() < 28 {
        return None;
    }
    Some(FakeInput {
        event_type: body[0],
        detail: body[1],
        time: read_u32_le(&body[4..]),
        root: read_u32_le(&body[8..]),
        root_x: read_i16_le(&body[20..]),
        root_y: read_i16_le(&body[22..]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_get_version_extracts_major_minor() {
        let body = [2u8, 0, 2, 0]; // major=2, pad, minor=2
        assert_eq!(parse_get_version(&body), Some((2, 2)));
    }

    #[test]
    fn parse_get_version_too_short_is_none() {
        assert_eq!(parse_get_version(&[1, 2, 3]), None);
    }

    #[test]
    fn encode_get_version_reply_is_32_bytes_with_minor_in_data16() {
        let reply =
            encode_get_version_reply(ClientByteOrder::LittleEndian, SequenceNumber(7), 2, 2);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[0], 1); // reply tag
        assert_eq!(reply[1], 2); // major in data8
        assert_eq!(u16::from_le_bytes([reply[2], reply[3]]), 7); // sequence
        assert_eq!(
            u32::from_le_bytes([reply[4], reply[5], reply[6], reply[7]]),
            0
        );
        assert_eq!(u16::from_le_bytes([reply[8], reply[9]]), 2); // minor
    }

    #[test]
    fn encode_compare_cursor_reply_carries_same_bit() {
        let yes =
            encode_compare_cursor_reply(ClientByteOrder::LittleEndian, SequenceNumber(3), true);
        assert_eq!(yes[1], 1);
        let no =
            encode_compare_cursor_reply(ClientByteOrder::LittleEndian, SequenceNumber(3), false);
        assert_eq!(no[1], 0);
    }

    #[test]
    fn parse_fake_input_motion_with_absolute_coords() {
        let mut body = [0u8; 28];
        body[0] = FAKE_MOTION_NOTIFY;
        body[1] = 0; // absolute
        body[4..8].copy_from_slice(&100u32.to_le_bytes()); // time
        body[8..12].copy_from_slice(&0u32.to_le_bytes()); // root
        body[20..22].copy_from_slice(&123i16.to_le_bytes());
        body[22..24].copy_from_slice(&(-45i16).to_le_bytes());
        let fi = parse_fake_input(&body).unwrap();
        assert_eq!(fi.event_type, FAKE_MOTION_NOTIFY);
        assert_eq!(fi.detail, 0);
        assert_eq!(fi.time, 100);
        assert_eq!(fi.root_x, 123);
        assert_eq!(fi.root_y, -45);
    }

    #[test]
    fn parse_fake_input_button_press_keeps_detail_as_button_number() {
        let mut body = [0u8; 28];
        body[0] = FAKE_BUTTON_PRESS;
        body[1] = 1; // left button
        let fi = parse_fake_input(&body).unwrap();
        assert_eq!(fi.event_type, FAKE_BUTTON_PRESS);
        assert_eq!(fi.detail, 1);
    }

    #[test]
    fn parse_fake_input_too_short_is_none() {
        assert_eq!(parse_fake_input(&[0u8; 27]), None);
    }
}
