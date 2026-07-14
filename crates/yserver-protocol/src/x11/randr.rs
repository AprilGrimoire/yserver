use super::{
    ClientByteOrder, SequenceNumber,
    wire::{write_i16, write_u16, write_u32},
};

/// Type-dispatched writer that picks `write_u16`/`write_u32`/`write_i16`
/// based on the integer type. Lets the encoder body stay terse with a
/// single `put(byte_order, &mut out, x)` form regardless of x's width.
trait Put {
    fn put(self, byte_order: ClientByteOrder, out: &mut Vec<u8>);
}
impl Put for u16 {
    fn put(self, byte_order: ClientByteOrder, out: &mut Vec<u8>) {
        write_u16(byte_order, out, self);
    }
}
impl Put for u32 {
    fn put(self, byte_order: ClientByteOrder, out: &mut Vec<u8>) {
        write_u32(byte_order, out, self);
    }
}
impl Put for i16 {
    fn put(self, byte_order: ClientByteOrder, out: &mut Vec<u8>) {
        write_i16(byte_order, out, self);
    }
}
fn put<T: Put>(byte_order: ClientByteOrder, out: &mut Vec<u8>, x: T) {
    x.put(byte_order, out);
}

// ── Version ──────────────────────────────────────────────────────────────────

pub const MAJOR_VERSION: u32 = 1;
pub const MINOR_VERSION: u32 = 5;

// ── Minor opcode constants ────────────────────────────────────────────────────

pub const RR_QUERY_VERSION: u8 = 0;
pub const RR_SET_SCREEN_CONFIG: u8 = 2;
pub const RR_SELECT_INPUT: u8 = 4;
pub const RR_GET_SCREEN_INFO: u8 = 5;
pub const RR_GET_SCREEN_SIZE_RANGE: u8 = 6;
pub const RR_SET_SCREEN_SIZE: u8 = 7;
pub const RR_GET_SCREEN_RESOURCES: u8 = 8;
pub const RR_GET_OUTPUT_INFO: u8 = 9;
pub const RR_LIST_OUTPUT_PROPERTIES: u8 = 10;
pub const RR_QUERY_OUTPUT_PROPERTY: u8 = 11;
pub const RR_CONFIGURE_OUTPUT_PROPERTY: u8 = 12;
pub const RR_CHANGE_OUTPUT_PROPERTY: u8 = 13;
pub const RR_DELETE_OUTPUT_PROPERTY: u8 = 14;
pub const RR_GET_OUTPUT_PROPERTY: u8 = 15;
pub const RR_CREATE_MODE: u8 = 16;
pub const RR_DESTROY_MODE: u8 = 17;
pub const RR_ADD_OUTPUT_MODE: u8 = 18;
pub const RR_DELETE_OUTPUT_MODE: u8 = 19;
pub const RR_GET_CRTC_INFO: u8 = 20;
pub const RR_SET_CRTC_CONFIG: u8 = 21;
pub const RR_GET_CRTC_GAMMA_SIZE: u8 = 22;
pub const RR_GET_CRTC_GAMMA: u8 = 23;
pub const RR_SET_CRTC_GAMMA: u8 = 24;
pub const RR_GET_SCREEN_RESOURCES_CURRENT: u8 = 25;
pub const RR_SET_CRTC_TRANSFORM: u8 = 26;
pub const RR_GET_CRTC_TRANSFORM: u8 = 27;
pub const RR_GET_PANNING: u8 = 28;
pub const RR_SET_PANNING: u8 = 29;
pub const RR_SET_OUTPUT_PRIMARY: u8 = 30;
pub const RR_GET_OUTPUT_PRIMARY: u8 = 31;
pub const RR_GET_PROVIDERS: u8 = 32;
pub const RR_GET_PROVIDER_INFO: u8 = 33;
pub const RR_SET_PROVIDER_OFFLOAD_SINK: u8 = 34;
pub const RR_SET_PROVIDER_OUTPUT_SOURCE: u8 = 35;
pub const RR_LIST_PROVIDER_PROPERTIES: u8 = 36;
pub const RR_QUERY_PROVIDER_PROPERTY: u8 = 37;
pub const RR_CONFIGURE_PROVIDER_PROPERTY: u8 = 38;
pub const RR_CHANGE_PROVIDER_PROPERTY: u8 = 39;
pub const RR_DELETE_PROVIDER_PROPERTY: u8 = 40;
pub const RR_GET_PROVIDER_PROPERTY: u8 = 41;
pub const RR_GET_MONITORS: u8 = 42;
pub const RR_SET_MONITOR: u8 = 43;
pub const RR_DELETE_MONITOR: u8 = 44;
pub const RR_CREATE_LEASE: u8 = 45;
pub const RR_FREE_LEASE: u8 = 46;

pub const NOTIFY_MASK_SCREEN_CHANGE: u16 = 1 << 0;
pub const NOTIFY_MASK_CRTC_CHANGE: u16 = 1 << 1;
pub const NOTIFY_MASK_OUTPUT_CHANGE: u16 = 1 << 2;
pub const NOTIFY_MASK_PROVIDER_CHANGE: u16 = 1 << 4;

pub const EVENT_SCREEN_CHANGE_NOTIFY: u8 = 0;
pub const EVENT_NOTIFY: u8 = 1;
pub const NOTIFY_CRTC_CHANGE: u8 = 0;
pub const NOTIFY_OUTPUT_CHANGE: u8 = 1;
pub const NOTIFY_PROVIDER_CHANGE: u8 = 3;
pub const ROTATION_ROTATE_0: u16 = 1;
pub const SET_CONFIG_SUCCESS: u8 = 0;
pub const SET_CONFIG_FAILED: u8 = 3;
pub const SUBPIXEL_UNKNOWN: u16 = 0;
pub const CONNECTION_CONNECTED: u8 = 0;
pub const CONNECTION_DISCONNECTED: u8 = 1;
pub const PROVIDER_CAPABILITY_SOURCE_OUTPUT: u32 = 1 << 0;
pub const PROVIDER_CAPABILITY_SINK_OUTPUT: u32 = 1 << 1;
pub const PROVIDER_CAPABILITY_SOURCE_OFFLOAD: u32 = 1 << 2;
pub const PROVIDER_CAPABILITY_SINK_OFFLOAD: u32 = 1 << 3;
/// RANDR extension-relative error number (`BadProvider`).
pub const ERROR_BAD_PROVIDER: u8 = 3;

// ── Local wire helpers (mirrors of wire.rs helpers, private to this module) ──

