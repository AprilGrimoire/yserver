//! GLX wire-protocol decoders/encoders (Phase 4.2.5).
//!
//! Server-side GLX scope is *identification + bookkeeping*. libGLX_mesa
//! loads, hits these requests, gets enough metadata to identify our
//! DRI3 backend, then renders client-side direct-to-GPU. Indirect
//! rendering (server-side GL execution) is explicitly out of scope —
//! design §1 "Out of scope".
//!
//! Opcode numbering follows `glxproto.h`. Indirect-rendering opcodes
//! 1..=198 are stubbed with `GLXBadRequest` at the dispatcher layer.

use super::{
    ClientByteOrder, SequenceNumber,
    wire::{write_u16, write_u32},
};

// Identification + setup (always-handled minor opcodes).
pub const RENDER: u8 = 1;
pub const QUERY_VERSION: u8 = 7;
pub const WAIT_GL: u8 = 8;
pub const WAIT_X: u8 = 9;
pub const QUERY_SERVER_STRING: u8 = 19;
pub const CLIENT_INFO: u8 = 20;

// Vendor-private dispatch (legacy).
pub const VENDOR_PRIVATE: u8 = 16;
pub const VENDOR_PRIVATE_WITH_REPLY: u8 = 17;

// Visual / FBConfig enumeration.
pub const GET_VISUAL_CONFIGS: u8 = 14;
pub const GET_FB_CONFIGS: u8 = 21;

// Context lifecycle.
pub const CREATE_CONTEXT: u8 = 3;
pub const DESTROY_CONTEXT: u8 = 4;
pub const MAKE_CURRENT: u8 = 5;
pub const IS_DIRECT: u8 = 6;
pub const SWAP_BUFFERS: u8 = 11;
pub const COPY_CONTEXT: u8 = 10;
pub const CREATE_NEW_CONTEXT: u8 = 24;
pub const CREATE_CONTEXT_ATTRIBS_ARB: u8 = 34; // GLX_ARB_create_context
pub const QUERY_EXTENSIONS_STRING: u8 = 18;
pub const QUERY_CONTEXT: u8 = 25;
pub const MAKE_CONTEXT_CURRENT: u8 = 26;
pub const SET_CLIENT_INFO_ARB: u8 = 33;
pub const SET_CLIENT_INFO_2_ARB: u8 = 35;

// GLX 1.3 drawable lifecycle (CreateGLXWindow / DestroyGLXWindow /
// CreateGLXPbuffer / DestroyGLXPbuffer / GetDrawableAttributes /
// ChangeDrawableAttributes).
pub const CREATE_PIXMAP: u8 = 22;
pub const DESTROY_PIXMAP: u8 = 23;
pub const CREATE_PBUFFER: u8 = 27;
pub const DESTROY_PBUFFER: u8 = 28;
pub const GET_DRAWABLE_ATTRIBUTES: u8 = 29;
pub const CHANGE_DRAWABLE_ATTRIBUTES: u8 = 30;
pub const CREATE_WINDOW: u8 = 31;
pub const DELETE_WINDOW: u8 = 32;

pub const MAJOR_VERSION: u32 = 1;
pub const MINOR_VERSION: u32 = 4;

/// GLX error codes per /usr/share/xcb/glx.xml `errorcopy` entries.
/// Numbers are extension-relative; the dispatcher resolves the
/// absolute code via `nested.rs::GLX_FIRST_ERROR + N`.
///
/// `BadRenderRequest` is the canonical reply for an indirect-rendering
/// minor opcode the server doesn't implement; `UnsupportedPrivateRequest`
/// is for `VendorPrivate`/`VendorPrivateWithReply` we don't handle.
/// `BadPixmap` (3) is returned when `glXCreatePixmap` is called with an
/// XID that does not name a valid X pixmap — matches `/usr/include/GL/glxproto.h`
/// `GLXBadPixmap = 3` and Xorg `glx/glxcmds.c:185`.
pub const ERROR_GLX_BAD_PIXMAP: u8 = 3;
pub const ERROR_GLX_BAD_RENDER_REQUEST: u8 = 6;
pub const ERROR_GLX_UNSUPPORTED_PRIVATE_REQUEST: u8 = 8;

/// VendorPrivate codes for `GLX_EXT_texture_from_pixmap` indirect path.
/// Values from `glx/glxext.h` (`GLX_BIND_TEX_IMAGE_EXT` / `GLX_RELEASE_TEX_IMAGE_EXT`).
pub const VENDOR_CODE_BIND_TEX_IMAGE: u32 = 1330;
pub const VENDOR_CODE_RELEASE_TEX_IMAGE: u32 = 1331;

/// VendorPrivate(WithReply) opcodes for `GLX_SGIX_fbconfig`.
/// Values from `/usr/include/GL/glxproto.h` `X_GLXvop_*` defines.
///
/// ```text
/// #define X_GLXvop_GetFBConfigsSGIX               65540
/// #define X_GLXvop_CreateContextWithConfigSGIX    65541
/// #define X_GLXvop_CreateGLXPixmapWithConfigSGIX  65542
/// ```
pub const VENDOR_CODE_GET_FB_CONFIGS_SGIX: u32 = 65540;
pub const VENDOR_CODE_CREATE_CONTEXT_WITH_CONFIG_SGIX: u32 = 65541;
pub const VENDOR_CODE_CREATE_GLX_PIXMAP_WITH_CONFIG_SGIX: u32 = 65542;

/// Buffer token for `glXBindTexImageEXT`. Confirmed against
/// `/usr/include/GL/glxext.h`: `#define GLX_FRONT_LEFT_EXT 0x20DE`.
/// The indirect bind path accepts any buffer value (single front buffer);
/// this constant is exported for tests / documentation.
pub const GLX_FRONT_LEFT_EXT: u32 = 0x20DE;

/// Parsed VendorPrivate / VendorPrivateWithReply body for
/// `BindTexImageEXT` (1330) and `ReleaseTexImageEXT` (1331).
///
/// Wire layout (little-endian u32 quads):
/// ```text
/// [0..4]   vendor_code   (u32)
/// [4..8]   context_tag   (u32, informational — indirect GLX context id)
/// [8..12]  glx_drawable  (u32, GLXPixmap XID)
/// [12..16] buffer        (u32, e.g. GLX_FRONT_LEFT_EXT — informational)
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VendorPrivateTexImageRequest {
    pub vendor_code: u32,
    pub context_tag: u32,
    pub glx_drawable: u32,
    pub buffer: u32,
}

/// Parse the body of a VendorPrivate request for `BindTexImageEXT` or
/// `ReleaseTexImageEXT`. Returns `None` if the body is shorter than 16 bytes.
#[must_use]
pub fn parse_vendor_private_tex_image(body: &[u8]) -> Option<VendorPrivateTexImageRequest> {
    if body.len() < 16 {
        return None;
    }
    Some(VendorPrivateTexImageRequest {
        vendor_code: read_u32_le(body),
        context_tag: read_u32_le(&body[4..]),
        glx_drawable: read_u32_le(&body[8..]),
        buffer: read_u32_le(&body[12..]),
    })
}

