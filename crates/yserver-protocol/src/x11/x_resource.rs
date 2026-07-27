//! X-Resource extension (`Res`).
//!
//! `QueryClients` (the connected-client list) and `QueryClientResources`
//! (per-type resource counts for a client) return real data — the two
//! queries `xrestop` leans on. The remaining queries
//! `QueryClientPixmapBytes` is computed from live pixmap geometry. The
//! `QueryClientIds` reports ClientXID identities but omits PID identities
//! because yserver does not retain peer credentials. `QueryResourceBytes`
//! remains empty until recursive resource-size accounting exists.
//!
//! Canonical layout: `/usr/share/xcb/res.xml`, version 1.2.

use super::{
    ClientByteOrder, SequenceNumber,
    wire::{write_u16, write_u32},
};

pub const QUERY_VERSION: u8 = 0;
pub const QUERY_CLIENTS: u8 = 1;
pub const QUERY_CLIENT_RESOURCES: u8 = 2;
pub const QUERY_CLIENT_PIXMAP_BYTES: u8 = 3;
pub const QUERY_CLIENT_IDS: u8 = 4;
pub const QUERY_RESOURCE_BYTES: u8 = 5;

pub const MAJOR_VERSION: u16 = 1;
pub const MINOR_VERSION: u16 = 2;
pub const CLIENT_XID_MASK: u32 = 1 << 0;
pub const LOCAL_CLIENT_PID_MASK: u32 = 1 << 1;

fn read_u8(bytes: &[u8], idx: usize) -> u8 {
    bytes[idx]
}

/// Parse `QueryVersion(client_major: CARD8, client_minor: CARD8)`.
#[must_use]
pub fn parse_query_version(body: &[u8]) -> Option<(u8, u8)> {
    if body.len() < 2 {
        return None;
    }
    Some((read_u8(body, 0), read_u8(body, 1)))
}

/// Reply layout per `res.xml`:
///
/// ```text
/// response(1) pad(1) seq(2) reply_length(4)
/// server_major(2) server_minor(2) pad(20)
/// ```
///
/// Returns the negotiated `(major, minor)` clamped to our supported
/// `(1, 2)`.
#[must_use]
pub fn encode_query_version_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    server_major: u16,
    server_minor: u16,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0); // reply_length = 0 (fixed-size reply)
    write_u16(byte_order, &mut out, server_major);
    write_u16(byte_order, &mut out, server_minor);
    out.extend_from_slice(&[0u8; 20]);
    debug_assert_eq!(out.len(), 32);
    out
}

/// Empty `QueryClients` reply: `num_clients = 0`, no `Client` entries.
/// Per `res.xml` reply layout: pad(1) num_clients(4) pad(20) clients[].
#[must_use]
pub fn encode_query_clients_empty_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> Vec<u8> {
    encode_count_reply_32_byte(byte_order, sequence, 0)
}

/// `QueryClients` reply listing every connected client by its XID
/// resource range. Layout per `res.xml`: pad(1) reply_length(4 = 2·n)
/// num_clients(4) pad(20), then `n` × { resource_base:CARD32,
/// resource_mask:CARD32 }. Each `Client` is 8 bytes = 2 reply units.
#[must_use]
pub fn encode_query_clients_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    clients: &[(u32, u32)],
) -> Vec<u8> {
    let n = u32::try_from(clients.len()).unwrap_or(0);
    let mut out = Vec::with_capacity(32 + clients.len() * 8);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, n.saturating_mul(2)); // reply_length (units)
    write_u32(byte_order, &mut out, n); // num_clients @8
    out.extend_from_slice(&[0u8; 20]); // pad @12..32
    for (base, mask) in clients {
        write_u32(byte_order, &mut out, *base);
        write_u32(byte_order, &mut out, *mask);
    }
    out
}

/// `QueryClientResources` reply: per-type resource counts for one
/// client. Layout per `res.xml`: pad(1) reply_length(4 = 2·n)
/// num_types(4) pad(20), then `n` × `Type { resource_type:ATOM,
/// count:CARD32 }`. Each `Type` is 8 bytes = 2 reply units.
#[must_use]
pub fn encode_query_client_resources_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    types: &[(u32, u32)],
) -> Vec<u8> {
    let n = u32::try_from(types.len()).unwrap_or(0);
    let mut out = Vec::with_capacity(32 + types.len() * 8);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, n.saturating_mul(2)); // reply_length (units)
    write_u32(byte_order, &mut out, n); // num_types @8
    out.extend_from_slice(&[0u8; 20]); // pad @12..32
    for (resource_type, count) in types {
        write_u32(byte_order, &mut out, *resource_type);
        write_u32(byte_order, &mut out, *count);
    }
    out
}