fn read_u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn read_u16_le(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

/// Round `len` up to the nearest multiple of 4.
fn pad4(len: usize) -> usize {
    (len + 3) & !3
}

/// Pad `out` with zero bytes until its length is a multiple of 4.
fn pad_vec4(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

/// Create the standard 8-byte prefix for an X11 reply:
/// `[1, data, seq_lo, seq_hi, length_bytes…]` (little-endian u32 `length`).
fn fixed_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    data: u8,
    length: u32,
) -> Vec<u8> {
    let mut reply = Vec::with_capacity(32);
    reply.push(1u8); // reply type
    reply.push(data);
    put(byte_order, &mut reply, sequence.0);
    put(byte_order, &mut reply, length);
    reply
}

// ── Request structs ───────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
pub struct QueryVersionRequest {
    pub major: u32,
    pub minor: u32,
}

#[derive(Debug, PartialEq)]
pub struct ScreenRequest {
    pub window: u32,
}

#[derive(Debug, PartialEq)]
pub struct OutputRequest {
    pub output: u32,
    pub config_timestamp: u32,
}

#[derive(Debug, PartialEq)]
pub struct OutputPropertyRequest {
    pub output: u32,
    pub property: u32,
}

/// `RRGetOutputProperty` (randrproto §RRGetOutputProperty): the full
/// GetProperty-style request. `long_offset`/`long_length` are in
/// 32-bit (4-byte) units, matching core `GetProperty`.
#[derive(Debug, PartialEq, Eq)]
pub struct GetOutputPropertyRequest {
    pub output: u32,
    pub property: u32,
    pub prop_type: u32,
    pub long_offset: u32,
    pub long_length: u32,
    pub delete: bool,
    pub pending: bool,
}

#[derive(Debug, PartialEq)]
pub struct CrtcRequest {
    pub crtc: u32,
    pub config_timestamp: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub struct CrtcIdRequest {
    pub crtc: u32,
}

#[derive(Debug, PartialEq)]
pub struct SelectInputRequest {
    pub window: u32,
    pub enable: u16,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SetScreenSizeRequest {
    pub window: u32,
    pub width: u16,
    pub height: u16,
    pub mm_width: u32,
    pub mm_height: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SetCrtcTransformRequest {
    pub crtc: u32,
    pub transform: [i32; 9],
    pub filter_name_len: u16,
}

impl SetCrtcTransformRequest {
    #[must_use]
    pub fn is_identity_transform(&self) -> bool {
        self.transform == [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x0001_0000]
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct SetPanningRequest {
    pub crtc: u32,
    pub timestamp: u32,
    pub left: u16,
    pub top: u16,
    pub width: u16,
    pub height: u16,
    pub track_left: u16,
    pub track_top: u16,
    pub track_width: u16,
    pub track_height: u16,
    pub border_left: i16,
    pub border_top: i16,
    pub border_right: i16,
    pub border_bottom: i16,
}

impl SetPanningRequest {
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        self.left == 0
            && self.top == 0
            && self.width == 0
            && self.height == 0
            && self.track_left == 0
            && self.track_top == 0
            && self.track_width == 0
            && self.track_height == 0
            && self.border_left == 0
            && self.border_top == 0
            && self.border_right == 0
            && self.border_bottom == 0
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct SetOutputPrimaryRequest {
    pub window: u32,
    pub output: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ProviderInfoRequest {
    pub provider: u32,
    pub config_timestamp: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SetProviderOffloadSinkRequest {
    pub provider: u32,
    pub sink_provider: u32,
    pub config_timestamp: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SetProviderOutputSourceRequest {
    pub provider: u32,
    pub source_provider: u32,
    pub config_timestamp: u32,
}

// ── Request parsers ───────────────────────────────────────────────────────────

pub fn parse_query_version(body: &[u8]) -> Option<QueryVersionRequest> {
    if body.len() < 8 {
        return None;
    }
    Some(QueryVersionRequest {
        major: read_u32_le(body),
        minor: read_u32_le(&body[4..]),
    })
}

pub fn parse_screen_request(body: &[u8]) -> Option<ScreenRequest> {
    if body.len() < 4 {
        return None;
    }
    Some(ScreenRequest {
        window: read_u32_le(body),
    })
}

pub fn parse_output_request(body: &[u8]) -> Option<OutputRequest> {
    if body.len() < 8 {
        return None;
    }
    Some(OutputRequest {
        output: read_u32_le(body),
        config_timestamp: read_u32_le(&body[4..]),
    })
}

pub fn parse_output_property_request(body: &[u8]) -> Option<OutputPropertyRequest> {
    if body.len() < 8 {
        return None;
    }
    Some(OutputPropertyRequest {
        output: read_u32_le(body),
        property: read_u32_le(&body[4..]),
    })
}

/// Parse `RRGetOutputProperty`. Body: output, property, type,
/// long-offset, long-length (5×CARD32) + delete, pending (2×BOOL) +
/// 2 pad. `delete`/`pending` are tolerated as absent (default false)
/// for short bodies.
pub fn parse_get_output_property_request(body: &[u8]) -> Option<GetOutputPropertyRequest> {
    if body.len() < 20 {
        return None;
    }
    Some(GetOutputPropertyRequest {
        output: read_u32_le(body),
        property: read_u32_le(&body[4..]),
        prop_type: read_u32_le(&body[8..]),
        long_offset: read_u32_le(&body[12..]),
        long_length: read_u32_le(&body[16..]),
        delete: body.get(20).is_some_and(|&b| b != 0),
        pending: body.get(21).is_some_and(|&b| b != 0),
    })
}

pub fn parse_crtc_request(body: &[u8]) -> Option<CrtcRequest> {
    if body.len() < 8 {
        return None;
    }
    Some(CrtcRequest {
        crtc: read_u32_le(body),
        config_timestamp: read_u32_le(&body[4..]),
    })
}

pub fn parse_crtc_id_request(body: &[u8]) -> Option<CrtcIdRequest> {
    if body.len() < 4 {
        return None;
    }
    Some(CrtcIdRequest {
        crtc: read_u32_le(body),
    })
}

pub fn parse_select_input(body: &[u8]) -> Option<SelectInputRequest> {
    if body.len() < 8 {
        return None;
    }
    Some(SelectInputRequest {
        window: read_u32_le(body),
        enable: read_u16_le(&body[4..]),
        // bytes 6-7: padding, ignored
    })
}

/// RANDR 1.2 `SetScreenSize` body (post-X11-request-header):
/// `window(4) width(CARD16) height(CARD16) widthInMillimeters(CARD32)
/// heightInMillimeters(CARD32)`.
pub fn parse_set_screen_size_request(body: &[u8]) -> Option<SetScreenSizeRequest> {
    if body.len() < 16 {
        return None;
    }
    Some(SetScreenSizeRequest {
        window: read_u32_le(body),
        width: read_u16_le(&body[4..]),
        height: read_u16_le(&body[6..]),
        mm_width: read_u32_le(&body[8..]),
        mm_height: read_u32_le(&body[12..]),
    })
}

pub fn parse_set_crtc_transform_request(body: &[u8]) -> Option<SetCrtcTransformRequest> {
    if body.len() < 44 {
        return None;
    }
    let mut transform = [0i32; 9];
    for (idx, slot) in transform.iter_mut().enumerate() {
        let offset = 4 + idx * 4;
        *slot = i32::from_le_bytes(body[offset..offset + 4].try_into().ok()?);
    }
    let filter_name_len = read_u16_le(&body[40..]);
    let padded_filter_len = pad4(usize::from(filter_name_len));
    if body.len() < 44usize.saturating_add(padded_filter_len) {
        return None;
    }
    Some(SetCrtcTransformRequest {
        crtc: read_u32_le(body),
        transform,
        filter_name_len,
    })
}

pub fn parse_set_panning_request(body: &[u8]) -> Option<SetPanningRequest> {
    if body.len() < 32 {
        return None;
    }
    Some(SetPanningRequest {
        crtc: read_u32_le(body),
        timestamp: read_u32_le(&body[4..]),
        left: read_u16_le(&body[8..]),
        top: read_u16_le(&body[10..]),
        width: read_u16_le(&body[12..]),
        height: read_u16_le(&body[14..]),
        track_left: read_u16_le(&body[16..]),
        track_top: read_u16_le(&body[18..]),
        track_width: read_u16_le(&body[20..]),
        track_height: read_u16_le(&body[22..]),
        border_left: i16::from_le_bytes(body[24..26].try_into().ok()?),
        border_top: i16::from_le_bytes(body[26..28].try_into().ok()?),
        border_right: i16::from_le_bytes(body[28..30].try_into().ok()?),
        border_bottom: i16::from_le_bytes(body[30..32].try_into().ok()?),
    })
}

pub fn parse_set_output_primary_request(body: &[u8]) -> Option<SetOutputPrimaryRequest> {
    if body.len() < 8 {
        return None;
    }
    Some(SetOutputPrimaryRequest {
        window: read_u32_le(body),
        output: read_u32_le(&body[4..]),
    })
}

pub fn parse_provider_info_request(body: &[u8]) -> Option<ProviderInfoRequest> {
    if body.len() < 8 {
        return None;
    }
    Some(ProviderInfoRequest {
        provider: read_u32_le(body),
        config_timestamp: read_u32_le(&body[4..]),
    })
}

pub fn parse_set_provider_offload_sink_request(
    body: &[u8],
) -> Option<SetProviderOffloadSinkRequest> {
    if body.len() < 12 {
        return None;
    }
    Some(SetProviderOffloadSinkRequest {
        provider: read_u32_le(body),
        sink_provider: read_u32_le(&body[4..]),
        config_timestamp: read_u32_le(&body[8..]),
    })
}

pub fn parse_set_provider_output_source_request(
    body: &[u8],
) -> Option<SetProviderOutputSourceRequest> {
    if body.len() < 12 {
        return None;
    }
    Some(SetProviderOutputSourceRequest {
        provider: read_u32_le(body),
        source_provider: read_u32_le(&body[4..]),
        config_timestamp: read_u32_le(&body[8..]),
    })
}

// ── Reply data structs ────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct ModeInfo {
    pub id: u32,
    pub width: u16,
    pub height: u16,
    pub dot_clock: u32,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    /// Length of the mode name in bytes.
    pub name_len: u16,
    pub mode_flags: u32,
}

#[derive(Debug)]
pub struct ScreenResources {
    pub timestamp: u32,
    pub config_timestamp: u32,
    pub crtcs: Vec<u32>,
    pub outputs: Vec<u32>,
    pub modes: Vec<ModeInfo>,
    /// Concatenated mode name bytes.
    pub mode_names: Vec<u8>,
}

// ── Reply encoders ────────────────────────────────────────────────────────────

/// Encodes a `QueryVersion` reply (32 bytes total).
pub fn encode_query_version_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    major: u32,
    minor: u32,
) -> Vec<u8> {
    let mut out = fixed_reply(byte_order, sequence, 0, 0);
    // out is now 8 bytes; add major + minor (8 bytes) then pad to 32
    put(byte_order, &mut out, major);
    put(byte_order, &mut out, minor);
    out.extend_from_slice(&[0u8; 16]);
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encodes a `GetScreenInfo` reply for the single synthetic mode.
///
/// Layout (RANDR 1.1+): 32-byte header followed by one `ScreenSize` (8 bytes)
/// and one `RefreshRates` list (`nRates` u16 + `nRates` * 2 bytes, padded to 4
/// bytes). `nInfo = nSizes + nRefreshLists` so libXrandr can iterate the
/// trailing refresh-list section.
#[must_use]
pub fn encode_get_screen_info_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    root: u32,
    timestamp: u32,
    config_timestamp: u32,
    width: u16,
    height: u16,
    mwidth: u16,
    mheight: u16,
) -> Vec<u8> {
    let n_sizes: u16 = 1;
    let n_rates: u16 = 1;
    let n_info: u16 = n_sizes * 2; // one refresh list per size
    let refresh_record_padded = pad4(2 + 2 * usize::from(n_rates));
    let extra = usize::from(n_sizes) * 8 + usize::from(n_sizes) * refresh_record_padded;
    #[allow(clippy::cast_possible_truncation)]
    let length = (extra / 4) as u32;
    let rotations: u8 = 1; // RR_Rotate_0 only

    let mut out = fixed_reply(byte_order, sequence, rotations, length);
    put(byte_order, &mut out, root);
    put(byte_order, &mut out, timestamp);
    put(byte_order, &mut out, config_timestamp);
    put(byte_order, &mut out, n_sizes);
    put(byte_order, &mut out, 0u16); // sizeID = 0 (current)
    put(byte_order, &mut out, 1u16); // rotation = RR_Rotate_0
    put(byte_order, &mut out, 60u16); // current rate = 60 Hz
    put(byte_order, &mut out, n_info);
    out.extend_from_slice(&[0u8; 2]);
    debug_assert_eq!(out.len(), 32);

    put(byte_order, &mut out, width);
    put(byte_order, &mut out, height);
    put(byte_order, &mut out, mwidth);
    put(byte_order, &mut out, mheight);

    put(byte_order, &mut out, n_rates);
    put(byte_order, &mut out, 60u16);
    pad_vec4(&mut out);

    out
}

/// Encodes a `GetScreenSizeRange` reply (32 bytes total).
pub fn encode_get_screen_size_range_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    min_width: u16,
    min_height: u16,
    max_width: u16,
    max_height: u16,
) -> Vec<u8> {
    let mut out = fixed_reply(byte_order, sequence, 0, 0);
    put(byte_order, &mut out, min_width);
    put(byte_order, &mut out, min_height);
    put(byte_order, &mut out, max_width);
    put(byte_order, &mut out, max_height);
    out.extend_from_slice(&[0u8; 16]);
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encodes a `GetScreenResourcesCurrent` reply.
pub fn encode_get_screen_resources_current_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    resources: &ScreenResources,
) -> Vec<u8> {
    let num_crtcs = resources.crtcs.len();
    let num_outputs = resources.outputs.len();
    let num_modes = resources.modes.len();
    let names_len = resources.mode_names.len();
    let names_padded = pad4(names_len);

    // Extra bytes after the 32-byte header
    let extra = num_crtcs * 4 + num_outputs * 4 + num_modes * 32 + names_padded;
    #[allow(clippy::cast_possible_truncation)]
    let length = (extra / 4) as u32;

    let mut out = fixed_reply(byte_order, sequence, 0, length);
    // bytes 8-11: timestamp
    out.extend_from_slice(&resources.timestamp.to_le_bytes());
    // bytes 12-15: config_timestamp
    out.extend_from_slice(&resources.config_timestamp.to_le_bytes());
    // bytes 16-17: num_crtcs
    #[allow(clippy::cast_possible_truncation)]
    put(byte_order, &mut out, num_crtcs as u16);
    // bytes 18-19: num_outputs
    #[allow(clippy::cast_possible_truncation)]
    put(byte_order, &mut out, num_outputs as u16);
    // bytes 20-21: num_modes
    #[allow(clippy::cast_possible_truncation)]
    put(byte_order, &mut out, num_modes as u16);
    // bytes 22-23: names_len
    #[allow(clippy::cast_possible_truncation)]
    put(byte_order, &mut out, names_len as u16);
    // bytes 24-31: 8 bytes padding
    out.extend_from_slice(&[0u8; 8]);

    // crtcs array
    for &crtc in &resources.crtcs {
        put(byte_order, &mut out, crtc);
    }
    // outputs array
    for &output in &resources.outputs {
        put(byte_order, &mut out, output);
    }
    // mode info structs (xRRModeInfo, each 32 bytes)
    for mode in &resources.modes {
        out.extend_from_slice(&mode.id.to_le_bytes());
        out.extend_from_slice(&mode.width.to_le_bytes());
        out.extend_from_slice(&mode.height.to_le_bytes());
        out.extend_from_slice(&mode.dot_clock.to_le_bytes());
        out.extend_from_slice(&mode.hsync_start.to_le_bytes());
        out.extend_from_slice(&mode.hsync_end.to_le_bytes());
        out.extend_from_slice(&mode.htotal.to_le_bytes());
        out.extend_from_slice(&mode.hskew.to_le_bytes());
        out.extend_from_slice(&mode.vsync_start.to_le_bytes());
        out.extend_from_slice(&mode.vsync_end.to_le_bytes());
        out.extend_from_slice(&mode.vtotal.to_le_bytes());
        out.extend_from_slice(&mode.name_len.to_le_bytes());
        out.extend_from_slice(&mode.mode_flags.to_le_bytes());
    }
    // mode names (padded to 4)
    out.extend_from_slice(&resources.mode_names);
    pad_vec4(&mut out);

    out
}

/// Parameters for encoding a `GetOutputInfo` reply.
pub struct OutputInfoReply<'a> {
    pub timestamp: u32,
    /// CRTC currently driving this output (0 if none).
    pub crtc: u32,
    pub width_mm: u32,
    pub height_mm: u32,
    /// 0 = Connected, 1 = Disconnected, 2 = Unknown.
    pub connection: u8,
    pub subpixel_order: u8,
    pub crtcs: &'a [u32],
    pub modes: &'a [u32],
    pub num_preferred: u16,
    pub clones: &'a [u32],
    pub name: &'a [u8],
}

/// Encodes a `GetOutputInfo` reply.
pub fn encode_get_output_info_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    info: &OutputInfoReply<'_>,
) -> Vec<u8> {
    let timestamp = info.timestamp;
    let crtc = info.crtc;
    let width_mm = info.width_mm;
    let height_mm = info.height_mm;
    let connection = info.connection;
    let subpixel_order = info.subpixel_order;
    let crtcs = info.crtcs;
    let modes = info.modes;
    let num_preferred = info.num_preferred;
    let clones = info.clones;
    let name = info.name;
    let num_crtcs = crtcs.len();
    let num_modes = modes.len();
    let num_clones = clones.len();
    let name_len = name.len();
    let name_padded = pad4(name_len);

    // xRRGetOutputInfoReply (sz=36): connection and subpixelOrder are CARD8 (1 byte each).
    // bytes 24-25: u8+u8, then nCrtcs at 26, nModes at 28, nPreferred at 30 (all in first 32 bytes),
    // nClones at 32, nameLen at 34 (the one extra 4-byte word beyond byte 31).
    // Arrays start at byte 36 (4-byte aligned, no pad needed).
    // length = (4 + crtcs*4 + modes*4 + clones*4 + pad4(name)) / 4
    let extra = 4 + num_crtcs * 4 + num_modes * 4 + num_clones * 4 + name_padded;
    #[allow(clippy::cast_possible_truncation)]
    let length = (extra / 4) as u32;

    let mut out = fixed_reply(byte_order, sequence, 0, length);
    // bytes 8-11: timestamp
    put(byte_order, &mut out, timestamp);
    // bytes 12-15: crtc
    put(byte_order, &mut out, crtc);
    // bytes 16-19: mm_width
    put(byte_order, &mut out, width_mm);
    // bytes 20-23: mm_height
    put(byte_order, &mut out, height_mm);
    // byte 24: connection (CARD8)
    out.push(connection);
    // byte 25: subpixel_order (CARD8)
    out.push(subpixel_order);
    // bytes 26-27: num_crtcs
    #[allow(clippy::cast_possible_truncation)]
    put(byte_order, &mut out, num_crtcs as u16);
    // bytes 28-29: num_modes
    #[allow(clippy::cast_possible_truncation)]
    put(byte_order, &mut out, num_modes as u16);
    // bytes 30-31: num_preferred (Xorg nPreferred prefix count)
    #[allow(clippy::cast_possible_truncation)]
    put(byte_order, &mut out, num_preferred);
    // bytes 32-33: num_clones  (extra word read by _XReply with extra=1)
    #[allow(clippy::cast_possible_truncation)]
    put(byte_order, &mut out, num_clones as u16);
    // bytes 34-35: name_len
    #[allow(clippy::cast_possible_truncation)]
    put(byte_order, &mut out, name_len as u16);
    // no pad: byte 36 is 4-byte aligned, arrays follow immediately

    // crtcs
    for &c in crtcs {
        put(byte_order, &mut out, c);
    }
    // modes
    for &m in modes {
        put(byte_order, &mut out, m);
    }
    // clones
    for &cl in clones {
        put(byte_order, &mut out, cl);
    }
    // name (padded to 4)
    out.extend_from_slice(name);
    pad_vec4(&mut out);

    out
}

/// Parameters for encoding a `GetCrtcInfo` reply.
pub struct CrtcInfoReply<'a> {
    pub timestamp: u32,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    /// Active mode ID (0 if CRTC is disabled).
    pub mode: u32,
    pub rotation: u16,
    pub rotations: u16,
    pub outputs: &'a [u32],
    pub possible: &'a [u32],
}

/// Encodes a `GetCrtcInfo` reply.
pub fn encode_get_crtc_info_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    info: &CrtcInfoReply<'_>,
) -> Vec<u8> {
    let timestamp = info.timestamp;
    let x = info.x;
    let y = info.y;
    let width = info.width;
    let height = info.height;
    let mode = info.mode;
    let rotation = info.rotation;
    let rotations = info.rotations;
    let outputs = info.outputs;
    let possible = info.possible;
    let num_outputs = outputs.len();
    let num_possible = possible.len();

    // Extra bytes after 32-byte header
    let extra = num_outputs * 4 + num_possible * 4;
    #[allow(clippy::cast_possible_truncation)]
    let length = (extra / 4) as u32;

    let mut out = fixed_reply(byte_order, sequence, 0, length);
    // bytes 8-11: timestamp
    put(byte_order, &mut out, timestamp);
    // bytes 12-13: x (i16)
    put(byte_order, &mut out, x);
    // bytes 14-15: y (i16)
    put(byte_order, &mut out, y);
    // bytes 16-17: width
    put(byte_order, &mut out, width);
    // bytes 18-19: height
    put(byte_order, &mut out, height);
    // bytes 20-23: mode
    put(byte_order, &mut out, mode);
    // bytes 24-25: rotation
    put(byte_order, &mut out, rotation);
    // bytes 26-27: rotations
    put(byte_order, &mut out, rotations);
    // bytes 28-29: num_outputs
    #[allow(clippy::cast_possible_truncation)]
    put(byte_order, &mut out, num_outputs as u16);
    // bytes 30-31: num_possible
    #[allow(clippy::cast_possible_truncation)]
    put(byte_order, &mut out, num_possible as u16);

    // outputs
    for &o in outputs {
        put(byte_order, &mut out, o);
    }
    // possible outputs
    for &p in possible {
        put(byte_order, &mut out, p);
    }

    out
}

/// Encodes a `GetCrtcTransform` reply (96 bytes) with identity transforms and no filter.
///
/// Wire layout: standard 8-byte header + pendingTransform(36) + hasTransforms(1)+pad(3) +
/// currentTransform(36) + pad(4) + four u16 filter-length fields.
/// Identity matrix in 16.16 fixed-point: diagonal = 0x0001_0000, off-diagonal = 0.
pub fn encode_get_crtc_transform_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> Vec<u8> {
    const IDENTITY: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x0001_0000];
    let mut out = fixed_reply(byte_order, sequence, 0, 16); // 64 extra bytes = 16 CARD32s
    for &v in &IDENTITY {
        put(byte_order, &mut out, v); // bytes 8-43: pendingTransform
    }
    out.push(0); // byte 44: hasTransforms = false
    out.extend_from_slice(&[0u8; 3]); // bytes 45-47: pad
    for &v in &IDENTITY {
        put(byte_order, &mut out, v); // bytes 48-83: currentTransform
    }
    out.extend_from_slice(&[0u8; 4]); // bytes 84-87: pad
    out.extend_from_slice(&[0u8; 8]); // bytes 88-95: four u16 filter lengths (all 0)
    debug_assert_eq!(out.len(), 96);
    out
}

/// Encodes a `ListOutputProperties` reply with zero properties (32 bytes).
pub fn encode_list_output_properties_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    atoms: &[u32],
) -> Vec<u8> {
    // reply-length = number of trailing ATOMs (each a 4-byte unit).
    let mut out = fixed_reply(byte_order, sequence, 0, atoms.len() as u32);
    put(byte_order, &mut out, atoms.len() as u16); // nAtoms
    out.extend_from_slice(&[0u8; 22]); // pad → 32-byte header
    debug_assert_eq!(out.len(), 32);
    for &atom in atoms {
        put(byte_order, &mut out, atom);
    }
    out
}

/// Encodes a `GetOutputProperty` reply indicating the property does not exist (format=0,
/// type=None, bytes_after=0, num_items=0, no data).
/// `RRGetOutputProperty` reply. `prop_type` is the value's type atom
/// (0 = None), `format` ∈ {0,8,16,32}, `bytes_after` the count of
/// value bytes not returned in this window, and `value` the returned
/// bytes (already windowed by the caller). A None reply is
/// `(prop_type=0, format=0, bytes_after=0, value=&[])`.
pub fn encode_get_output_property_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    prop_type: u32,
    format: u8,
    bytes_after: u32,
    value: &[u8],
) -> Vec<u8> {
    let padded = pad4(value.len());
    // reply-length = value length in 4-byte units (padded).
    let mut out = fixed_reply(byte_order, sequence, format, (padded / 4) as u32);
    put(byte_order, &mut out, prop_type);
    put(byte_order, &mut out, bytes_after);
    let num_items = if format == 0 {
        0
    } else {
        value.len() / (format as usize / 8)
    };
    put(byte_order, &mut out, num_items as u32);
    out.extend_from_slice(&[0u8; 12]); // pad → 32-byte header
    debug_assert_eq!(out.len(), 32);
    out.extend_from_slice(value);
    pad_vec4(&mut out);
    out
}