fn read_u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[must_use]
pub fn parse_query_version(body: &[u8]) -> Option<(u32, u32)> {
    if body.len() < 8 {
        return None;
    }
    Some((read_u32_le(body), read_u32_le(&body[4..])))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueryServerStringRequest {
    pub screen: u32,
    pub name: u32,
}

/// `GLX_VENDOR` / `GLX_VERSION` / `GLX_EXTENSIONS` constants per
/// `glxtokens.h`. Used by `QueryServerString` to identify which
/// string the client wants.
pub const STRING_VENDOR: u32 = 1;
pub const STRING_VERSION: u32 = 2;
pub const STRING_EXTENSIONS: u32 = 3;

/// `GLX_VENDOR_NAMES_EXT` (`glxext.h`, 0x20F6) — the name libglvnd's
/// vendor-neutral dispatch passes to `glXQueryServerString` to learn
/// which client GLX vendor library drives this screen. Gated behind the
/// server advertising `GLX_EXT_libglvnd` in its extension string: only
/// then does libglvnd issue this query (HW-verified by tracing glxinfo
/// against Xorg). Without it, libglvnd falls back to a built-in default
/// vendor — which resolves to mesa on most setups but to *nothing* on
/// Asahi/ALARM, so `glXQueryExtensionsString` returns NULL and
/// cogl/clutter (Cinnamon) crashes in `cogl_renderer_connect`.
pub const VENDOR_NAMES_EXT: u32 = 0x20F6;

/// The client GLX vendor library name yserver reports for
/// `GLX_VENDOR_NAMES_EXT` — Mesa's `libGLX_mesa.so` drives our DRI3
/// screens. Matches what Xorg returns for the same query.
pub const VENDOR_NAMES: &str = "mesa";

/// The GLX extensions yserver advertises. Returned for BOTH
/// `glXQueryServerString(GLX_EXTENSIONS)` (opcode 19, `QUERY_SERVER_STRING`,
/// `STRING_EXTENSIONS`) and `glXQueryExtensionsString` (opcode 18,
/// `QUERY_EXTENSIONS_STRING`). They must stay identical: ANGLE/Chromium
/// queries the *server* string (opcode 19) and refuses to create a GLES
/// context if `GLX_ARB_create_context` is absent — an empty server string
/// there breaks Chromium even though opcode 18 carried the list.
///
/// Only advertise what the server actually backs: libGL computes the
/// effective extension list as client ∩ server, so every entry here changes
/// client behavior. `GLX_INTEL_swap_event` was advertised without the
/// `glXSelectEvent`/`GLX_BufferSwapComplete` plumbing behind it — cogl
/// (Cinnamon/muffin) then parked its frame clock waiting for swap events
/// that never arrived, freezing the stage after one black frame.
///
/// `GLX_EXT_libglvnd` is load-bearing: its presence is what makes
/// libglvnd issue the `GLX_VENDOR_NAMES_EXT` (0x20F6) query (see
/// [`VENDOR_NAMES_EXT`]) instead of guessing a default vendor. Xorg
/// advertises it; without it libglvnd's fallback returns no vendor on
/// Asahi/ALARM → NULL `glXQueryExtensionsString` → Cinnamon/cogl SIGSEGV.
pub const SERVER_EXTENSIONS: &str = "GLX_ARB_create_context GLX_ARB_create_context_profile \
    GLX_EXT_create_context_es2_profile GLX_EXT_swap_control \
    GLX_ARB_fbconfig_float GLX_EXT_visual_info \
    GLX_EXT_visual_rating GLX_EXT_import_context GLX_EXT_libglvnd";

/// Extension token appended to the advertised extension string when the
/// backend can allocate and export a BGRA8 dma-buf (probed once at init).
pub const TFP_EXTENSION: &str = "GLX_EXT_texture_from_pixmap";

/// Extension token for `GLX_SGIX_fbconfig` — the pre-GLX-1.3 vendor-private
/// spelling of the FBConfig API.  Compiz and other legacy compositors require
/// it.  Appended to the advertised extension string only after the
/// VendorPrivate dispatch for opcodes 65540–65542 is implemented.
pub const SGIX_FBCONFIG_EXTENSION: &str = "GLX_SGIX_fbconfig";

#[must_use]
pub fn parse_query_server_string(body: &[u8]) -> Option<QueryServerStringRequest> {
    if body.len() < 8 {
        return None;
    }
    Some(QueryServerStringRequest {
        screen: read_u32_le(body),
        name: read_u32_le(&body[4..]),
    })
}

#[must_use]
pub fn encode_query_version_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    major: u32,
    minor: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0);
    write_u32(byte_order, &mut out, major);
    write_u32(byte_order, &mut out, minor);
    out.extend_from_slice(&[0u8; 16]);
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encode a string-list reply (`QueryServerString` /
/// `QueryExtensionsString`). Layout per `glxproto.h`
/// `xGLXQueryServerStringReply` (sz=32):
///
/// ```text
/// 1   Reply
/// 1   pad
/// 2   sequence
/// 4   length (4-byte units past the 32-byte header)
/// 4   pad1                         ← reserved
/// 4   n        (string byte count, including \0)
/// 16  pad3..pad6
/// n   string bytes (null-terminated, padded to 4-byte boundary)
/// ```
///
/// The `pad1` slot is load-bearing: it's where Mesa's xcb stub
/// expects four bytes of *padding*, not the string length. Putting
/// `n` at offset 8 instead of 12 is the bug that crashed
/// libepoxy's `epoxy_glx_version` (`sscanf` of the resulting
/// "string" returned ret != 2).
#[must_use]
pub fn encode_string_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    string: &str,
) -> Vec<u8> {
    let bytes = string.as_bytes();
    let n = bytes.len() + 1; // include null terminator
    let padded = n.div_ceil(4) * 4;
    let length_units = u32::try_from(padded / 4).unwrap_or(0);
    let mut out = Vec::with_capacity(32 + padded);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, length_units);
    out.extend_from_slice(&[0u8; 4]); // pad1
    write_u32(byte_order, &mut out, u32::try_from(n).unwrap_or(0)); // n
    out.extend_from_slice(&[0u8; 16]); // pad3..pad6
    debug_assert_eq!(out.len(), 32);
    out.extend_from_slice(bytes);
    out.push(0); // null terminator
    while out.len() < 32 + padded {
        out.push(0);
    }
    out
}

/// Encode an `IsDirect` reply.
#[must_use]
pub fn encode_is_direct_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    is_direct: bool,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0);
    out.push(u8::from(is_direct));
    out.extend_from_slice(&[0u8; 23]);
    debug_assert_eq!(out.len(), 32);
    out
}