/// Empty `QueryClientResources` reply: `num_types = 0`.
/// Layout: pad(1) num_types(4) pad(20) types[].
#[must_use]
pub fn encode_query_client_resources_empty_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> Vec<u8> {
    encode_count_reply_32_byte(byte_order, sequence, 0)
}

/// `QueryClientPixmapBytes` reply. The 64-bit total is split into the low
/// `bytes` word and high `bytes_overflow` word used by the protocol.
/// Layout: pad(1) bytes(4) bytes_overflow(4) pad(16).
#[must_use]
pub fn encode_query_client_pixmap_bytes_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    bytes: u64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0);
    let words = bytes.to_le_bytes();
    write_u32(
        byte_order,
        &mut out,
        u32::from_le_bytes(words[0..4].try_into().unwrap()),
    );
    write_u32(
        byte_order,
        &mut out,
        u32::from_le_bytes(words[4..8].try_into().unwrap()),
    );
    out.extend_from_slice(&[0u8; 16]);
    debug_assert_eq!(out.len(), 32);
    out
}

/// Empty `QueryClientIds` reply (v1.2): `num_ids = 0`.
/// Layout: pad(1) num_ids(4) pad(20) ids[].
#[must_use]
pub fn encode_query_client_ids_empty_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> Vec<u8> {
    encode_count_reply_32_byte(byte_order, sequence, 0)
}

/// One `ClientIdValue` in a `QueryClientIds` reply: the 12-byte header
/// `{client, mask, length}` plus `length` bytes of value words.
///
/// Xorg emits one of these PER IDENTITY, not per client, so a client with both
/// a XID and a PID identity contributes two entries and counts twice in
/// `num_ids` (`Xext/xres.c` `ConstructClientIdValue`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientIdEntry {
    /// The subject client, as its resource-id base (Xorg's `clientAsMask`).
    pub client: u32,
    /// Exactly one of `CLIENT_XID_MASK` / `LOCAL_CLIENT_PID_MASK`.
    pub mask: u32,
    /// Value word, if this identity carries one. `None` encodes `length = 0`
    /// (ClientXID); `Some(pid)` encodes `length = 4` plus the word.
    pub value: Option<u32>,
}

impl ClientIdEntry {
    /// ClientXID identity: always available, carries no value word.
    #[must_use]
    pub fn xid(client: u32) -> Self {
        Self {
            client,
            mask: CLIENT_XID_MASK,
            value: None,
        }
    }

    /// LocalClientPID identity, carrying the peer pid as its single value word.
    #[must_use]
    pub fn pid(client: u32, pid: u32) -> Self {
        Self {
            client,
            mask: LOCAL_CLIENT_PID_MASK,
            value: Some(pid),
        }
    }

    const fn wire_len(self) -> usize {
        if self.value.is_some() { 16 } else { 12 }
    }
}

/// `QueryClientIds` reply. `num_ids` counts ENTRIES (not clients), and the
/// reply `length` is the total entry bytes in 4-byte units — matching Xorg's
/// `rep.length = bytes_to_int32(ctx->resultBytes)`.
///
/// Per-entry `length` is in BYTES (`rep.length = 4` for a pid), not in words.
#[must_use]
pub fn encode_query_client_ids_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    entries: &[ClientIdEntry],
) -> Vec<u8> {
    let body_bytes: usize = entries.iter().map(|e| e.wire_len()).sum();
    let mut out = Vec::with_capacity(32 + body_bytes);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(
        byte_order,
        &mut out,
        u32::try_from(body_bytes / 4).unwrap_or(u32::MAX),
    );
    write_u32(
        byte_order,
        &mut out,
        u32::try_from(entries.len()).unwrap_or(u32::MAX),
    );
    out.extend_from_slice(&[0u8; 20]);
    for entry in entries {
        write_u32(byte_order, &mut out, entry.client);
        write_u32(byte_order, &mut out, entry.mask);
        match entry.value {
            Some(value) => {
                write_u32(byte_order, &mut out, 4);
                write_u32(byte_order, &mut out, value);
            }
            None => write_u32(byte_order, &mut out, 0),
        }
    }
    out
}

/// Empty `QueryResourceBytes` reply (v1.2): `num_sizes = 0`.
/// Layout: pad(1) num_sizes(4) pad(20) sizes[].
#[must_use]
pub fn encode_query_resource_bytes_empty_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> Vec<u8> {
    encode_count_reply_32_byte(byte_order, sequence, 0)
}