/// Encodes a `GetPanning` reply (36 bytes) with all-zero panning (no panning configured).
///
/// Wire layout: `status(1) seq(2) length=1(4) timestamp(4) left top width height
/// trackLeft trackTop trackWidth trackHeight borderLeft borderTop borderRight borderBottom`
/// (each of the 12 panning fields is u16/i16).
pub fn encode_get_panning_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    timestamp: u32,
) -> Vec<u8> {
    let mut out = fixed_reply(byte_order, sequence, 0 /* status=Success */, 1);
    put(byte_order, &mut out, timestamp); // bytes 8-11
    out.extend_from_slice(&[0u8; 24]); // 12 × u16 fields, all zero
    debug_assert_eq!(out.len(), 36);
    out
}

/// Encodes a `SetPanning` reply. `status` is one of the RRSetConfig*
/// values; yserver only accepts disabled/no-op panning and returns
/// `SET_CONFIG_FAILED` for non-zero panning because that would require
/// a transformed/composited viewport path.
pub fn encode_set_panning_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    status: u8,
    timestamp: u32,
) -> Vec<u8> {
    let mut out = fixed_reply(byte_order, sequence, status, 0);
    put(byte_order, &mut out, timestamp);
    out.extend_from_slice(&[0u8; 20]);
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encodes a `GetOutputPrimary` reply (32 bytes), returning no primary output.
pub fn encode_get_output_primary_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    output: u32,
) -> Vec<u8> {
    let mut out = fixed_reply(byte_order, sequence, 0, 0);
    put(byte_order, &mut out, output); // bytes 8-11: primary output XID (0 = none)
    out.extend_from_slice(&[0u8; 20]); // pad
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encodes a `GetProviders` reply followed by the provider XID array.
pub fn encode_get_providers_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    timestamp: u32,
    providers: &[u32],
) -> Vec<u8> {
    #[allow(clippy::cast_possible_truncation)]
    let mut out = fixed_reply(byte_order, sequence, 0, providers.len() as u32);
    put(byte_order, &mut out, timestamp); // bytes 8-11
    #[allow(clippy::cast_possible_truncation)]
    put(byte_order, &mut out, providers.len() as u16);
    out.extend_from_slice(&[0u8; 18]);
    debug_assert_eq!(out.len(), 32);
    for &provider in providers {
        put(byte_order, &mut out, provider);
    }
    out
}