// GLX FBConfig attribute IDs per `glxtokens.h`. Used as the keys in
// the (attrib, value) pairs that follow the `GetFBConfigsReply`
// header.
pub const GLX_BUFFER_SIZE: u32 = 2;
pub const GLX_LEVEL: u32 = 3;
pub const GLX_DOUBLEBUFFER: u32 = 5;
pub const GLX_STEREO: u32 = 6;
pub const GLX_AUX_BUFFERS: u32 = 7;
pub const GLX_RED_SIZE: u32 = 8;
pub const GLX_GREEN_SIZE: u32 = 9;
pub const GLX_BLUE_SIZE: u32 = 10;
pub const GLX_ALPHA_SIZE: u32 = 11;
pub const GLX_DEPTH_SIZE: u32 = 12;
pub const GLX_STENCIL_SIZE: u32 = 13;
pub const GLX_ACCUM_RED_SIZE: u32 = 14;
pub const GLX_ACCUM_GREEN_SIZE: u32 = 15;
pub const GLX_ACCUM_BLUE_SIZE: u32 = 16;
pub const GLX_ACCUM_ALPHA_SIZE: u32 = 17;

pub const GLX_CONFIG_CAVEAT: u32 = 0x20;
pub const GLX_X_VISUAL_TYPE: u32 = 0x22;
pub const GLX_TRANSPARENT_TYPE: u32 = 0x23;

pub const GLX_VISUAL_ID: u32 = 0x800B;
pub const GLX_SCREEN: u32 = 0x800C;
pub const GLX_DRAWABLE_TYPE: u32 = 0x8010;
pub const GLX_RENDER_TYPE: u32 = 0x8011;
pub const GLX_X_RENDERABLE: u32 = 0x8012;
pub const GLX_FBCONFIG_ID: u32 = 0x8013;

// Drawable geometry / pbuffer tokens (glxtokens.h, GLX 1.3). `GLX_WIDTH`/
// `GLX_HEIGHT` are what `GetDrawableAttributes` reports (Xorg glxcmds.c
// `ATTRIB(GLX_WIDTH, pDraw->width)`); `GLX_PBUFFER_WIDTH`/`HEIGHT` are the
// *request* attribs `CreatePbuffer` carries the requested size in.
pub const GLX_PRESERVED_CONTENTS: u32 = 0x801B;
pub const GLX_LARGEST_PBUFFER: u32 = 0x801C;
pub const GLX_WIDTH: u32 = 0x801D;
pub const GLX_HEIGHT: u32 = 0x801E;
pub const GLX_PBUFFER_HEIGHT: u32 = 0x8040;
pub const GLX_PBUFFER_WIDTH: u32 = 0x8041;

pub const GLX_SAMPLE_BUFFERS: u32 = 100_000;
pub const GLX_SAMPLES: u32 = 100_001;

// GLX_EXT_texture_from_pixmap tokens (glxext.h, 0x20D0–0x20DE range).
// Verified against /usr/include/GL/glxext.h.
pub const GLX_BIND_TO_TEXTURE_RGB_EXT: u32 = 0x20D0;
pub const GLX_BIND_TO_TEXTURE_RGBA_EXT: u32 = 0x20D1;
pub const GLX_BIND_TO_MIPMAP_TEXTURE_EXT: u32 = 0x20D2;
pub const GLX_BIND_TO_TEXTURE_TARGETS_EXT: u32 = 0x20D3;
pub const GLX_Y_INVERTED_EXT: u32 = 0x20D4;
pub const GLX_TEXTURE_TARGET_EXT: u32 = 0x20D6;
pub const GLX_TEXTURE_2D_EXT: u32 = 0x20DC;
// Bind-to-texture target bitmask values (GLX_TEXTURE_*_BIT_EXT).
pub const GLX_TEXTURE_1D_BIT_EXT: u32 = 0x0001;
pub const GLX_TEXTURE_2D_BIT_EXT: u32 = 0x0002;
pub const GLX_TEXTURE_RECTANGLE_BIT_EXT: u32 = 0x0004;
// GLX_DONT_CARE: used for FBConfig "don't care" values (Xorg glxcmds.c:1093).
pub const GLX_DONT_CARE: u32 = 0xFFFF_FFFF;

// Values
pub const GLX_TRUE_COLOR: u32 = 0x8002;
pub const GLX_NONE: u32 = 0x8000;
pub const GLX_WINDOW_BIT: u32 = 0x1;
pub const GLX_PIXMAP_BIT: u32 = 0x2;
/// `GLX_PBUFFER_BIT` (glxtokens.h, 0x4) — `GLX_DRAWABLE_TYPE` bit marking a
/// config usable for pbuffers. ANGLE (Chromium's GL layer) filters
/// `glXChooseFBConfig` on this bit to allocate its offscreen surface; if no
/// config advertises it, ANGLE's hardware GL init fails and Chromium falls
/// back to SwiftShader (software) — no WebGL acceleration, so Google Maps
/// 3D disappears. Firefox uses window surfaces and never needs it. (#96)
pub const GLX_PBUFFER_BIT: u32 = 0x4;
pub const GLX_RGBA_BIT: u32 = 0x1;

/// `GLX_MAX_PBUFFER_WIDTH` / `_HEIGHT` / `_PIXELS` (glxtokens.h, 0x8016–0x8018).
/// Xorg emits these three pairs for every config whose `GLX_DRAWABLE_TYPE`
/// carries `GLX_PBUFFER_BIT` (glxcmds.c:1101-1104); ANGLE reads the max via
/// `glXGetFBConfigAttrib` to validate its pbuffer fits, so a config that sets
/// the bit but reports max 0 is still rejected. Not part of mesa's
/// `driConfigEqual` compare set (proven: yserver's pre-#96 configs omitted
/// them yet still matched radeonsi, which advertises them).
pub const GLX_MAX_PBUFFER_WIDTH: u32 = 0x8016;
pub const GLX_MAX_PBUFFER_HEIGHT: u32 = 0x8017;
pub const GLX_MAX_PBUFFER_PIXELS: u32 = 0x8018;