/// Helper for the four replies that share the same shape: a CARD32
/// count at offset 8 followed by 20 bytes of pad (no payload entries).
fn encode_count_reply_32_byte(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    count: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0); // reply_length = 0 (no list entries)
    write_u32(byte_order, &mut out, count);
    out.extend_from_slice(&[0u8; 20]);
    debug_assert_eq!(out.len(), 32);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_constants_match_xcb_res_xml() {
        // Canonical: /usr/share/xcb/res.xml `major-version="1" minor-version="2"`.
        assert_eq!(MAJOR_VERSION, 1);
        assert_eq!(MINOR_VERSION, 2);
    }

    #[test]
    fn opcode_constants_match_xcb_res_xml() {
        // Canonical: /usr/share/xcb/res.xml `<request name="X" opcode="N">`.
        assert_eq!(QUERY_VERSION, 0);
        assert_eq!(QUERY_CLIENTS, 1);
        assert_eq!(QUERY_CLIENT_RESOURCES, 2);
        assert_eq!(QUERY_CLIENT_PIXMAP_BYTES, 3);
        assert_eq!(QUERY_CLIENT_IDS, 4);
        assert_eq!(QUERY_RESOURCE_BYTES, 5);
    }

    #[test]
    fn query_version_reply_layout() {
        let reply =
            encode_query_version_reply(ClientByteOrder::LittleEndian, SequenceNumber(7), 1, 2);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[0], 1, "response type");
        assert_eq!(reply[2], 7, "sequence low byte");
        assert_eq!(reply[8], 1, "server_major low byte");
        assert_eq!(reply[10], 2, "server_minor low byte");
        // reply_length must be 0 — fixed-size reply.
        assert_eq!(&reply[4..8], &[0, 0, 0, 0]);
    }

    #[test]
    fn query_clients_reply_lists_clients() {
        let clients = [
            (0x0040_0000u32, 0x001f_ffffu32),
            (0x0080_0000u32, 0x001f_ffffu32),
        ];
        let reply =
            encode_query_clients_reply(ClientByteOrder::LittleEndian, SequenceNumber(5), &clients);
        // 32-byte header + 2 clients * 8 bytes.
        assert_eq!(reply.len(), 48);
        assert_eq!(reply[0], 1, "reply");
        assert_eq!(&reply[2..4], &5u16.to_le_bytes(), "sequence");
        // reply_length = 2 units/client * 2 = 4.
        assert_eq!(&reply[4..8], &4u32.to_le_bytes(), "reply_length");
        assert_eq!(&reply[8..12], &2u32.to_le_bytes(), "num_clients @8");
        // Client list begins at offset 32: { resource_base, resource_mask }.
        assert_eq!(&reply[32..36], &0x0040_0000u32.to_le_bytes());
        assert_eq!(&reply[36..40], &0x001f_ffffu32.to_le_bytes());
        assert_eq!(&reply[40..44], &0x0080_0000u32.to_le_bytes());
        assert_eq!(&reply[44..48], &0x001f_ffffu32.to_le_bytes());
    }

    #[test]
    fn query_client_resources_reply_lists_types() {
        // (resource_type ATOM, count) pairs.
        let types = [(0x0000_0123u32, 5u32), (0x0000_0456u32, 2u32)];
        let reply = encode_query_client_resources_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(9),
            &types,
        );
        assert_eq!(reply.len(), 48, "32 header + 2 Type × 8");
        assert_eq!(reply[0], 1, "reply");
        assert_eq!(&reply[2..4], &9u16.to_le_bytes(), "sequence");
        assert_eq!(
            &reply[4..8],
            &4u32.to_le_bytes(),
            "reply_length = 2·num_types"
        );
        assert_eq!(&reply[8..12], &2u32.to_le_bytes(), "num_types @8");
        // Type list at offset 32: { resource_type:ATOM, count:CARD32 }.
        assert_eq!(&reply[32..36], &0x0000_0123u32.to_le_bytes(), "type0 atom");
        assert_eq!(&reply[36..40], &5u32.to_le_bytes(), "type0 count");
        assert_eq!(&reply[40..44], &0x0000_0456u32.to_le_bytes(), "type1 atom");
        assert_eq!(&reply[44..48], &2u32.to_le_bytes(), "type1 count");
    }

    #[test]
    fn empty_query_clients_reply_layout() {
        let reply =
            encode_query_clients_empty_reply(ClientByteOrder::LittleEndian, SequenceNumber(3));
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[0], 1);
        // num_clients at offset 8 must be 0.
        assert_eq!(&reply[8..12], &[0, 0, 0, 0]);
        // reply_length 0 (no Client entries follow).
        assert_eq!(&reply[4..8], &[0, 0, 0, 0]);
    }
}