pub struct ProviderInfoReply<'a> {
    pub status: u8,
    pub timestamp: u32,
    pub capabilities: u32,
    pub crtcs: &'a [u32],
    pub outputs: &'a [u32],
    pub associated_providers: &'a [u32],
    pub associated_capabilities: &'a [u32],
    pub name: &'a [u8],
}

/// Encodes a RANDR 1.4 `GetProviderInfo` reply.
pub fn encode_get_provider_info_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    info: &ProviderInfoReply<'_>,
) -> Vec<u8> {
    debug_assert_eq!(
        info.associated_providers.len(),
        info.associated_capabilities.len()
    );
    let name_padded = pad4(info.name.len());
    let extra_bytes = (info.crtcs.len()
        + info.outputs.len()
        + info.associated_providers.len()
        + info.associated_capabilities.len())
        * 4
        + name_padded;
    #[allow(clippy::cast_possible_truncation)]
    let mut out = fixed_reply(byte_order, sequence, info.status, (extra_bytes / 4) as u32);
    put(byte_order, &mut out, info.timestamp);
    put(byte_order, &mut out, info.capabilities);
    #[allow(clippy::cast_possible_truncation)]
    put(byte_order, &mut out, info.crtcs.len() as u16);
    #[allow(clippy::cast_possible_truncation)]
    put(byte_order, &mut out, info.outputs.len() as u16);
    #[allow(clippy::cast_possible_truncation)]
    put(byte_order, &mut out, info.associated_providers.len() as u16);
    #[allow(clippy::cast_possible_truncation)]
    put(byte_order, &mut out, info.name.len() as u16);
    out.extend_from_slice(&[0u8; 8]);
    debug_assert_eq!(out.len(), 32);
    for values in [
        info.crtcs,
        info.outputs,
        info.associated_providers,
        info.associated_capabilities,
    ] {
        for &value in values {
            put(byte_order, &mut out, value);
        }
    }
    out.extend_from_slice(info.name);
    pad_vec4(&mut out);
    out
}