/// Encode `GetFBConfigsReply`. `configs` is a slice of (attrib,
/// value) pair lists — one list per FBConfig. All lists must have
/// the same length (`num_properties` is shared in the wire format).
///
/// Layout per glxproto `GetFBConfigsReply`:
///
/// ```text
/// 1   Reply
/// 1   pad
/// 2   sequence
/// 4   length = num_FB_configs * num_properties * 2 (in 4-byte units)
/// 4   num_FB_configs
/// 4   num_properties
/// 16  pad
/// (num_FB_configs * num_properties) (CARD32 attrib, CARD32 value) pairs
/// ```
///
/// Falls back to `encode_get_fb_configs_empty_reply` when `configs`
/// is empty.
#[must_use]
pub fn encode_get_fb_configs_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    configs: &[&[(u32, u32)]],
) -> Vec<u8> {
    if configs.is_empty() {
        return encode_get_fb_configs_empty_reply(byte_order, sequence);
    }
    let num_properties = configs[0].len();
    debug_assert!(
        configs.iter().all(|c| c.len() == num_properties),
        "every FBConfig in a GetFBConfigsReply must declare the same property count"
    );
    let num_configs = u32::try_from(configs.len()).unwrap_or(0);
    let num_props = u32::try_from(num_properties).unwrap_or(0);
    let length_units = num_configs.saturating_mul(num_props).saturating_mul(2);
    let mut out = Vec::with_capacity(32 + (length_units as usize) * 4);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, length_units);
    write_u32(byte_order, &mut out, num_configs);
    write_u32(byte_order, &mut out, num_props);
    out.extend_from_slice(&[0u8; 16]);
    debug_assert_eq!(out.len(), 32);
    for config in configs {
        for &(attrib, value) in *config {
            write_u32(byte_order, &mut out, attrib);
            write_u32(byte_order, &mut out, value);
        }
    }
    out
}

/// Encode an empty `GetFBConfigs` reply (zero configs). Used by
/// callers that want to advertise "no FBConfigs" — Mesa survives this
/// by falling back to GetVisualConfigs.
#[must_use]
pub fn encode_get_fb_configs_empty_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0);
    write_u32(byte_order, &mut out, 0); // num_FB_configs
    write_u32(byte_order, &mut out, 0); // num_properties
    out.extend_from_slice(&[0u8; 16]);
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encode a `GetFBConfigsSGIX` reply (`VendorPrivateWithReply`, vendor
/// code 65540).
///
/// glxproto.h has no separate `xGLXGetFBConfigsSGIXReply` typedef — the
/// SGI extension reuses the GLX 1.3 `xGLXGetFBConfigsReply` wire layout
/// (confirmed: only the request struct is defined for SGIX, not a reply).
/// So this function is a thin alias over [`encode_get_fb_configs_reply`]
/// with the same byte layout:
///
/// ```text
/// 1   Reply
/// 1   pad
/// 2   sequence
/// 4   length = num_FB_configs * num_properties * 2 (in 4-byte units)
/// 4   num_FB_configs
/// 4   num_properties
/// 16  pad
/// (num_FB_configs * num_properties) (CARD32 attrib, CARD32 value) pairs
/// ```
#[must_use]
pub fn encode_get_fb_configs_sgix_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    configs: &[&[(u32, u32)]],
) -> Vec<u8> {
    // Wire format is identical to the GLX 1.3 GetFBConfigsReply — proxy
    // directly.  (glxproto.h defines xGLXGetFBConfigsSGIXReq but no
    // separate reply struct; the SGIX reply uses the same layout.)
    encode_get_fb_configs_reply(byte_order, sequence, configs)
}

/// Parse the body of a `GetFBConfigsSGIX` `VendorPrivateWithReply`
/// request.  Returns the screen number, or `None` if the body is too
/// short.
///
/// Wire body layout (`xGLXGetFBConfigsSGIXReq` minus the 4-byte X
/// request header; inside VendorPrivate the full body including the
/// vendor_code is present):
/// ```text
/// [0..4]   vendor_code  (u32 LE) = 65540 — already extracted by caller
/// [4..8]   pad1         (unused contextTag slot)
/// [8..12]  screen       (CARD32)
/// ```
#[must_use]
pub fn parse_get_fb_configs_sgix(body: &[u8]) -> Option<u32> {
    if body.len() < 12 {
        return None;
    }
    Some(read_u32_le(&body[8..]))
}

/// Parsed `CreateContextWithConfigSGIX` request body.
///
/// Wire layout (`xGLXCreateContextWithConfigSGIXReq` minus 4-byte
/// X header; vendor_code+pad1 precede these fields in VendorPrivate):
/// ```text
/// [0..4]   vendor_code  (u32 LE) = 65541
/// [4..8]   pad1
/// [8..12]  context      (GLXContextID — client-chosen XID)
/// [12..16] fbconfig     (GLXFBConfigID)
/// [16..20] screen       (CARD32)
/// [20..24] renderType   (CARD32)
/// [24..28] shareList    (GLXContextID)
/// [28]     isDirect     (BOOL)
/// [29..32] reserved
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreateContextWithConfigSgixRequest {
    pub context: u32,
    pub fbconfig: u32,
    pub screen: u32,
    pub render_type: u32,
    pub share_list: u32,
    pub is_direct: bool,
}

/// Parse a `CreateContextWithConfigSGIX` VendorPrivate body.
/// Returns `None` if body is shorter than 29 bytes.
#[must_use]
pub fn parse_create_context_with_config_sgix(
    body: &[u8],
) -> Option<CreateContextWithConfigSgixRequest> {
    if body.len() < 29 {
        return None;
    }
    Some(CreateContextWithConfigSgixRequest {
        context: read_u32_le(&body[8..]),
        fbconfig: read_u32_le(&body[12..]),
        screen: read_u32_le(&body[16..]),
        render_type: read_u32_le(&body[20..]),
        share_list: read_u32_le(&body[24..]),
        is_direct: body[28] != 0,
    })
}

/// Parsed `CreateGLXPixmapWithConfigSGIX` request body.
///
/// Wire layout (`xGLXCreateGLXPixmapWithConfigSGIXReq` minus 4-byte
/// X header; vendor_code+pad1 precede these fields in VendorPrivate):
/// ```text
/// [0..4]   vendor_code  (u32 LE) = 65542
/// [4..8]   pad1
/// [8..12]  screen       (CARD32)
/// [12..16] fbconfig     (GLXFBConfigID)
/// [16..20] pixmap       (X pixmap XID)
/// [20..24] glxpixmap    (GLXPixmap XID)
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreateGlxPixmapWithConfigSgixRequest {
    pub screen: u32,
    pub fbconfig: u32,
    pub pixmap: u32,
    pub glx_pixmap: u32,
}

/// Parse a `CreateGLXPixmapWithConfigSGIX` VendorPrivate body.
/// Returns `None` if body is shorter than 24 bytes.
#[must_use]
pub fn parse_create_glx_pixmap_with_config_sgix(
    body: &[u8],
) -> Option<CreateGlxPixmapWithConfigSgixRequest> {
    if body.len() < 24 {
        return None;
    }
    Some(CreateGlxPixmapWithConfigSgixRequest {
        screen: read_u32_le(&body[8..]),
        fbconfig: read_u32_le(&body[12..]),
        pixmap: read_u32_le(&body[16..]),
        glx_pixmap: read_u32_le(&body[20..]),
    })
}