/// One monitor descriptor inside a `GetMonitors` reply.
pub struct MonitorInfo<'a> {
    /// Atom ID for the monitor name (0 = anonymous).
    pub name: u32,
    pub primary: bool,
    /// Active server-generated monitors are reported as automatic.
    pub automatic: bool,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub width_mm: u32,
    pub height_mm: u32,
    pub outputs: &'a [u32],
}

/// Encodes a `GetMonitors` reply (RANDR 1.5).
///
/// Wire format per monitor (`xRRMonitorInfo`, 24 bytes fixed + 4*nOutput):
/// `name(4) primary(1) automatic(1) nOutput(2) x(2) y(2) width(2) height(2)
///  widthMM(4) heightMM(4)` followed by output XIDs.
pub fn encode_get_monitors_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    timestamp: u32,
    monitors: &[MonitorInfo<'_>],
) -> Vec<u8> {
    let n_monitors = monitors.len();
    let n_outputs: usize = monitors.iter().map(|m| m.outputs.len()).sum();

    // Total extra bytes after the 32-byte header.
    let extra: usize = monitors.iter().map(|m| 24 + m.outputs.len() * 4).sum();
    #[allow(clippy::cast_possible_truncation)]
    let length = (extra / 4) as u32;

    let mut out = fixed_reply(byte_order, sequence, 0, length);
    // bytes 8-11: timestamp
    put(byte_order, &mut out, timestamp);
    // bytes 12-15: nMonitors
    #[allow(clippy::cast_possible_truncation)]
    put(byte_order, &mut out, n_monitors as u32);
    // bytes 16-19: nOutputs (total across all monitors)
    #[allow(clippy::cast_possible_truncation)]
    put(byte_order, &mut out, n_outputs as u32);
    // bytes 20-31: pad
    out.extend_from_slice(&[0u8; 12]);
    debug_assert_eq!(out.len(), 32);

    for m in monitors {
        #[allow(clippy::cast_possible_truncation)]
        let n_out = m.outputs.len() as u16;
        out.extend_from_slice(&m.name.to_le_bytes()); // 4: name (Atom)
        out.push(u8::from(m.primary)); // 1: primary
        out.push(u8::from(m.automatic)); // 1: automatic
        put(byte_order, &mut out, n_out); // 2: nOutput
        out.extend_from_slice(&m.x.to_le_bytes()); // 2: x
        out.extend_from_slice(&m.y.to_le_bytes()); // 2: y
        out.extend_from_slice(&m.width.to_le_bytes()); // 2: width
        out.extend_from_slice(&m.height.to_le_bytes()); // 2: height
        out.extend_from_slice(&m.width_mm.to_le_bytes()); // 4: widthInMillimeters
        out.extend_from_slice(&m.height_mm.to_le_bytes()); // 4: heightInMillimeters
        for &oid in m.outputs {
            put(byte_order, &mut out, oid);
        }
    }

    out
}