/// One GLX 1.2 visual config entry.
///
/// `GetVisualConfigs` (opcode 14) returns the legacy untagged layout:
/// 18 leading `INT32` fields followed by 2*N tagged (attrib, value)
/// pairs. We emit only the 18 leading fields — `num_properties = 18`.
/// Mesa's `__glXInitializeVisualConfigFromTags(!tagged_only)` expects
/// exactly this prefix, in this order.
#[derive(Clone, Copy, Debug)]
pub struct VisualConfig {
    pub visual_id: u32,
    pub visual_class: u32,
    pub rgba: bool,
    pub red_bits: u32,
    pub green_bits: u32,
    pub blue_bits: u32,
    pub alpha_bits: u32,
    pub double_buffer: bool,
    pub stereo: bool,
    pub rgb_bits: u32,
    pub depth_bits: u32,
    pub stencil_bits: u32,
    pub aux_buffers: u32,
    pub level: u32,
}

/// Encode a `GetVisualConfigs` reply. Layout per glxproto:
///
/// ```text
/// 1   Reply (=1)
/// 1   pad
/// 2   sequence
/// 4   length (in 4-byte units beyond 32)
/// 4   numVisuals
/// 4   numProps  (INT32 count per visual)
/// 16  pad
/// numVisuals * numProps INT32 fields
/// ```
///
/// Mesa's `createConfigsFromProperties` rejects `numProps == 0` (and
/// anything less than `__GLX_MIN_CONFIG_PROPS == 18`), so even an
/// "empty" client-visible visual list must ship a non-zero numProps;
/// we just emit zero visuals in that case.
#[must_use]
pub fn encode_get_visual_configs_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    visuals: &[VisualConfig],
) -> Vec<u8> {
    const PROPS_PER_VISUAL: u32 = 18;
    let num_visuals = u32::try_from(visuals.len()).unwrap_or(0);
    let length_units = num_visuals.saturating_mul(PROPS_PER_VISUAL);
    let mut out = Vec::with_capacity(32 + (length_units as usize) * 4);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, length_units);
    write_u32(byte_order, &mut out, num_visuals);
    write_u32(byte_order, &mut out, PROPS_PER_VISUAL);
    out.extend_from_slice(&[0u8; 16]);
    debug_assert_eq!(out.len(), 32);
    for v in visuals {
        write_u32(byte_order, &mut out, v.visual_id);
        write_u32(byte_order, &mut out, v.visual_class);
        write_u32(byte_order, &mut out, u32::from(v.rgba));
        write_u32(byte_order, &mut out, v.red_bits);
        write_u32(byte_order, &mut out, v.green_bits);
        write_u32(byte_order, &mut out, v.blue_bits);
        write_u32(byte_order, &mut out, v.alpha_bits);
        write_u32(byte_order, &mut out, 0); // accum_red_bits
        write_u32(byte_order, &mut out, 0); // accum_green_bits
        write_u32(byte_order, &mut out, 0); // accum_blue_bits
        write_u32(byte_order, &mut out, 0); // accum_alpha_bits
        write_u32(byte_order, &mut out, u32::from(v.double_buffer));
        write_u32(byte_order, &mut out, u32::from(v.stereo));
        write_u32(byte_order, &mut out, v.rgb_bits);
        write_u32(byte_order, &mut out, v.depth_bits);
        write_u32(byte_order, &mut out, v.stencil_bits);
        write_u32(byte_order, &mut out, v.aux_buffers);
        write_u32(byte_order, &mut out, v.level);
    }
    out
}

/// Encode a `MakeCurrent` / `MakeContextCurrent` reply. Fixed 32-byte
/// reply carrying a server-assigned `contextTag` that the client uses
/// to identify subsequent indirect-rendering requests.
///
/// Per `glxproto.h` `xGLXMakeCurrentReply`:
/// ```text
/// 1   Reply
/// 1   pad
/// 2   sequence
/// 4   length = 0
/// 4   contextTag
/// 20  pad
/// ```
#[must_use]
pub fn encode_make_current_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    context_tag: u32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, 0);
    write_u32(byte_order, &mut out, context_tag);
    out.extend_from_slice(&[0u8; 20]);
    debug_assert_eq!(out.len(), 32);
    out
}

/// Encode a `GetDrawableAttributes` reply. Layout per glxproto:
///
/// ```text
/// 1   Reply
/// 1   pad
/// 2   sequence
/// 4   length = 2 * numAttribs (in 4-byte units)
/// 4   numAttribs
/// 20  pad
/// numAttribs * (CARD32 attrib, CARD32 value) pairs
/// ```
///
/// Mesa's `loader_dri3` reads `GLX_FBCONFIG_ID`, `GLX_TEXTURE_TARGET_EXT`
/// and `GLX_Y_INVERTED_EXT` to set up swap-chain orientation for
/// `texture_from_pixmap`. Direct-rendering glxgears doesn't actually
/// consult any of these for rendering, but it does roundtrip on the
/// reply, so we ship the canonical defaults.
#[must_use]
pub fn encode_get_drawable_attributes_reply(
    byte_order: ClientByteOrder,
    sequence: SequenceNumber,
    attribs: &[(u32, u32)],
) -> Vec<u8> {
    let num_attribs = u32::try_from(attribs.len()).unwrap_or(0);
    let length_units = num_attribs.saturating_mul(2);
    let mut out = Vec::with_capacity(32 + (length_units as usize) * 4);
    out.push(1);
    out.push(0);
    write_u16(byte_order, &mut out, sequence.0);
    write_u32(byte_order, &mut out, length_units);
    write_u32(byte_order, &mut out, num_attribs);
    out.extend_from_slice(&[0u8; 20]);
    debug_assert_eq!(out.len(), 32);
    for &(attrib, value) in attribs {
        write_u32(byte_order, &mut out, attrib);
        write_u32(byte_order, &mut out, value);
    }
    out
}