/// Encodes a `GetCrtcGammaSize` reply (32 bytes, `size` = 0 means no gamma support).
pub fn encode_get_crtc_gamma_size_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    size: u16,
) -> Vec<u8> {
    let mut out = fixed_reply(byte_order, sequence, 0, 0);
    put(byte_order, &mut out, size); // bytes 8-9: size
    out.extend_from_slice(&[0u8; 22]); // bytes 10-31: pad
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encodes a `GetCrtcGamma` reply as a 32-byte fixed header followed by
/// `red`, `green`, then `blue` `CARD16` arrays, padded to 4 bytes.
pub fn encode_get_crtc_gamma_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    red: &[u16],
    green: &[u16],
    blue: &[u16],
) -> Vec<u8> {
    debug_assert_eq!(red.len(), green.len());
    debug_assert_eq!(red.len(), blue.len());
    let size = u16::try_from(red.len()).unwrap_or(u16::MAX);
    let payload_bytes = red.len().saturating_mul(3).saturating_mul(2);
    let length = u32::try_from((payload_bytes + 3) >> 2).unwrap_or(u32::MAX);
    let mut out = fixed_reply(byte_order, sequence, 0, length);
    put(byte_order, &mut out, size); // bytes 8-9: size
    out.extend_from_slice(&[0u8; 22]); // bytes 10-31: pad
    for channel in [red, green, blue] {
        for &entry in channel {
            put(byte_order, &mut out, entry);
        }
    }
    pad_vec4(&mut out);
    out
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScreenChangeNotify {
    pub timestamp: u32,
    pub config_timestamp: u32,
    pub root: u32,
    pub request_window: u32,
    pub width: u16,
    pub height: u16,
    pub width_mm: u16,
    pub height_mm: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CrtcChangeNotify {
    pub timestamp: u32,
    pub request_window: u32,
    pub crtc: u32,
    pub mode: u32,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputChangeNotify {
    pub timestamp: u32,
    pub config_timestamp: u32,
    pub request_window: u32,
    pub output: u32,
    pub crtc: u32,
    pub mode: u32,
    pub connection: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderChangeNotify {
    pub timestamp: u32,
    pub request_window: u32,
    pub provider: u32,
}

#[must_use]
pub fn encode_screen_change_notify_event(
    byte_order: ClientByteOrder,
    first_event: u8,
    sequence: SequenceNumber,
    event: ScreenChangeNotify,
) -> [u8; 32] {
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    buf.push(first_event + EVENT_SCREEN_CHANGE_NOTIFY);
    buf.push(ROTATION_ROTATE_0 as u8);
    put(byte_order, &mut buf, sequence.0);
    put(byte_order, &mut buf, event.timestamp);
    put(byte_order, &mut buf, event.config_timestamp);
    put(byte_order, &mut buf, event.root);
    put(byte_order, &mut buf, event.request_window);
    put(byte_order, &mut buf, 0u16);
    put(byte_order, &mut buf, SUBPIXEL_UNKNOWN);
    put(byte_order, &mut buf, event.width);
    put(byte_order, &mut buf, event.height);
    put(byte_order, &mut buf, event.width_mm);
    put(byte_order, &mut buf, event.height_mm);
    buf.try_into().expect("32-byte event")
}

#[must_use]
pub fn encode_crtc_change_notify_event(
    byte_order: ClientByteOrder,
    first_event: u8,
    sequence: SequenceNumber,
    event: CrtcChangeNotify,
) -> [u8; 32] {
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    buf.push(first_event + EVENT_NOTIFY);
    buf.push(NOTIFY_CRTC_CHANGE);
    put(byte_order, &mut buf, sequence.0);
    put(byte_order, &mut buf, event.timestamp);
    put(byte_order, &mut buf, event.request_window);
    put(byte_order, &mut buf, event.crtc);
    put(byte_order, &mut buf, event.mode);
    put(byte_order, &mut buf, ROTATION_ROTATE_0);
    // 2 bytes of pad before x/y per spec (CRTC change notify is 32 bytes total).
    buf.extend_from_slice(&[0u8; 2]);
    put(byte_order, &mut buf, event.x);
    put(byte_order, &mut buf, event.y);
    put(byte_order, &mut buf, event.width);
    put(byte_order, &mut buf, event.height);
    buf.try_into().expect("32-byte event")
}

#[must_use]
pub fn encode_output_change_notify_event(
    byte_order: ClientByteOrder,
    first_event: u8,
    sequence: SequenceNumber,
    event: OutputChangeNotify,
) -> [u8; 32] {
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    buf.push(first_event + EVENT_NOTIFY);
    buf.push(NOTIFY_OUTPUT_CHANGE);
    put(byte_order, &mut buf, sequence.0);
    put(byte_order, &mut buf, event.timestamp);
    put(byte_order, &mut buf, event.config_timestamp);
    put(byte_order, &mut buf, event.request_window);
    put(byte_order, &mut buf, event.output);
    put(byte_order, &mut buf, event.crtc);
    put(byte_order, &mut buf, event.mode);
    put(byte_order, &mut buf, ROTATION_ROTATE_0);
    buf.push(event.connection);
    buf.push(SUBPIXEL_UNKNOWN as u8);
    buf.try_into().expect("32-byte event")
}

#[must_use]
pub fn encode_provider_change_notify_event(
    byte_order: ClientByteOrder,
    first_event: u8,
    sequence: SequenceNumber,
    event: ProviderChangeNotify,
) -> [u8; 32] {
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    buf.push(first_event + EVENT_NOTIFY);
    buf.push(NOTIFY_PROVIDER_CHANGE);
    put(byte_order, &mut buf, sequence.0);
    put(byte_order, &mut buf, event.timestamp);
    put(byte_order, &mut buf, event.request_window);
    put(byte_order, &mut buf, event.provider);
    buf.extend_from_slice(&[0u8; 16]);
    buf.try_into().expect("32-byte event")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Parser tests ──────────────────────────────────────────────────────────

    #[test]
    fn parse_query_version_short_body_returns_none() {
        assert!(parse_query_version(&[]).is_none());
        assert!(parse_query_version(&[0u8; 7]).is_none());
    }

    #[test]
    fn parse_set_screen_size_roundtrip() {
        // window=0x200, 2560x1440, 677x381 mm — little-endian.
        let body = [
            0x00, 0x02, 0x00, 0x00, // window
            0x00, 0x0a, // width = 2560
            0xa0, 0x05, // height = 1440
            0xa5, 0x02, 0x00, 0x00, // mm_width = 677
            0x7d, 0x01, 0x00, 0x00, // mm_height = 381
        ];
        let r = parse_set_screen_size_request(&body).expect("valid");
        assert_eq!(
            r,
            SetScreenSizeRequest {
                window: 0x200,
                width: 2560,
                height: 1440,
                mm_width: 677,
                mm_height: 381,
            }
        );
        // Short body rejected.
        assert!(parse_set_screen_size_request(&body[..15]).is_none());
    }

    #[test]
    fn parse_query_version_round_trip() {
        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_le_bytes()); // major
        body.extend_from_slice(&2u32.to_le_bytes()); // minor
        let req = parse_query_version(&body).unwrap();
        assert_eq!(req, QueryVersionRequest { major: 1, minor: 2 });
    }

    #[test]
    fn parse_output_request_round_trip() {
        let mut body = Vec::new();
        body.extend_from_slice(&42u32.to_le_bytes()); // output
        body.extend_from_slice(&1000u32.to_le_bytes()); // config_timestamp
        let req = parse_output_request(&body).unwrap();
        assert_eq!(
            req,
            OutputRequest {
                output: 42,
                config_timestamp: 1000,
            }
        );
    }

    #[test]
    fn parse_output_request_short_body_returns_none() {
        assert!(parse_output_request(&[]).is_none());
        assert!(parse_output_request(&[0u8; 7]).is_none());
    }

    #[test]
    fn parse_provider_requests_round_trip() {
        let mut info = Vec::new();
        info.extend_from_slice(&0x11u32.to_le_bytes());
        info.extend_from_slice(&0x22u32.to_le_bytes());
        assert_eq!(
            parse_provider_info_request(&info),
            Some(ProviderInfoRequest {
                provider: 0x11,
                config_timestamp: 0x22,
            })
        );

        let mut relationship = info.clone();
        relationship[4..8].copy_from_slice(&0x33u32.to_le_bytes());
        relationship.extend_from_slice(&0x44u32.to_le_bytes());
        assert_eq!(
            parse_set_provider_offload_sink_request(&relationship),
            Some(SetProviderOffloadSinkRequest {
                provider: 0x11,
                sink_provider: 0x33,
                config_timestamp: 0x44,
            })
        );
        assert_eq!(
            parse_set_provider_output_source_request(&relationship),
            Some(SetProviderOutputSourceRequest {
                provider: 0x11,
                source_provider: 0x33,
                config_timestamp: 0x44,
            })
        );
        assert!(parse_provider_info_request(&info[..7]).is_none());
        assert!(parse_set_provider_offload_sink_request(&relationship[..11]).is_none());
        assert!(parse_set_provider_output_source_request(&relationship[..11]).is_none());
    }

    // ── Reply size tests ──────────────────────────────────────────────────────

    #[test]
    fn encode_query_version_reply_shape() {
        let buf =
            encode_query_version_reply(ClientByteOrder::LittleEndian, SequenceNumber(0xABCD), 1, 2);
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 1); // reply code
        assert_eq!(&buf[2..4], &0xABCDu16.to_le_bytes()); // sequence
        assert_eq!(&buf[8..12], &1u32.to_le_bytes()); // major
        assert_eq!(&buf[12..16], &2u32.to_le_bytes()); // minor
    }

    #[test]
    fn encode_get_screen_size_range_reply_shape() {
        let buf = encode_get_screen_size_range_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(1),
            320,
            240,
            3840,
            2160,
        );
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 1);
        assert_eq!(&buf[8..10], &320u16.to_le_bytes());
        assert_eq!(&buf[10..12], &240u16.to_le_bytes());
        assert_eq!(&buf[12..14], &3840u16.to_le_bytes());
        assert_eq!(&buf[14..16], &2160u16.to_le_bytes());
    }

    #[test]
    fn encode_get_screen_resources_current_reply_shape() {
        let mode_name = b"800x600";
        let resources = ScreenResources {
            timestamp: 100,
            config_timestamp: 200,
            crtcs: vec![0x10],
            outputs: vec![0x20],
            modes: vec![ModeInfo {
                id: 1,
                width: 800,
                height: 600,
                dot_clock: 40_000_000,
                hsync_start: 840,
                hsync_end: 968,
                htotal: 1056,
                hskew: 0,
                vsync_start: 601,
                vsync_end: 605,
                vtotal: 628,
                name_len: mode_name.len() as u16,
                mode_flags: 0,
            }],
            mode_names: mode_name.to_vec(),
        };

        let buf = encode_get_screen_resources_current_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(5),
            &resources,
        );

        // 32 header + 4 (1 crtc) + 4 (1 output) + 32 (1 mode info) + 8 ("800x600" = 7 bytes, padded to 8)
        let expected_len = 32 + 4 + 4 + 32 + 8;
        assert_eq!(buf.len(), expected_len);
        assert_eq!(buf[0], 1);
        // length field in 4-byte units after first 32 bytes
        let length_field = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        assert_eq!(length_field, ((expected_len - 32) / 4) as u32);
        // timestamp
        assert_eq!(&buf[8..12], &100u32.to_le_bytes());
        // config_timestamp
        assert_eq!(&buf[12..16], &200u32.to_le_bytes());
        // num_crtcs
        assert_eq!(&buf[16..18], &1u16.to_le_bytes());
        // num_outputs
        assert_eq!(&buf[18..20], &1u16.to_le_bytes());
        // num_modes
        assert_eq!(&buf[20..22], &1u16.to_le_bytes());
    }

    #[test]
    fn encode_get_output_info_reply_shape() {
        let crtcs = [2u32];
        let modes = [3u32];
        let name = b"ynest-0";
        let buf = encode_get_output_info_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(7),
            &OutputInfoReply {
                timestamp: 42,
                crtc: 2,
                width_mm: 211,
                height_mm: 158,
                connection: 0,
                subpixel_order: 0,
                crtcs: &crtcs,
                modes: &modes,
                num_preferred: 1,
                clones: &[],
                name,
            },
        );
        // 32 header + 4 (nClones+nameLen extra word) + 4 (1 crtc) + 4 (1 mode) + 8 (7-byte name padded to 8) = 52
        let expected_len = 32 + 4 + 4 + 4 + 8;
        assert_eq!(buf.len(), expected_len);
        assert_eq!(buf[0], 1);
        let length_field = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        assert_eq!(length_field, ((expected_len - 32) / 4) as u32);
        // crtc XID in fixed header at bytes 12-15
        assert_eq!(&buf[12..16], &2u32.to_le_bytes());
        // connection (CARD8) at byte 24, subpixelOrder at byte 25
        assert_eq!(buf[24], 0u8); // Connected
        assert_eq!(buf[25], 0u8); // subpixel Unknown
        // num_crtcs at bytes 26-27
        assert_eq!(&buf[26..28], &1u16.to_le_bytes());
        // num_modes at bytes 28-29
        assert_eq!(&buf[28..30], &1u16.to_le_bytes());
        // num_preferred at bytes 30-31
        assert_eq!(&buf[30..32], &1u16.to_le_bytes());
        // num_clones at bytes 32-33
        assert_eq!(&buf[32..34], &0u16.to_le_bytes());
        // name_len at bytes 34-35
        assert_eq!(&buf[34..36], &7u16.to_le_bytes());
        // CRTCs array starts at byte 36
        assert_eq!(&buf[36..40], &2u32.to_le_bytes());
        // modes array at byte 40
        assert_eq!(&buf[40..44], &3u32.to_le_bytes());
        // name at byte 44
        assert_eq!(&buf[44..51], b"ynest-0");
    }

    #[test]
    fn encode_get_providers_reply_lists_provider_xids() {
        let buf = encode_get_providers_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(12),
            99,
            &[0x10, 0x20],
        );
        assert_eq!(buf.len(), 40);
        assert_eq!(&buf[4..8], &2u32.to_le_bytes());
        assert_eq!(&buf[8..12], &99u32.to_le_bytes());
        assert_eq!(&buf[12..14], &2u16.to_le_bytes());
        assert_eq!(&buf[32..36], &0x10u32.to_le_bytes());
        assert_eq!(&buf[36..40], &0x20u32.to_le_bytes());
    }

    #[test]
    fn encode_get_provider_info_reply_layout() {
        let buf = encode_get_provider_info_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(13),
            &ProviderInfoReply {
                status: SET_CONFIG_SUCCESS,
                timestamp: 101,
                capabilities: PROVIDER_CAPABILITY_SOURCE_OUTPUT | PROVIDER_CAPABILITY_SINK_OFFLOAD,
                crtcs: &[0x21],
                outputs: &[0x31, 0x32],
                associated_providers: &[0x41],
                associated_capabilities: &[PROVIDER_CAPABILITY_SINK_OUTPUT],
                name: b"card1",
            },
        );
        // Four CARD32 arrays entries total = 20 bytes; name pads 5 -> 8.
        assert_eq!(buf.len(), 60);
        assert_eq!(&buf[4..8], &7u32.to_le_bytes());
        assert_eq!(&buf[8..12], &101u32.to_le_bytes());
        assert_eq!(&buf[16..18], &1u16.to_le_bytes());
        assert_eq!(&buf[18..20], &2u16.to_le_bytes());
        assert_eq!(&buf[20..22], &1u16.to_le_bytes());
        assert_eq!(&buf[22..24], &5u16.to_le_bytes());
        assert_eq!(&buf[32..36], &0x21u32.to_le_bytes());
        assert_eq!(&buf[36..40], &0x31u32.to_le_bytes());
        assert_eq!(&buf[40..44], &0x32u32.to_le_bytes());
        assert_eq!(&buf[44..48], &0x41u32.to_le_bytes());
        assert_eq!(&buf[48..52], &PROVIDER_CAPABILITY_SINK_OUTPUT.to_le_bytes());
        assert_eq!(&buf[52..57], b"card1");
        assert!(buf[57..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn encode_get_output_property_empty_reply_shape() {
        // None reply: prop_type=0, format=0, no value.
        let buf = encode_get_output_property_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(9),
            0,
            0,
            0,
            &[],
        );

        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 1);
        assert_eq!(buf[1], 0); // format = 0
        assert_eq!(&buf[2..4], &9u16.to_le_bytes());
        assert_eq!(&buf[4..8], &0u32.to_le_bytes());
        assert!(buf[8..].iter().all(|b| *b == 0));
    }

    #[test]
    fn parse_get_output_property_full_request() {
        // output, property, type, long-offset, long-length, delete, pending, pad
        let mut body = Vec::new();
        body.extend_from_slice(&0x11u32.to_le_bytes());
        body.extend_from_slice(&0x7bu32.to_le_bytes()); // EDID atom
        body.extend_from_slice(&0u32.to_le_bytes()); // AnyPropertyType
        body.extend_from_slice(&2u32.to_le_bytes()); // long-offset (32-bit units)
        body.extend_from_slice(&8u32.to_le_bytes()); // long-length
        body.push(0); // delete
        body.push(1); // pending
        body.extend_from_slice(&[0, 0]); // pad
        let req = parse_get_output_property_request(&body).expect("parse");
        assert_eq!(
            req,
            GetOutputPropertyRequest {
                output: 0x11,
                property: 0x7b,
                prop_type: 0,
                long_offset: 2,
                long_length: 8,
                delete: false,
                pending: true,
            }
        );
        assert!(parse_get_output_property_request(&body[..19]).is_none());
    }

    #[test]
    fn encode_list_output_properties_lists_atoms() {
        let buf = encode_list_output_properties_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(3),
            &[0x7b, 0x97, 0x96],
        );
        // 32 header + 3 atoms * 4 = 44
        assert_eq!(buf.len(), 44);
        assert_eq!(&buf[4..8], &3u32.to_le_bytes()); // length = nAtoms units
        assert_eq!(&buf[8..10], &3u16.to_le_bytes()); // nAtoms
        assert_eq!(&buf[32..36], &0x7bu32.to_le_bytes());
        assert_eq!(&buf[36..40], &0x97u32.to_le_bytes());
        assert_eq!(&buf[40..44], &0x96u32.to_le_bytes());
    }

    #[test]
    fn encode_get_output_property_edid_value_and_window() {
        // A 3-byte value returned with 5 bytes still to come.
        let buf = encode_get_output_property_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(5),
            31, // INTEGER
            8,
            5,
            &[0xDE, 0xAD, 0xBE],
        );
        assert_eq!(buf[1], 8); // format
        assert_eq!(&buf[8..12], &31u32.to_le_bytes()); // type
        assert_eq!(&buf[12..16], &5u32.to_le_bytes()); // bytes_after
        assert_eq!(&buf[16..20], &3u32.to_le_bytes()); // num_items (3 bytes / 1)
        assert_eq!(&buf[32..35], &[0xDE, 0xAD, 0xBE]);
        assert_eq!(buf.len(), 36); // 32 + 3 padded to 4
        assert_eq!(&buf[4..8], &1u32.to_le_bytes()); // length = 1 unit
    }

    #[test]
    fn parse_query_output_property_request() {
        let body = [
            0x01, 0x00, 0x00, 0x00, // output
            0x02, 0x00, 0x00, 0x00, // property
        ];
        let req = parse_output_property_request(&body).unwrap();
        assert_eq!(req.output, 1);
        assert_eq!(req.property, 2);
    }

    #[test]
    fn parse_crtc_id_request_reads_crtc_only() {
        let body = [0x02, 0x00, 0x00, 0x00];
        let req = parse_crtc_id_request(&body).expect("crtc id request");
        assert_eq!(req.crtc, 2);
    }

    #[test]
    fn encode_get_crtc_gamma_reply_layout() {
        let red = [1u16, 2];
        let green = [3u16, 4];
        let blue = [5u16, 6];
        let buf = encode_get_crtc_gamma_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(8),
            &red,
            &green,
            &blue,
        );

        assert_eq!(buf.len(), 44);
        assert_eq!(buf[0], 1);
        assert_eq!(&buf[2..4], &8u16.to_le_bytes());
        assert_eq!(&buf[4..8], &3u32.to_le_bytes());
        assert_eq!(&buf[8..10], &2u16.to_le_bytes());
        assert_eq!(&buf[32..36], &[1, 0, 2, 0]);
        assert_eq!(&buf[36..40], &[3, 0, 4, 0]);
        assert_eq!(&buf[40..44], &[5, 0, 6, 0]);
    }

    #[test]
    fn encode_get_monitors_single_monitor_shape() {
        let outputs = [0x20u32];
        let buf = encode_get_monitors_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(10),
            123,
            &[MonitorInfo {
                name: 0,
                primary: true,
                automatic: true,
                x: 0,
                y: 0,
                width: 800,
                height: 600,
                width_mm: 211,
                height_mm: 158,
                outputs: &outputs,
            }],
        );

        assert_eq!(buf.len(), 60);
        assert_eq!(buf[0], 1);
        assert_eq!(&buf[2..4], &10u16.to_le_bytes());
        assert_eq!(&buf[4..8], &7u32.to_le_bytes());
        assert_eq!(&buf[8..12], &123u32.to_le_bytes());
        assert_eq!(&buf[12..16], &1u32.to_le_bytes()); // nMonitors
        assert_eq!(&buf[16..20], &1u32.to_le_bytes()); // nOutputs
        assert_eq!(buf[36], 1); // primary
        assert_eq!(buf[37], 1); // automatic
        assert_eq!(&buf[40..42], &0i16.to_le_bytes());
        assert_eq!(&buf[44..46], &800u16.to_le_bytes());
        assert_eq!(&buf[56..60], &0x20u32.to_le_bytes());
    }

    #[test]
    fn screen_change_notify_event_shape() {
        let event = encode_screen_change_notify_event(
            ClientByteOrder::LittleEndian,
            89,
            SequenceNumber(11),
            ScreenChangeNotify {
                timestamp: 100,
                config_timestamp: 101,
                root: 0x100,
                request_window: 0x101,
                width: 1024,
                height: 768,
                width_mm: 271,
                height_mm: 203,
            },
        );

        assert_eq!(event.len(), 32);
        assert_eq!(event[0], 89);
        assert_eq!(event[1], 1);
        assert_eq!(&event[2..4], &11u16.to_le_bytes());
        assert_eq!(&event[4..8], &100u32.to_le_bytes());
        assert_eq!(&event[12..16], &0x100u32.to_le_bytes());
        assert_eq!(&event[24..26], &1024u16.to_le_bytes());
        assert_eq!(&event[30..32], &203u16.to_le_bytes());
    }

    #[test]
    fn crtc_change_notify_event_shape() {
        let event = encode_crtc_change_notify_event(
            ClientByteOrder::LittleEndian,
            89,
            SequenceNumber(12),
            CrtcChangeNotify {
                timestamp: 200,
                request_window: 0x100,
                crtc: 2,
                mode: 3,
                x: 4,
                y: 5,
                width: 1280,
                height: 720,
            },
        );

        assert_eq!(event[0], 90);
        assert_eq!(event[1], 0);
        assert_eq!(&event[4..8], &200u32.to_le_bytes());
        assert_eq!(&event[12..16], &2u32.to_le_bytes());
        assert_eq!(&event[20..22], &1u16.to_le_bytes());
        assert_eq!(&event[28..30], &1280u16.to_le_bytes());
    }

    #[test]
    fn output_change_notify_event_shape() {
        let event = encode_output_change_notify_event(
            ClientByteOrder::LittleEndian,
            89,
            SequenceNumber(13),
            OutputChangeNotify {
                timestamp: 300,
                config_timestamp: 301,
                request_window: 0x100,
                output: 1,
                crtc: 2,
                mode: 3,
                connection: CONNECTION_CONNECTED,
            },
        );

        assert_eq!(event[0], 90);
        assert_eq!(event[1], 1);
        assert_eq!(&event[8..12], &301u32.to_le_bytes());
        assert_eq!(&event[16..20], &1u32.to_le_bytes());
        assert_eq!(event[30], CONNECTION_CONNECTED);
        assert_eq!(event[31], 0);
    }

    #[test]
    fn output_change_notify_encodes_disconnected_state() {
        let event = encode_output_change_notify_event(
            ClientByteOrder::LittleEndian,
            89,
            SequenceNumber(13),
            OutputChangeNotify {
                timestamp: 300,
                config_timestamp: 301,
                request_window: 0x100,
                output: 1,
                crtc: 0,
                mode: 0,
                connection: CONNECTION_DISCONNECTED,
            },
        );

        assert_eq!(event[30], CONNECTION_DISCONNECTED);
    }

    #[test]
    fn provider_change_notify_event_shape() {
        let event = encode_provider_change_notify_event(
            ClientByteOrder::LittleEndian,
            89,
            SequenceNumber(14),
            ProviderChangeNotify {
                timestamp: 400,
                request_window: 0x100,
                provider: 11,
            },
        );

        assert_eq!(event[0], 90);
        assert_eq!(event[1], NOTIFY_PROVIDER_CHANGE);
        assert_eq!(&event[2..4], &14u16.to_le_bytes());
        assert_eq!(&event[4..8], &400u32.to_le_bytes());
        assert_eq!(&event[8..12], &0x100u32.to_le_bytes());
        assert_eq!(&event[12..16], &11u32.to_le_bytes());
        assert!(event[16..].iter().all(|byte| *byte == 0));
    }
}