/// Parse a `CreateGLXWindow` (minor 31) request body — fbconfig +
/// X drawable + GLX drawable XID + property list.
#[must_use]
pub fn parse_create_glx_window(body: &[u8]) -> Option<CreateGlxWindowRequest> {
    if body.len() < 16 {
        return None;
    }
    Some(CreateGlxWindowRequest {
        screen: read_u32_le(body),
        fbconfig: read_u32_le(&body[4..]),
        x_window: read_u32_le(&body[8..]),
        glx_window: read_u32_le(&body[12..]),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreatePbufferRequest {
    pub screen: u32,
    pub fbconfig: u32,
    pub pbuffer: u32,
    pub width: u32,
    pub height: u32,
}

/// Parse a `CreatePbuffer` (minor 27) request body. Layout differs from
/// `CreateGLXWindow`: `screen, fbconfig, pbuffer_id, num_attribs` followed
/// by `num_attribs` (attrib, value) `u32` pairs. The pbuffer has **no**
/// parent X drawable; its requested size lives in the `GLX_PBUFFER_WIDTH`/
/// `GLX_PBUFFER_HEIGHT` attribs. Mirrors Xorg `__glXDisp_CreatePbuffer`.
#[must_use]
pub fn parse_create_pbuffer(body: &[u8]) -> Option<CreatePbufferRequest> {
    if body.len() < 16 {
        return None;
    }
    let num_attribs = read_u32_le(&body[12..]) as usize;
    let mut width = 0;
    let mut height = 0;
    let mut p = 16;
    for _ in 0..num_attribs {
        if p + 8 > body.len() {
            break;
        }
        let id = read_u32_le(&body[p..]);
        let value = read_u32_le(&body[p + 4..]);
        match id {
            GLX_PBUFFER_WIDTH => width = value,
            GLX_PBUFFER_HEIGHT => height = value,
            _ => {}
        }
        p += 8;
    }
    Some(CreatePbufferRequest {
        screen: read_u32_le(body),
        fbconfig: read_u32_le(&body[4..]),
        pbuffer: read_u32_le(&body[8..]),
        width,
        height,
    })
}

/// `CreateGLXWindow` request fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreateGlxWindowRequest {
    pub screen: u32,
    pub fbconfig: u32,
    pub x_window: u32,
    pub glx_window: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcodes_match_glxproto() {
        // Pinned against /usr/share/xcb/glx.xml.
        assert_eq!(RENDER, 1);
        assert_eq!(CREATE_CONTEXT, 3);
        assert_eq!(DESTROY_CONTEXT, 4);
        assert_eq!(MAKE_CURRENT, 5);
        assert_eq!(IS_DIRECT, 6);
        assert_eq!(QUERY_VERSION, 7);
        assert_eq!(WAIT_GL, 8);
        assert_eq!(WAIT_X, 9);
        assert_eq!(COPY_CONTEXT, 10);
        assert_eq!(SWAP_BUFFERS, 11);
        // 12 is UseXFont — we don't model it; ensure we didn't shadow it.
        assert_eq!(GET_VISUAL_CONFIGS, 14);
        assert_eq!(VENDOR_PRIVATE, 16);
        assert_eq!(VENDOR_PRIVATE_WITH_REPLY, 17);
        assert_eq!(QUERY_EXTENSIONS_STRING, 18);
        assert_eq!(QUERY_SERVER_STRING, 19);
        assert_eq!(CLIENT_INFO, 20);
        assert_eq!(GET_FB_CONFIGS, 21);
        assert_eq!(CREATE_PIXMAP, 22);
        assert_eq!(DESTROY_PIXMAP, 23);
        assert_eq!(CREATE_NEW_CONTEXT, 24);
        assert_eq!(QUERY_CONTEXT, 25);
        assert_eq!(MAKE_CONTEXT_CURRENT, 26);
        assert_eq!(CREATE_PBUFFER, 27);
        assert_eq!(DESTROY_PBUFFER, 28);
        assert_eq!(GET_DRAWABLE_ATTRIBUTES, 29);
        assert_eq!(CHANGE_DRAWABLE_ATTRIBUTES, 30);
        assert_eq!(CREATE_WINDOW, 31);
        assert_eq!(DELETE_WINDOW, 32);
        assert_eq!(SET_CLIENT_INFO_ARB, 33);
        assert_eq!(CREATE_CONTEXT_ATTRIBS_ARB, 34);
        assert_eq!(SET_CLIENT_INFO_2_ARB, 35);
    }

    #[test]
    fn error_codes_match_glxproto() {
        // Pinned against /usr/share/xcb/glx.xml `errorcopy` entries.
        // Shipped wrong all of today: BAD_REQUEST=0 was actually
        // BadContext, UNSUPPORTED_PRIVATE_REQUEST=11 was
        // BadCurrentDrawable.
        assert_eq!(ERROR_GLX_BAD_RENDER_REQUEST, 6);
        assert_eq!(ERROR_GLX_UNSUPPORTED_PRIVATE_REQUEST, 8);
    }

    #[test]
    fn parse_create_pbuffer_extracts_size_from_attribs() {
        // screen=0, fbconfig=0x104, pbuffer=0x3600001, num_attribs=2,
        // then (GLX_PBUFFER_WIDTH, 16)(GLX_PBUFFER_HEIGHT, 32) — the wire
        // layout Mesa sends (cf. trace `glXCreatePbuffer ... num_attribs=2`).
        let mut body = Vec::new();
        body.extend_from_slice(&0u32.to_le_bytes()); // screen
        body.extend_from_slice(&0x104u32.to_le_bytes()); // fbconfig
        body.extend_from_slice(&0x0360_0001u32.to_le_bytes()); // pbuffer
        body.extend_from_slice(&2u32.to_le_bytes()); // num_attribs
        body.extend_from_slice(&GLX_PBUFFER_WIDTH.to_le_bytes());
        body.extend_from_slice(&16u32.to_le_bytes());
        body.extend_from_slice(&GLX_PBUFFER_HEIGHT.to_le_bytes());
        body.extend_from_slice(&32u32.to_le_bytes());
        let req = parse_create_pbuffer(&body).expect("parse");
        assert_eq!(req.fbconfig, 0x104);
        assert_eq!(req.pbuffer, 0x0360_0001);
        assert_eq!(req.width, 16);
        assert_eq!(req.height, 32);
        // The CreateGLXWindow parser would mis-read this: it would treat
        // the pbuffer id as x_window and num_attribs as the glx_window id.
        let wrong = parse_create_glx_window(&body).expect("parse");
        assert_ne!(wrong.glx_window, req.pbuffer);
    }

    #[test]
    fn server_extensions_advertise_create_context() {
        // ANGLE/Chromium calls glXQueryServerString(GLX_EXTENSIONS)
        // (opcode 19) and refuses a GLES context if GLX_ARB_create_context
        // is missing. The server string must be non-empty and advertise it
        // (regression: STRING_EXTENSIONS used to return "").
        assert!(!SERVER_EXTENSIONS.is_empty());
        assert!(SERVER_EXTENSIONS.contains("GLX_ARB_create_context"));
        // The string reply for STRING_EXTENSIONS must carry the same list.
        let reply = encode_string_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(1),
            SERVER_EXTENSIONS,
        );
        let body = String::from_utf8_lossy(&reply);
        assert!(body.contains("GLX_ARB_create_context"));
        // Regression (cinnamon black screen): GLX_INTEL_swap_event must NOT
        // be advertised until glXSelectEvent + GLX_BufferSwapComplete
        // delivery exist — cogl parks its frame clock on the event otherwise.
        assert!(!SERVER_EXTENSIONS.contains("GLX_INTEL_swap_event"));
        // GLX_EXT_buffer_age promises that GLX_BACK_BUFFER_AGE_EXT reports
        // how many swaps ago the current back buffer was displayed. yserver
        // does not track that history, so advertising the extension lets
        // clients preserve only partial damage in an undefined buffer.
        assert!(!SERVER_EXTENSIONS.contains("GLX_EXT_buffer_age"));
        // Regression (cinnamon SIGSEGV on Asahi): GLX_EXT_libglvnd is what
        // makes libglvnd query GLX_VENDOR_NAMES_EXT instead of guessing a
        // default vendor — without it libglvnd resolves no vendor on
        // Asahi/ALARM and glXQueryExtensionsString returns NULL. Must stay
        // paired with the VENDOR_NAMES_EXT handler in process_request.rs.
        assert!(SERVER_EXTENSIONS.contains("GLX_EXT_libglvnd"));
    }

    #[test]
    fn vendor_names_query_reports_mesa() {
        // libglvnd's vendor-neutral dispatch issues
        // glXQueryServerString(GLX_VENDOR_NAMES_EXT=0x20F6) once it sees
        // GLX_EXT_libglvnd advertised; the reply names the client GLX
        // vendor library (libGLX_mesa) that drives our DRI3 screens.
        assert_eq!(VENDOR_NAMES_EXT, 0x20F6);
        assert_eq!(VENDOR_NAMES, "mesa");
        let reply = encode_string_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(1),
            VENDOR_NAMES,
        );
        assert!(String::from_utf8_lossy(&reply).contains("mesa"));
    }

    #[test]
    fn query_version_reply_shape() {
        let reply =
            encode_query_version_reply(ClientByteOrder::LittleEndian, SequenceNumber(7), 1, 4);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[0], 1);
        assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(reply[12..16].try_into().unwrap()), 4);
    }

    #[test]
    fn string_reply_round_trip() {
        // n = 7 + 1 = 8 → padded = 8 → length_units = 2.
        let reply =
            encode_string_reply(ClientByteOrder::LittleEndian, SequenceNumber(3), "yserver");
        assert_eq!(reply.len(), 32 + 8);
        let length_units = u32::from_le_bytes(reply[4..8].try_into().unwrap());
        assert_eq!(length_units, 2);
        // Bytes 8..12 are pad1 — MUST be zero, not the string length.
        // Putting `n` here used to crash libepoxy's epoxy_glx_version
        // because Mesa reads `n` at offset 12 per glxproto.h
        // xGLXQueryServerStringReply.
        assert_eq!(&reply[8..12], &[0u8; 4], "pad1 must be zero");
        let n = u32::from_le_bytes(reply[12..16].try_into().unwrap());
        assert_eq!(n, 8, "n at offset 12 (after pad1)");
        // Bytes 16..32 are pad3..pad6.
        assert_eq!(&reply[16..32], &[0u8; 16], "pad3..pad6 must be zero");
        assert_eq!(&reply[32..39], b"yserver");
        assert_eq!(reply[39], 0);
    }

    /// Regression for the libepoxy `epoxy_glx_version` assertion
    /// failure that crashed xfce4-session: the version reply must
    /// `sscanf` as `M.N`. With `n` mis-located at offset 8, Mesa's
    /// xcb stub read the X11 reply length (1) instead of strLen,
    /// truncating the version string to a single byte.
    #[test]
    fn version_reply_parses_as_dotted_number() {
        let reply = encode_string_reply(ClientByteOrder::LittleEndian, SequenceNumber(1), "1.4");
        // Header (32) + "1.4\0" (4 bytes, already 4-aligned) = 36.
        assert_eq!(reply.len(), 36);
        let length_units = u32::from_le_bytes(reply[4..8].try_into().unwrap());
        assert_eq!(length_units, 1);
        let n = u32::from_le_bytes(reply[12..16].try_into().unwrap());
        assert_eq!(n, 4);
        assert_eq!(&reply[32..35], b"1.4");
        assert_eq!(reply[35], 0);
    }

    #[test]
    fn is_direct_reply_true() {
        let reply = encode_is_direct_reply(ClientByteOrder::LittleEndian, SequenceNumber(2), true);
        assert_eq!(reply.len(), 32);
        assert_eq!(reply[8], 1);
    }

    #[test]
    fn empty_fb_configs_reply() {
        let reply =
            encode_get_fb_configs_empty_reply(ClientByteOrder::LittleEndian, SequenceNumber(1));
        assert_eq!(reply.len(), 32);
        assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(reply[12..16].try_into().unwrap()), 0);
    }

    #[test]
    fn fb_configs_reply_layout() {
        let cfg_a: &[(u32, u32)] = &[(GLX_VISUAL_ID, 0x102), (GLX_DOUBLEBUFFER, 1)];
        let cfg_b: &[(u32, u32)] = &[(GLX_VISUAL_ID, 0x103), (GLX_DOUBLEBUFFER, 0)];
        let reply = encode_get_fb_configs_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(2),
            &[cfg_a, cfg_b],
        );
        // Header (32) + 2 configs × 2 props × 8 bytes/pair = 32 → total 64.
        assert_eq!(reply.len(), 64);
        // length field: 2 configs × 2 props × 2 (4-byte units per pair) = 8.
        assert_eq!(u32::from_le_bytes(reply[4..8].try_into().unwrap()), 8);
        assert_eq!(u32::from_le_bytes(reply[8..12].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(reply[12..16].try_into().unwrap()), 2);
        assert_eq!(
            u32::from_le_bytes(reply[32..36].try_into().unwrap()),
            GLX_VISUAL_ID
        );
        assert_eq!(u32::from_le_bytes(reply[36..40].try_into().unwrap()), 0x102);
    }

    // ── GLX_SGIX_fbconfig tests ──────────────────────────────────────────────

    /// Verify the SGIX vendor opcode constants are pinned to the values
    /// defined in `/usr/include/GL/glxproto.h`.
    #[test]
    fn sgix_fbconfig_vendor_opcodes_match_glxproto() {
        // Values from glxproto.h:
        //   #define X_GLXvop_GetFBConfigsSGIX               65540
        //   #define X_GLXvop_CreateContextWithConfigSGIX    65541
        //   #define X_GLXvop_CreateGLXPixmapWithConfigSGIX  65542
        assert_eq!(VENDOR_CODE_GET_FB_CONFIGS_SGIX, 65540);
        assert_eq!(VENDOR_CODE_CREATE_CONTEXT_WITH_CONFIG_SGIX, 65541);
        assert_eq!(VENDOR_CODE_CREATE_GLX_PIXMAP_WITH_CONFIG_SGIX, 65542);
    }

    /// `encode_get_fb_configs_sgix_reply` must produce the same byte layout
    /// as `encode_get_fb_configs_reply` — glxproto.h defines no separate SGIX
    /// reply struct; the SGI extension reuses the GLX 1.3 wire format.
    #[test]
    fn get_fb_configs_sgix_reply_matches_glx13_layout() {
        let cfg_a: &[(u32, u32)] = &[(GLX_FBCONFIG_ID, 0x101), (GLX_VISUAL_ID, 0x102)];
        let cfg_b: &[(u32, u32)] = &[(GLX_FBCONFIG_ID, 0x102), (GLX_VISUAL_ID, 0x103)];
        let configs: &[&[(u32, u32)]] = &[cfg_a, cfg_b];

        let sgix_reply = encode_get_fb_configs_sgix_reply(
            ClientByteOrder::LittleEndian,
            SequenceNumber(5),
            configs,
        );
        let glx13_reply =
            encode_get_fb_configs_reply(ClientByteOrder::LittleEndian, SequenceNumber(5), configs);

        // Wire layouts must be identical.
        assert_eq!(
            sgix_reply, glx13_reply,
            "SGIX and GLX 1.3 GetFBConfigs reply must have the same wire format"
        );

        // Spot-check the key offsets per xGLXGetFBConfigsReply:
        // byte 0  = Reply (1)
        // byte 1  = pad
        // byte 2..4 = sequence
        // byte 4..8 = length in 4-byte units = num_configs * num_props * 2
        // byte 8..12 = num_FB_configs
        // byte 12..16 = num_properties
        assert_eq!(sgix_reply[0], 1, "byte 0 must be Reply type");
        let length = u32::from_le_bytes(sgix_reply[4..8].try_into().unwrap());
        // 2 configs × 2 props × 2 = 8 4-byte units
        assert_eq!(length, 8, "length field");
        let num_configs = u32::from_le_bytes(sgix_reply[8..12].try_into().unwrap());
        assert_eq!(num_configs, 2, "num_FB_configs at offset 8");
        let num_props = u32::from_le_bytes(sgix_reply[12..16].try_into().unwrap());
        assert_eq!(num_props, 2, "num_properties at offset 12");
        // payload starts at byte 32
        assert_eq!(
            sgix_reply.len(),
            32 + 2 * 2 * 8,
            "total reply size = 32-byte header + 2 configs × 2 props × 8 bytes/pair"
        );
        // First (attrib, value) pair of first config at byte 32.
        assert_eq!(
            u32::from_le_bytes(sgix_reply[32..36].try_into().unwrap()),
            GLX_FBCONFIG_ID,
            "first attrib must be GLX_FBCONFIG_ID"
        );
        assert_eq!(
            u32::from_le_bytes(sgix_reply[36..40].try_into().unwrap()),
            0x101,
            "first value must be 0x101"
        );
    }

    /// `parse_get_fb_configs_sgix` must extract the screen number from
    /// bytes [8..12] of the VendorPrivate body.
    #[test]
    fn parse_get_fb_configs_sgix_extracts_screen() {
        // Build a minimal SGIX GetFBConfigsSGIX VendorPrivate body:
        //   [0..4]  vendorCode = 65540
        //   [4..8]  pad1 = 0
        //   [8..12] screen = 3
        let mut body = Vec::new();
        body.extend_from_slice(&VENDOR_CODE_GET_FB_CONFIGS_SGIX.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // pad1
        body.extend_from_slice(&3u32.to_le_bytes()); // screen
        assert_eq!(parse_get_fb_configs_sgix(&body), Some(3));
        // Too short → None
        assert_eq!(parse_get_fb_configs_sgix(&body[..8]), None);
    }

    /// `parse_create_context_with_config_sgix` must extract all fields
    /// per the `xGLXCreateContextWithConfigSGIXReq` layout.
    #[test]
    fn parse_create_context_with_config_sgix_fields() {
        // [0..4]  vendorCode = 65541
        // [4..8]  pad1 = 0
        // [8..12] context = 0xABC
        // [12..16] fbconfig = 0x101
        // [16..20] screen = 0
        // [20..24] renderType = 1 (GLX_RGBA_TYPE)
        // [24..28] shareList = 0
        // [28]    isDirect = 1
        // [29..32] reserved = 0
        let mut body = Vec::new();
        body.extend_from_slice(&VENDOR_CODE_CREATE_CONTEXT_WITH_CONFIG_SGIX.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // pad1
        body.extend_from_slice(&0xABCu32.to_le_bytes()); // context
        body.extend_from_slice(&0x101u32.to_le_bytes()); // fbconfig
        body.extend_from_slice(&0u32.to_le_bytes()); // screen
        body.extend_from_slice(&1u32.to_le_bytes()); // renderType
        body.extend_from_slice(&0u32.to_le_bytes()); // shareList
        body.push(1u8); // isDirect
        body.push(0u8); // reserved1
        body.extend_from_slice(&0u16.to_le_bytes()); // reserved2

        let req = parse_create_context_with_config_sgix(&body).expect("parse");
        assert_eq!(req.context, 0xABC);
        assert_eq!(req.fbconfig, 0x101);
        assert_eq!(req.screen, 0);
        assert_eq!(req.render_type, 1);
        assert_eq!(req.share_list, 0);
        assert!(req.is_direct);
        // Too short → None
        assert_eq!(parse_create_context_with_config_sgix(&body[..20]), None);
    }

    /// `parse_create_glx_pixmap_with_config_sgix` must extract all fields
    /// per the `xGLXCreateGLXPixmapWithConfigSGIXReq` layout.
    #[test]
    fn parse_create_glx_pixmap_with_config_sgix_fields() {
        // [0..4]  vendorCode = 65542
        // [4..8]  pad1 = 0
        // [8..12]  screen = 0
        // [12..16] fbconfig = 0x101
        // [16..20] pixmap = 0x2000
        // [20..24] glxpixmap = 0x5000_0001
        let mut body = Vec::new();
        body.extend_from_slice(&VENDOR_CODE_CREATE_GLX_PIXMAP_WITH_CONFIG_SGIX.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // pad1
        body.extend_from_slice(&0u32.to_le_bytes()); // screen
        body.extend_from_slice(&0x101u32.to_le_bytes()); // fbconfig
        body.extend_from_slice(&0x2000u32.to_le_bytes()); // pixmap
        body.extend_from_slice(&0x5000_0001u32.to_le_bytes()); // glxpixmap

        let req = parse_create_glx_pixmap_with_config_sgix(&body).expect("parse");
        assert_eq!(req.screen, 0);
        assert_eq!(req.fbconfig, 0x101);
        assert_eq!(req.pixmap, 0x2000);
        assert_eq!(req.glx_pixmap, 0x5000_0001);
        // Too short → None
        assert_eq!(parse_create_glx_pixmap_with_config_sgix(&body[..16]), None);
    }

    /// `SERVER_EXTENSIONS` must NOT yet contain `GLX_SGIX_fbconfig` —
    /// that token is appended only via the `glx_extension_string` builder
    /// in process_request.rs after the dispatch works.  This test ensures
    /// the base constant stays clean (advertise-after-implement ordering).
    #[test]
    fn sgix_fbconfig_not_in_base_server_extensions() {
        assert!(
            !SERVER_EXTENSIONS.contains("GLX_SGIX_fbconfig"),
            "GLX_SGIX_fbconfig must NOT be in SERVER_EXTENSIONS base constant; \
             it is appended separately via glx_extension_string when supported"
        );
    }
}
