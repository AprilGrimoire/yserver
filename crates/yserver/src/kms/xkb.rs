use xkbcommon::xkb::{Keycode, Keymap};

/// Per-key data extracted from `xkbcommon::Keymap`, ready to lay
/// out into the `KeySymMap` wire structure xkb.xml defines.
struct KeyData {
    /// Width per group — derived from `num_levels_for_key(kc, 0)`,
    /// capped at the wire `width` field's u8 range. With
    /// `num_groups == 0` this is 0 and `nSyms == 0`, yielding the
    /// 8-byte fixed `KeySymMap` header.
    width: u8,
    /// 1 when at least one keysym is defined for layout 0; xkb.xml
    /// stores it in `groupInfo`'s low nibble. We only ever publish
    /// layout 0 to keep the reply shape predictable.
    num_groups: u8,
    /// Index into the `KeyTypes` table — 0 (`ONE_LEVEL`) when
    /// `width <= 1`, 1 (`TWO_LEVEL`) otherwise.
    type_index: u8,
    /// `nSyms = width * num_groups` keysyms, level-0 first then
    /// level-1, etc. Empty levels are filled with `NoSymbol` (0).
    syms: Vec<u32>,
}

/// Modifier-map entry for the `ModifierMap` section: keycode plus
/// the bitset of standard X11 modifier bits the key triggers
/// (Shift=0x01, Lock=0x02, Control=0x04, Mod1..Mod5=0x08..0x80).
struct ModMapEntry {
    keycode: u8,
    mods: u8,
}

/// Map a level-0 keysym to its standard X11 modifier-map bit.
/// Returns 0 when the keysym is not a recognised modifier so the
/// keycode is excluded from the modifier-map list. Mirrors the
/// default xkb modifier conventions used by `evdev_aliases(qwerty)`
/// and friends — enough for xkbcommon to drive Shift/Ctrl/Alt/Lock
/// in any client that re-derives modifier state from the keymap
/// (xcb-rs / xkbcommon-rs in particular).
fn modifier_bit_for_keysym(sym: u32) -> u8 {
    match sym {
        0xFFE1 | 0xFFE2 => 0x01,                            // Shift_L, Shift_R
        0xFFE5 | 0xFFE6 => 0x02,                            // Caps_Lock, Shift_Lock
        0xFFE3 | 0xFFE4 => 0x04,                            // Control_L, Control_R
        0xFFE7 | 0xFFE8 | 0xFFE9 | 0xFFEA | 0xFE03 => 0x08, // Meta_*, Alt_*, ISO_Level3_Shift→Mod1 (xkb default)
        0xFF7F => 0x10,                                     // Num_Lock
        0xFFEB | 0xFFEC => 0x40,                            // Super_L, Super_R
        _ => 0,
    }
}

/// Map a level-0 keysym to the XKB *virtual* modifier name it
/// realises, or `None` for keys that are pure real modifiers
/// (Shift/Lock/Control) or non-modifiers. These names are the XKB
/// convention GDK/mutter match against (`gdkkeys-x11.c`'s
/// `update_keymaps` looks up "Super"/"Hyper"/"Meta"/"Alt"/…), so the
/// real-modifier *binding* for each is derived from the keymap (via
/// [`modifier_bit_for_keysym`]) while only the name↔keysym pairing is
/// convention.
fn vmod_name_for_keysym(sym: u32) -> Option<&'static str> {
    match sym {
        0xFFE9 | 0xFFEA => Some("Alt"),   // Alt_L, Alt_R
        0xFFE7 | 0xFFE8 => Some("Meta"),  // Meta_L, Meta_R
        0xFFEB | 0xFFEC => Some("Super"), // Super_L, Super_R
        0xFFED | 0xFFEE => Some("Hyper"), // Hyper_L, Hyper_R
        0xFF7F => Some("NumLock"),        // Num_Lock
        0xFE03 => Some("LevelThree"),     // ISO_Level3_Shift
        0xFF14 => Some("ScrollLock"),     // Scroll_Lock
        _ => None,
    }
}

/// XKB caps virtual modifiers at 16 (`XkbNumVirtualMods`).
const XKB_NUM_VIRTUAL_MODS: usize = 16;

/// Virtual-modifier description derived from the live keymap.
///
/// Mutter/GDK devirtualize keybinding modifiers (`<Super>`, `<Alt>`)
/// by reading the XKB virtual-modifier section: it matches a vmod by
/// *name* (`VirtualModNames`, GetNames) then resolves it to a real
/// modifier mask via `XkbVirtualModsToReal` over the `vmods[]`
/// bindings (GetMap). With both empty (yserver's prior behaviour),
/// `<Super>` resolves to 0 and `<Super>p` collapses to bare `p`.
pub(super) struct VirtualModData {
    /// Bitmask (16-bit) of which vmod indices are present.
    pub present_mask: u16,
    /// `bindings[i]` = real-modifier mask bound to vmod index `i`.
    pub bindings: [u8; XKB_NUM_VIRTUAL_MODS],
    /// `(vmod_index, name)` for each present vmod, in index order.
    pub names: Vec<(u8, &'static str)>,
    /// `VirtualModMap`: `(keycode, vmod_bits)` pairs.
    pub vmodmap: Vec<(u8, u16)>,
}

/// Build the virtual-modifier section from the live keymap. Vmod
/// indices are assigned in first-seen order over the keycode range;
/// the same assignment feeds GetMap (`vmods[]` + `VirtualModMap`) and
/// GetNames (`VirtualModNames`), so a client matching by name reads a
/// consistent real-modifier binding.
pub(super) fn virtual_mods_from_keymap(keymap: &Keymap) -> VirtualModData {
    let raw_min = keymap.min_keycode().raw();
    let raw_max = keymap.max_keycode().raw();
    let min_kc: u8 = u8::try_from(raw_min).unwrap_or(8).max(8);
    let max_kc: u8 = u8::try_from(raw_max.min(255)).unwrap_or(255).max(min_kc);

    let mut present_mask: u16 = 0;
    let mut bindings = [0u8; XKB_NUM_VIRTUAL_MODS];
    let mut names: Vec<(u8, &'static str)> = Vec::new();
    let mut vmodmap: Vec<(u8, u16)> = Vec::new();
    // Name → assigned vmod index, so repeated keys (L/R) share an index.
    let mut index_for_name: Vec<(&'static str, u8)> = Vec::new();

    for kc_raw in min_kc..=max_kc {
        let kc = Keycode::new(u32::from(kc_raw));
        if keymap.num_layouts_for_key(kc) == 0 {
            continue;
        }
        let level_syms = keymap.key_get_syms_by_level(kc, 0, 0);
        let Some(sym) = level_syms.first().map(|s| s.raw()) else {
            continue;
        };
        let Some(name) = vmod_name_for_keysym(sym) else {
            continue;
        };
        // Real-modifier binding from the same source as the modmap.
        let real_mod = modifier_bit_for_keysym(sym);

        let idx = if let Some((_, idx)) = index_for_name.iter().find(|(n, _)| *n == name) {
            *idx
        } else {
            let idx = u8::try_from(index_for_name.len()).unwrap_or(0);
            if usize::from(idx) >= XKB_NUM_VIRTUAL_MODS {
                continue; // out of vmod slots; ignore extras
            }
            index_for_name.push((name, idx));
            names.push((idx, name));
            idx
        };
        present_mask |= 1 << idx;
        bindings[usize::from(idx)] |= real_mod;
        vmodmap.push((kc_raw, 1 << idx));
    }

    VirtualModData {
        present_mask,
        bindings,
        names,
        vmodmap,
    }
}

/// Build the core `GetModifierMapping` table from the live keymap.
///
/// Returns `(keycodes_per_modifier, data)` where `data` is
/// `8 * keycodes_per_modifier` bytes: the keycodes assigned to each
/// of Shift, Lock, Control, Mod1, Mod2, Mod3, Mod4, Mod5 in that
/// order, zero-padded per row. Derived by walking the keymap's
/// level-0 keysyms through [`modifier_bit_for_keysym`] — the same
/// source of truth as the XKB `GetMap` modifier-map — so the core
/// and XKB views of "which key is Super/Alt/…" never disagree.
pub(super) fn modifier_mapping_from_keymap(keymap: &Keymap) -> (u8, Vec<u8>) {
    // One row per standard X11 modifier bit, indexed by bit position
    // (Shift=0, Lock=1, Control=2, Mod1=3, …, Mod5=7).
    let mut rows: [Vec<u8>; 8] = Default::default();

    let raw_min = keymap.min_keycode().raw();
    let raw_max = keymap.max_keycode().raw();
    let min_kc: u8 = u8::try_from(raw_min).unwrap_or(8).max(8);
    let max_kc: u8 = u8::try_from(raw_max.min(255)).unwrap_or(255).max(min_kc);

    for kc_raw in min_kc..=max_kc {
        let kc = Keycode::new(u32::from(kc_raw));
        if keymap.num_layouts_for_key(kc) == 0 {
            continue;
        }
        let level_syms = keymap.key_get_syms_by_level(kc, 0, 0);
        let Some(sym) = level_syms.first().map(|s| s.raw()) else {
            continue;
        };
        let bit = modifier_bit_for_keysym(sym);
        if bit == 0 {
            continue;
        }
        // `bit` is a single power-of-two modifier flag; its
        // trailing-zero count is the row index.
        let row = bit.trailing_zeros() as usize;
        rows[row].push(kc_raw);
    }

    let kpm = rows.iter().map(Vec::len).max().unwrap_or(0).max(1);
    let mut data = Vec::with_capacity(8 * kpm);
    for row in &rows {
        for i in 0..kpm {
            data.push(row.get(i).copied().unwrap_or(0));
        }
    }
    (u8::try_from(kpm).unwrap_or(u8::MAX), data)
}

fn core_key_groups_for_key(keymap: &Keymap, kc: Keycode) -> Vec<Vec<u32>> {
    let n_groups = usize::try_from(keymap.num_layouts_for_key(kc)).unwrap_or(0);
    let mut groups = Vec::with_capacity(n_groups);
    for group in 0..n_groups {
        let group = u32::try_from(group).unwrap_or(u32::MAX);
        let width = usize::try_from(keymap.num_levels_for_key(kc, group)).unwrap_or(0);
        let mut syms = Vec::with_capacity(width);
        for level in 0..u32::try_from(width).unwrap_or(0) {
            let level_syms = keymap.key_get_syms_by_level(kc, group, level);
            syms.push(level_syms.first().map_or(0, |s| s.raw()));
        }
        groups.push(syms);
    }
    groups
}

fn core_keyboard_map_width(keymap: &Keymap) -> u8 {
    let raw_min = keymap.min_keycode().raw();
    let raw_max = keymap.max_keycode().raw();
    let min_kc: u8 = u8::try_from(raw_min).unwrap_or(8).max(8);
    let max_kc: u8 = u8::try_from(raw_max.min(255)).unwrap_or(255).max(min_kc);

    let mut max_syms_per_key: usize = 0;
    let mut max_group1_width: usize = 0;
    let mut max_number_of_groups: usize = 0;

    for kc_raw in min_kc..=max_kc {
        let kc = Keycode::new(u32::from(kc_raw));
        if keymap.num_layouts_for_key(kc) == 0 {
            continue;
        }
        let n_groups = usize::try_from(keymap.num_layouts_for_key(kc)).unwrap_or(0);
        let mut tmp = 0usize;

        if n_groups > 0 {
            let w = usize::try_from(keymap.num_levels_for_key(kc, 0)).unwrap_or(0);
            tmp += if w <= 2 { 2 } else { w + 2 };
            max_group1_width = max_group1_width.max(w);
        }
        if n_groups > 1 {
            let w = usize::try_from(keymap.num_levels_for_key(kc, 1)).unwrap_or(0);
            if tmp <= 2 {
                tmp += if w < 2 { 2 } else { w };
            } else if w > 2 {
                tmp += w - 2;
            }
        }
        if n_groups > 2 {
            tmp += usize::try_from(keymap.num_levels_for_key(kc, 2)).unwrap_or(0);
        }
        if n_groups > 3 {
            tmp += usize::try_from(keymap.num_levels_for_key(kc, 3)).unwrap_or(0);
        }

        max_syms_per_key = max_syms_per_key.max(tmp);
        max_number_of_groups = max_number_of_groups.max(n_groups);
    }

    if max_syms_per_key == 0 {
        return 0;
    }
    let min_required = max_number_of_groups.saturating_mul(max_group1_width);
    if max_syms_per_key < min_required {
        max_syms_per_key = min_required;
    }
    u8::try_from(max_syms_per_key).unwrap_or(u8::MAX)
}

fn core_keyboard_mapping_row(
    keymap: &Keymap,
    kc: Keycode,
    map_width: usize,
    max_number_of_groups: usize,
) -> Vec<u32> {
    let groups = core_key_groups_for_key(keymap, kc);
    let mut row = vec![0u32; map_width];
    if groups.is_empty() {
        return row;
    }

    let group1 = &groups[0];
    let group1_width = group1.len();
    if group1_width > 0 {
        row[0] = group1[0];
    }
    if group1_width > 1 {
        row[1] = group1[1];
    }
    for n in 2..group1_width {
        let dst = 2 + n;
        if dst < map_width {
            row[dst] = group1[n];
        }
    }

    let mut nout = 2usize;
    if groups.len() == 1 {
        if group1_width > 0 && map_width >= 3 {
            row[2] = row[0];
        }
        if group1_width > 1 && map_width >= 4 {
            row[3] = row[1];
        }

        let mut idx = 2 + group1_width;
        while group1_width > 2 && idx < map_width && idx < group1_width * 2 {
            row[idx] = row[idx - group1_width + 2];
            idx += 1;
        }

        idx = 2 * group1_width;
        if idx < 4 {
            idx = 4;
        }
        for _ in 3..=max_number_of_groups {
            for &sym in group1 {
                if idx >= map_width {
                    break;
                }
                row[idx] = sym;
                idx += 1;
            }
        }
        return row;
    }

    if let Some(group2) = groups.get(1) {
        if !group2.is_empty() {
            row[2] = group2[0];
        }
        if group2.len() > 1 {
            row[3] = group2[1];
        }
        for n in 2..group2.len() {
            let dst = nout + (n - 2);
            if dst < map_width {
                row[dst] = group2[n];
            }
        }
        if group2.len() > 2 {
            nout += group2.len() - 2;
        }
    }

    for group in groups.iter().skip(2) {
        for &sym in group {
            if nout >= map_width {
                return row;
            }
            row[nout] = sym;
            nout += 1;
        }
    }

    row
}

/// Build the core keyboard mapping used by `GetKeyboardMapping` and
/// XI 1.x `GetDeviceKeyMapping`.
pub(super) fn core_keyboard_mapping_from_keymap(
    keymap: &Keymap,
    first_keycode: u8,
    count: u8,
) -> (u8, Vec<u32>) {
    let raw_min = keymap.min_keycode().raw();
    let raw_max = keymap.max_keycode().raw();
    let min_kc: u8 = u8::try_from(raw_min).unwrap_or(8).max(8);
    let max_kc: u8 = u8::try_from(raw_max.min(255)).unwrap_or(255).max(min_kc);
    let map_width = core_keyboard_map_width(keymap);
    if map_width == 0 {
        return (0, Vec::new());
    }

    let mut max_number_of_groups: usize = 0;
    for kc_raw in min_kc..=max_kc {
        let kc = Keycode::new(u32::from(kc_raw));
        max_number_of_groups = max_number_of_groups
            .max(usize::try_from(keymap.num_layouts_for_key(kc)).unwrap_or(0));
    }

    let mut flat = Vec::with_capacity(usize::from(count) * usize::from(map_width));
    for i in 0..count {
        let kc_raw = first_keycode.wrapping_add(i);
        let kc = Keycode::new(u32::from(kc_raw));
        if kc_raw < min_kc || kc_raw > max_kc || keymap.num_layouts_for_key(kc) == 0 {
            flat.extend(std::iter::repeat_n(0u32, usize::from(map_width)));
            continue;
        }
        flat.extend(core_keyboard_mapping_row(
            keymap,
            kc,
            usize::from(map_width),
            max_number_of_groups,
        ));
    }

    (map_width, flat)
}

/// X11 modifier-map bitmask that picks `Shift` — used by the
/// `TWO_LEVEL` `KeyType`'s map entry to say "Shift selects level 1".
const SHIFT_MASK: u8 = 0x01;

/// XKB UseExtension reply (minor=0). Fixed 32 bytes.
/// Reports success and server protocol version 1.0.
pub(super) fn reply_use_extension() -> Vec<u8> {
    let mut r = vec![0u8; 32];
    r[0] = 1; // reply type
    r[1] = 1; // success
    // [2..4] sequence: rewritten by caller
    // [4..8] extra length in 4-byte units = 0
    r[8] = 1; // server-major
    r[9] = 0; // server-minor
    r
}

/// XKB GetControls reply (minor=6). Fixed 92 bytes
/// (`sz_xkbGetControlsReply`). Field offsets follow `xkbGetControlsReply`
/// in `/usr/include/X11/extensions/XKBproto.h`:
///   [0] type, [1] deviceID, [2..4] seq, [4..8] length,
///   [8] mkDfltBtn, [9] numGroups, [10] groupsWrap, [11] internalMods,
///   [12] ignoreLockMods, [13] internalRealMods,
///   [14] ignoreLockRealMods, [15] pad1,
///   [16..18] internalVMods, [18..20] ignoreLockVMods,
///   [20..22] repeatDelay, [22..24] repeatInterval,
///   [24..26] slowKeysDelay, [26..28] debounceDelay,
///   [28..30] mkDelay, [30..32] mkInterval,
///   [32..34] mkTimeToMax, [34..36] mkMaxSpeed,
///   [36..38] mkCurve, [38..40] axOptions,
///   [40..42] axTimeout, [42..44] axtOptsMask,
///   [44..46] axtOptsValues, [46..48] pad2,
///   [48..52] axtCtrlsMask, [52..56] axtCtrlsValues,
///   [56..60] enabledCtrls, [60..92] perKeyRepeat[32].
///
/// xkbcommon's `get_controls` checks
/// `reply->numGroups > 0 && reply->numGroups <= 4` (verified
/// against `objdump` of libxkbcommon-x11.so.0.13.1) — so the
/// previous reply with `numGroups=0` and `repeatDelay/interval/
/// enabledCtrls` written at the wrong offsets failed the keymap
/// build for any xkbcommon-using client.
pub(super) fn reply_get_controls(keymap: &Keymap) -> Vec<u8> {
    let mut r = vec![0u8; 92];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID = 1
    // [4..8] extra length = (92-32)/4 = 15
    r[4..8].copy_from_slice(&15u32.to_le_bytes());
    // numGroups: xkbcommon requires 1..=4; clamp from keymap.num_layouts()
    // (may be 0 for an empty keymap).
    let num_groups: u8 = u8::try_from(keymap.num_layouts()).unwrap_or(1).clamp(1, 4);
    r[9] = num_groups;
    // Repeat delay = 500ms, interval = 33ms (≈30 Hz)
    r[20..22].copy_from_slice(&500_u16.to_le_bytes());
    r[22..24].copy_from_slice(&33_u16.to_le_bytes());
    // EnabledControls: RepeatKeys (bit 0) | PerKeyRepeat — pick
    // RepeatKeys so xkbcommon enables auto-repeat by default.
    r[56..60].copy_from_slice(&0x0000_0001_u32.to_le_bytes());
    r
}

/// XKB GetMap reply (minor=8). Builds a wire-correct reply from
/// `xkbcommon::Keymap` — real key types, per-key syms, and a
/// modifier map — so xkbcommon-x11 clients (wezterm via xkbcommon-rs,
/// every modern toolkit using libxkbcommon) get a usable keymap
/// rather than `NULL`.
///
/// What's published:
/// * Two `KeyTypes`: `ONE_LEVEL` (no modifiers, single level) and
///   `TWO_LEVEL` (Shift→level 1). Wider keys still slot into
///   `TWO_LEVEL`; xkbcommon then derives higher levels from the
///   `kt_index` + level data without us having to encode
///   `ALPHABETIC` / `KEYPAD`.
/// * Per-key `KeySymMap` entries with `num_groups=1` (we publish
///   only layout 0 to keep replies bounded), `width =
///   num_levels_for_key`, and the level-0..width-1 keysyms pulled
///   straight from xkbcommon. Keys with no syms get the 8-byte
///   header only (width=0, num_groups=0).
/// * `ModifierMap` populated from a keysym→mod-bit lookup
///   (`modifier_bit_for_keysym`) so Shift/Ctrl/Alt/Lock translate
///   correctly on the client.
/// * `KeyActions` advertises the full `[min, max]` range with
///   per-key counts of zero — xkbcommon's `get_actions` requires
///   exact range coverage, but accepts no actions per key.
/// * Other sections (`KeyBehaviors`, `VirtualMods`,
///   `ExplicitComponents`, `VirtualModMap`) stay empty —
///   xkbcommon's per-section validators tolerate that here.
///
/// Reply layout follows xkb.xml's `GetMap` switch order, which is
/// XML order, *not* bit-position order:
///   KeyTypes → KeySyms → KeyActions → KeyBehaviors → VirtualMods
///   → ExplicitComponents → ModifierMap → VirtualModMap.
pub(super) fn reply_get_map(keymap: &Keymap) -> Vec<u8> {
    // X11 keycodes are CARD8 — xkbcommon's keymap can carry a wider
    // range (min=9, max=709 for the default us layout). Clamp into
    // [8, 255] so wire counts fit in u8 and ensure min <= max.
    let raw_min = keymap.min_keycode().raw();
    let raw_max = keymap.max_keycode().raw();
    let min_kc: u8 = u8::try_from(raw_min).unwrap_or(8).max(8);
    let max_kc: u8 = u8::try_from(raw_max.min(255)).unwrap_or(255).max(min_kc);
    let n_keys: u8 = max_kc - min_kc + 1;

    // Walk xkbcommon's layout 0 for each keycode in the published
    // range and snapshot the data we'll serialise. Layout 0 covers
    // the user's primary group; secondary groups (AltGr, etc.)
    // would require multi-group KeySymMap entries — followup.
    let mut keys: Vec<KeyData> = Vec::with_capacity(usize::from(n_keys));
    let mut modmap: Vec<ModMapEntry> = Vec::new();
    let mut total_syms: u32 = 0;
    for kc_raw in min_kc..=max_kc {
        let kc = Keycode::new(u32::from(kc_raw));
        let layouts = keymap.num_layouts_for_key(kc);
        let levels = if layouts == 0 {
            0
        } else {
            keymap.num_levels_for_key(kc, 0)
        };
        // u8::MAX is more than enough — XKB caps each key at 8
        // levels in practice; clamp defensively.
        let width: u8 = u8::try_from(levels).unwrap_or(u8::MAX);
        let num_groups: u8 = if width > 0 && layouts > 0 { 1 } else { 0 };
        let mut syms: Vec<u32> = Vec::with_capacity(usize::from(width) * usize::from(num_groups));
        if num_groups == 1 {
            for level in 0..u32::from(width) {
                let level_syms = keymap.key_get_syms_by_level(kc, 0, level);
                let sym = level_syms.first().map(|s| s.raw()).unwrap_or(0);
                syms.push(sym);
            }
        }
        let nsyms_this = u32::from(width) * u32::from(num_groups);
        total_syms = total_syms.saturating_add(nsyms_this);
        let type_index: u8 = if width >= 2 { 1 } else { 0 };
        // Capture modmap entry from level-0 keysym (matches the
        // physical-modifier convention every client expects).
        if num_groups == 1 && !syms.is_empty() {
            let bit = modifier_bit_for_keysym(syms[0]);
            if bit != 0 {
                modmap.push(ModMapEntry {
                    keycode: kc_raw,
                    mods: bit,
                });
            }
        }
        keys.push(KeyData {
            width,
            num_groups,
            type_index,
            syms,
        });
    }

    let total_modmap = u8::try_from(modmap.len()).unwrap_or(u8::MAX);

    // Virtual-modifier section, derived from the keymap.
    let vmod = virtual_mods_from_keymap(keymap);

    // -- Section sizes --------------------------------------------
    // KeyTypes: 4 types. X11 reserves the first four indices
    // (ONE_LEVEL/TWO_LEVEL/ALPHABETIC/KEYPAD) and Xlib's
    // `XkbAllocClientMap` rejects `nTypes < XkbNumRequiredTypes`
    // (= 4) with BadValue — XkbGetMap then returns NULL and GTK3
    // dies with "Failed to get keymap". xkbcommon-x11 doesn't
    // enforce the minimum, which is why wezterm worked at 2.
    // Our keys only reference indices 0 and 1; types 2 and 3 are
    // shipped as minimal ONE_LEVEL stubs so they exist in the
    // table without expanding the symbol layout.
    // ONE_LEVEL = 8 B header (no map entries).
    // TWO_LEVEL = 8 B header + 1 KTSetMapEntry (8 B) = 16 B.
    // Types 2 + 3 = 8 B header each.
    let key_types_bytes: usize = 8 + 16 + 8 + 8;

    // KeySyms: 8-byte header + nSyms * 4 per key.
    let key_syms_bytes: usize = keys
        .iter()
        .map(|k| 8 + 4 * usize::from(k.width) * usize::from(k.num_groups))
        .sum();

    // KeyActions: nKeyActions CARD8s + pad to 4-byte align + 0
    // Action structs (per-key count is zero everywhere).
    let nk = usize::from(n_keys);
    let actions_count_pad = (4 - nk % 4) % 4;
    let key_actions_bytes: usize = nk + actions_count_pad;

    // ModifierMap: 2 bytes per entry + pad to 4-byte align.
    let modmap_raw_bytes: usize = usize::from(total_modmap) * 2;
    let modmap_pad = (4 - modmap_raw_bytes % 4) % 4;
    let modmap_bytes: usize = modmap_raw_bytes + modmap_pad;

    // VirtualMods: one CARD8 binding per present vmod, padded to a
    // 4-byte boundary (Xorg `XkbPaddedSize`).
    let vmod_count: usize = vmod.present_mask.count_ones() as usize;
    let vmod_pad = (4 - vmod_count % 4) % 4;
    let vmod_bytes: usize = vmod_count + vmod_pad;
    // VirtualModMap: `xkbVModMapWireDesc` { key(1) pad(1) vmods(2) } per
    // entry — already 4-byte sized, no extra pad.
    let vmodmap_bytes: usize = vmod.vmodmap.len() * 4;
    // ExplicitComponents stays empty (0 bytes).

    let extra = key_types_bytes
        + key_syms_bytes
        + key_actions_bytes
        + vmod_bytes
        + modmap_bytes
        + vmodmap_bytes;
    let total = 40 + extra;
    let length_words = u32::try_from((total - 32) / 4).unwrap_or(u32::MAX);

    // -- Fixed 40-byte reply header ------------------------------
    let mut r = vec![0u8; total];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID = 1
    r[4..8].copy_from_slice(&length_words.to_le_bytes());
    r[10] = min_kc;
    r[11] = max_kc;
    // present: KeyTypes|KeySyms|ModifierMap|ExplicitComponents
    //         |KeyActions|VirtualMods|VirtualModMap = 0xDF.
    r[12..14].copy_from_slice(&0x00DF_u16.to_le_bytes());
    // [14] firstType=0
    r[15] = 4; // nTypes — XkbNumRequiredTypes
    r[16] = 4; // totalTypes
    r[17] = min_kc; // firstKeySym
    r[18..20].copy_from_slice(&u16::try_from(total_syms).unwrap_or(u16::MAX).to_le_bytes());
    r[20] = n_keys; // nKeySyms — covers full range
    r[21] = min_kc; // firstKeyAction
    // [22..24] totalActions = 0
    r[24] = n_keys; // nKeyActions — covers full range
    r[25] = min_kc; // firstKeyBehavior (bit 5 unset → empty)
    // [26..28] nKeyBehaviors=0, totalKeyBehaviors=0
    r[28] = min_kc; // firstKeyExplicit
    // [29..31] nKeyExplicit=0, totalKeyExplicit=0
    r[31] = min_kc; // firstModMapKey
    r[32] = n_keys; // nModMapKeys (range; totalModMapKeys is the
    //                              actual list length, set next)
    r[33] = total_modmap;
    r[34] = min_kc; // firstVModMapKey
    r[35] = n_keys; // nVModMapKeys — covers full range
    r[36] = u8::try_from(vmod.vmodmap.len()).unwrap_or(u8::MAX); // totalVModMapKeys
    // [37] pad
    r[38..40].copy_from_slice(&vmod.present_mask.to_le_bytes()); // virtualMods

    // -- Section bodies ------------------------------------------
    let mut off = 40;

    // KeyTypes: ONE_LEVEL then TWO_LEVEL.
    // ONE_LEVEL: mods_mask=0, mods_mods=0, mods_vmods=0,
    //            numLevels=1, nMapEntries=0, hasPreserve=0, pad=0.
    r[off + 4] = 1; // numLevels
    off += 8;
    // TWO_LEVEL: mods_mask=Shift, mods_mods=Shift, mods_vmods=0,
    //            numLevels=2, nMapEntries=1, hasPreserve=0, pad=0,
    //            then KTSetMapEntry { active=1, mask=Shift,
    //            level=1, real_mods=Shift, vmods=0, pad=0 }.
    r[off] = SHIFT_MASK; // mods_mask
    r[off + 1] = SHIFT_MASK; // mods_mods (real_mods)
    // [off+2..off+4] mods_vmods = 0
    r[off + 4] = 2; // numLevels
    r[off + 5] = 1; // nMapEntries
    // [off+6] hasPreserve=0, [off+7] pad
    let map_entry = off + 8;
    r[map_entry] = 1; // active = true
    r[map_entry + 1] = SHIFT_MASK; // mask
    r[map_entry + 2] = 1; // level
    r[map_entry + 3] = SHIFT_MASK; // realMods
    // [map_entry+4..+6] virtualMods=0, [map_entry+6..+8] pad
    off += 16;

    // Types 2 (ALPHABETIC) and 3 (KEYPAD): minimal ONE_LEVEL
    // headers. Reserved by X11 but unreferenced by our keymap;
    // shipping them as `numLevels=1, nMapEntries=0` keeps the
    // wire-format valid without growing the body.
    r[off + 4] = 1; // numLevels (ALPHABETIC)
    off += 8;
    r[off + 4] = 1; // numLevels (KEYPAD)
    off += 8;

    // KeySyms: per-key KeySymMap.
    for k in &keys {
        // [off..off+4] kt_index[4] — group 0 uses k.type_index;
        // unused groups stay 0 (ONE_LEVEL), well within nTypes.
        r[off] = k.type_index;
        // [off+4] groupInfo: low 4 bits = num_groups
        r[off + 4] = k.num_groups & 0x0F;
        r[off + 5] = k.width;
        let nsyms = u16::try_from(k.syms.len()).unwrap_or(u16::MAX);
        r[off + 6..off + 8].copy_from_slice(&nsyms.to_le_bytes());
        let mut sym_off = off + 8;
        for sym in &k.syms {
            r[sym_off..sym_off + 4].copy_from_slice(&sym.to_le_bytes());
            sym_off += 4;
        }
        off = sym_off;
    }

    // KeyActions: n_keys per-key counts (all zero by default-init)
    // + pad to 4-byte boundary + 0 Action structs.
    off += nk + actions_count_pad;

    // VirtualMods: one CARD8 real-mod binding per present vmod, in
    // ascending bit order, then pad to a 4-byte boundary. Matches
    // Xorg `XkbSendMap` (xkb.c:1430-1438).
    for i in 0..XKB_NUM_VIRTUAL_MODS {
        if vmod.present_mask & (1 << i) != 0 {
            r[off] = vmod.bindings[i];
            off += 1;
        }
    }
    off += vmod_pad;
    // ExplicitComponents empty (0 entries, 0 pad).

    // ModifierMap: 2 bytes per entry, then pad.
    for entry in &modmap {
        r[off] = entry.keycode;
        r[off + 1] = entry.mods;
        off += 2;
    }
    off += modmap_pad;

    // VirtualModMap: `xkbVModMapWireDesc` { key, pad, vmods(CARD16) }
    // per entry. Combined with the ModifierMap, lets clients resolve
    // each vmod to its real modifier (`XkbVirtualModsToReal`).
    for (key, vmods) in &vmod.vmodmap {
        r[off] = *key;
        // r[off + 1] pad = 0
        r[off + 2..off + 4].copy_from_slice(&vmods.to_le_bytes());
        off += 4;
    }

    debug_assert_eq!(off, total, "GetMap reply body length matches total");
    r
}

/// XKB GetNames reply (minor=17). The `which` mask advertises
/// `KeyTypeNames|KTLevelNames|KeyNames|VirtualModNames`
/// (bits 6,7,9,11 = 0x40|0x80|0x200|0x800 = `0xAC0`), the bitset
/// xkbcommon's `get_names_required` validates. (Verified against
/// `objdump` of `libxkbcommon-x11.so.0.13.1`'s `get_names`: the
/// AND-mask in the `FAIL_UNLESS` is `0xac0`.)
///
/// The reply must agree with `reply_get_map` on `[min_kc, max_kc]`
/// and the type count: xkbcommon's `get_key_names` asserts
/// `firstKey == min_key_code`, `firstKey + nKeys - 1 == max_key_code`,
/// and `reply->{min,max}KeyCode == keymap->{min,max}_key_code`;
/// `get_type_names` asserts `reply->nTypes == keymap->num_types`.
///
/// All name slots carry REAL interned atoms (component names from
/// the compiled RMLVO, canonical type/level names, per-key names
/// from `xkb_keymap_key_get_name`). Plain libX11 clients
/// (xdotool, e16) `XGetAtomName` every atom in this reply — the
/// previous zero-atom stub made them exit on BadAtom. The
/// `nLevelsPerType` list mirrors what `reply_get_map` published:
/// `[1, 2, 1, 1]`; `sumof(nLevelsPerType)` = 5 atoms follow.
pub(super) fn reply_get_names(
    keymap: &Keymap,
    intern_atom: &mut dyn FnMut(&str) -> u32,
) -> Vec<u8> {
    let raw_min = keymap.min_keycode().raw();
    let raw_max = keymap.max_keycode().raw();
    let min_kc: u8 = u8::try_from(raw_min).unwrap_or(8).max(8);
    let max_kc: u8 = u8::try_from(raw_max.min(255)).unwrap_or(255).max(min_kc);
    let n_keys: u8 = max_kc - min_kc + 1;

    // Virtual modifiers (same derivation as GetMap). VirtualModNames
    // must carry one atom per present vmod, in ascending bit order, so
    // a client can match "Super"/"Alt"/… to the binding GetMap sent.
    let vmod = virtual_mods_from_keymap(keymap);
    let vmod_count: usize = vmod.present_mask.count_ones() as usize;

    // -- which mask -----------------------------------------------
    // KeyTypeNames|KTLevelNames|VirtualModNames|KeyNames is what
    // xkbcommon's `get_names_required` enforces (0xAC0). But
    // `get_names()` in libxkbcommon also unconditionally reads
    // `list.keycodesName`, `list.symbolsName`, `list.typesName`,
    // and `list.compatName` from a *stack-uninitialized*
    // `xcb_xkb_get_names_value_list_t list;` (keymap.c:1139-1146).
    // xcb-generated `value_list_unpack` only writes fields whose
    // bit is set in `which`; an absent bit leaves stack garbage
    // there, which xkbcommon then dispatches as GetAtomName
    // requests. We saw the resulting bogus atoms (0xAE4BAA70,
    // 22057, …) in the wire log. Set Keycodes|Symbols|Types|Compat
    // (0x35) on top of 0xAC0 so xcb actually writes zeros into
    // those fields.
    const REQUIRED: u32 = 0x0000_0AC0; // KeyTypeNames|KTLevelNames|VirtualModNames|KeyNames
    const UNCONDITIONALLY_READ: u32 = 0x0000_0035; // Keycodes|Symbols|Types|Compat
    let which: u32 = REQUIRED | UNCONDITIONALLY_READ;

    // -- Section sizes (in xkb.xml switch order, which is bit
    // order — Keycodes(0)→Geometry(1)→Symbols(2)→PhysSymbols(3)→
    // Types(4)→Compat(5)→KeyTypeNames(6)→KTLevelNames(7)→
    // IndicatorNames(8)→VirtualModNames(9)→GroupNames(10)→
    // KeyNames(11)→KeyAliases(12)→RGNames(13)). For us:
    //
    // * keycodesName   ATOM = 4 bytes  (Keycodes)
    // * symbolsName    ATOM = 4 bytes  (Symbols)
    // * typesName      ATOM = 4 bytes  (Types)
    // * compatName     ATOM = 4 bytes  (Compat)
    // * typeNames[4]   ATOM = 16 bytes (KeyTypeNames) — must match
    //                                  GetMap's nTypes=4
    // * nLevelsPerType[4] + pad + ktLevelNames[sumLevels] = 4 + 0 + 5*4
    //                                  = 24 bytes (KTLevelNames)
    // * virtualModNames: 0 bytes       (VirtualModNames, popcount=0)
    // * keyNames[nKeys] KeyName(4) = nKeys * 4 bytes (KeyNames)
    let unconditional_names_bytes = 4 * 4;
    let key_type_names_bytes = 4 * 4;
    let kt_levels_count = 4;
    let kt_levels_count_pad = (4 - kt_levels_count % 4) % 4;
    // nLevelsPerType = [1, 2, 1, 1] (ONE_LEVEL/TWO_LEVEL/ALPHABETIC-stub/KEYPAD-stub)
    let kt_level_names_count: usize = 1 + 2 + 1 + 1;
    let kt_level_names_bytes = kt_levels_count + kt_levels_count_pad + kt_level_names_count * 4;
    let nk = usize::from(n_keys);
    let key_names_bytes = nk * 4;
    // VirtualModNames: one ATOM per present vmod (4 bytes each).
    let vmod_names_bytes = vmod_count * 4;
    let extra = unconditional_names_bytes
        + key_type_names_bytes
        + kt_level_names_bytes
        + vmod_names_bytes
        + key_names_bytes;
    let total = 32 + extra;

    let mut r = vec![0u8; total];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID
    let length_words = u32::try_from(extra / 4).unwrap_or(u32::MAX);
    r[4..8].copy_from_slice(&length_words.to_le_bytes());
    r[8..12].copy_from_slice(&which.to_le_bytes());
    r[12] = min_kc;
    r[13] = max_kc;
    r[14] = 4; // nTypes — must match GetMap's nTypes
    // [15] groupNames = 0
    r[16..18].copy_from_slice(&vmod.present_mask.to_le_bytes()); // virtualMods
    r[18] = min_kc; // firstKey
    r[19] = n_keys; // nKeys — full range
    // [20..24] indicators = 0
    // [24] nRadioGroups = 0
    // [25] nKeyAliases = 0
    // [26..28] nKTLevels (sum of nLevelsPerType) = 1+2+1+1 = 5.
    r[26..28].copy_from_slice(&(kt_level_names_count as u16).to_le_bytes());
    // [28..32] pad

    // -- Body ----------------------------------------------------
    // Every ATOM slot carries a REAL interned atom. The previous
    // zero-atom stub was tuned for xkbcommon (whose
    // `get_escaped_atom_name` short-circuits atom == 0) but plain
    // libX11 clients (xdotool, e16) XGetAtomName every name in the
    // reply — atom 0 → BadAtom → the default error handler exits
    // the client (the e16-in-vng blocker).
    let mut off = 32;
    // keycodesName + symbolsName + typesName + compatName, in bit
    // order (Keycodes=0, Symbols=2, Types=4, Compat=5). Values are
    // the resolved KcCGST components for the RMLVO the server
    // compiles (evdev/pc105/us — `KmsCore::new`), per
    // `setxkbmap -print -rules evdev -model pc105 -layout us`;
    // libxkbcommon resolves the same rules files, it just doesn't
    // expose the component names through its API.
    for name in [
        "evdev+aliases(qwerty)", // keycodesName
        "pc+us+inet(evdev)",     // symbolsName
        "complete",              // typesName
        "complete",              // compatName
    ] {
        let atom = intern_atom(name);
        r[off..off + 4].copy_from_slice(&atom.to_le_bytes());
        off += 4;
    }
    // typeNames: the canonical XKB names of the 4 types
    // reply_get_map publishes (nTypes=4, XkbNumRequiredTypes).
    for name in ["ONE_LEVEL", "TWO_LEVEL", "ALPHABETIC", "KEYPAD"] {
        let atom = intern_atom(name);
        r[off..off + 4].copy_from_slice(&atom.to_le_bytes());
        off += 4;
    }
    // KTLevelNames: nLevelsPerType[] = [1, 2, 1, 1], pad to 4-byte
    // boundary (0 bytes, count already aligned), then sumof(levels)
    // = 5 level-name ATOMs in type order — the canonical XKB
    // shift-level names.
    r[off] = 1; // ONE_LEVEL has 1 level
    r[off + 1] = 2; // TWO_LEVEL has 2 levels
    r[off + 2] = 1; // ALPHABETIC stub
    r[off + 3] = 1; // KEYPAD stub
    off += kt_levels_count + kt_levels_count_pad;
    for name in ["Base", "Base", "Shift", "Base", "Base"] {
        let atom = intern_atom(name);
        r[off..off + 4].copy_from_slice(&atom.to_le_bytes());
        off += 4;
    }
    // VirtualModNames: one ATOM per present vmod, ascending bit order.
    // `vmod.names` is already (index, name) in ascending index order.
    for (_idx, name) in &vmod.names {
        let atom = intern_atom(name);
        r[off..off + 4].copy_from_slice(&atom.to_le_bytes());
        off += 4;
    }
    // KeyNames: char[4] per key (NOT atoms) — the keymap's canonical
    // key names ("ESC", "AE01", …) zero-padded/truncated to 4 bytes.
    // Keys the keymap doesn't name stay all-zero (anonymous).
    for i in 0..nk {
        let kc = u32::from(min_kc) + u32::try_from(i).unwrap_or(u32::MAX);
        if let Some(name) = keymap.key_get_name(Keycode::new(kc)) {
            for (j, b) in name.bytes().take(4).enumerate() {
                r[off + i * 4 + j] = b;
            }
        }
    }
    off += key_names_bytes;
    debug_assert_eq!(off, total, "GetNames reply body length matches total");
    r
}

/// XKB GetCompatMap reply (minor=10). Per xkbproto, fixed 32 bytes
/// — `sz_xkbGetCompatMapReply`. Empty compat map (no SI entries
/// follow).
pub(super) fn reply_get_compat_map() -> Vec<u8> {
    let mut r = vec![0u8; 32];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID = 1
    // [4..8] extra length = 0
    // [8] groupsRtrn = 0
    // [9] pad1
    // [10..12] firstSIRtrn = 0
    // [12..14] nSIRtrn = 0
    // [14..16] nTotalSI = 0
    // [16..32] pad2[16] = 0
    r
}

/// XKB GetDeviceInfo reply (minor=24). The wire-correct *empty*
/// reply is 36 bytes, not 32 — `sizeof(xcb_xkb_get_device_info_reply_t)`
/// (verified via gcc on `xcb/xkb.h`) is 36 because the C struct
/// places `nameLen: CARD16` at offset 32 with 2 bytes of trailing
/// pad. xcb-based clients (xkbcommon-x11 inside `vkgears`,
/// `wezterm`, …) cast the libxcb reply pointer straight to that
/// struct and access `reply->nameLen` plus
/// `xcb_xkb_get_device_info_name(reply) = (char*)(reply + 1)`
/// — both **read past a 32-byte allocation**, producing garbage
/// atoms that the client then fans out as GetAtomName requests
/// (we saw 0xAE4BAA70, 0xAE4B5808, 22057 in the log). xkbcommon-x11
/// then errors out and returns NULL, so `vkgears` segfaults on the
/// resulting `xkb_keymap_ref(NULL)`.
///
/// We publish an empty keyboard: no LED feedbacks, no buttons, no
/// name string, no actions. That's still a 36-byte body — fixed
/// header + `nameLen=0` (2B) + pad-to-4 (2B) — with `length = 1`.
pub(super) fn reply_get_device_info() -> Vec<u8> {
    let mut r = vec![0u8; 36];
    r[0] = 1; // reply
    r[1] = 1; // deviceID = 1
    // [4..8] extra length = (36 - 32) / 4 = 1
    r[4..8].copy_from_slice(&1u32.to_le_bytes());
    // [8..10] present, [10..12] supported, [12..14] unsupported = 0
    // [14..16] nDeviceLedFBs = 0
    // [16] firstBtnWanted, [17] nBtnsWanted
    // [18] firstBtnRtrn, [19] nBtnsRtrn
    // [20..22] totalBtns = 0
    // [22] hasOwnState
    // [23] (padding/alignment)
    // [24..26] dfltKbdFB, [26..28] dfltLedFB
    // [28..32] devType atom = 0
    // [32..34] nameLen = 0
    // [34..36] pad align(4)
    r
}

/// XKB PerClientFlags reply (minor=21). Fixed 32 bytes.
/// Mirrors Xorg's reply shape: advertise the standard per-client flag
/// mask and report the requested value for changed bits. This keeps
/// clients that enable detectable auto-repeat from seeing an all-zero
/// capability/value pair.
pub(super) fn reply_per_client_flags(body: &[u8]) -> Vec<u8> {
    const XKB_PCF_ALL_FLAGS_MASK: u32 = 0x1f;

    let mut r = vec![0u8; 32];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID
    // [2..4] sequence: rewritten by caller
    // [4..8] extra length in 4-byte units = 0
    r[8..12].copy_from_slice(&XKB_PCF_ALL_FLAGS_MASK.to_le_bytes());

    if body.len() >= 12 {
        let change = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
        let value = u32::from_le_bytes([body[8], body[9], body[10], body[11]]);
        let effective = value & change & XKB_PCF_ALL_FLAGS_MASK;
        r[12..16].copy_from_slice(&effective.to_le_bytes());
    }

    r
}

/// Minimal all-zero 32-byte reply for XKB minors that clients tolerate silently.
/// Only use for minors with no required reply content (e.g. SetControls has none).
pub(super) fn reply_minimal(minor: u8) -> Vec<u8> {
    log::debug!("xkb: unimplemented minor {minor}, returning minimal reply");
    let mut r = vec![0u8; 32];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID — must match the value returned by reply_get_map
    // and reply_get_controls etc.; xkbcommon-x11 cross-validates the
    // deviceID across replies and tears down the keymap when it
    // doesn't agree. GTK3's startup path probes minors 4 (GetState)
    // and 21 (PerClientFlags) through here.
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_keymap() -> xkbcommon::xkb::Keymap {
        let ctx = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
        xkbcommon::xkb::Keymap::new_from_names(
            &ctx,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .or_else(|| {
            xkbcommon::xkb::Keymap::new_from_names(
                &ctx,
                "",
                "",
                "",
                "",
                None,
                xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        })
        .expect("test xkb keymap")
    }

    #[test]
    fn use_extension_reply_length() {
        assert_eq!(reply_use_extension().len(), 32);
    }

    #[test]
    fn use_extension_success_flag() {
        let r = reply_use_extension();
        assert_eq!(r[1], 1, "success must be 1");
    }

    #[test]
    fn get_controls_reply_length() {
        let km = test_keymap();
        assert_eq!(reply_get_controls(&km).len(), 92);
    }

    #[test]
    fn modifier_mapping_derived_from_keymap_places_super_on_mod4() {
        // Ground-truth against evdev/pc105/us: Super_L lives on Mod4,
        // not Mod5; Alt on Mod1; Control_L on Control. The bug this
        // guards against is a hand-written table putting Super on the
        // wrong modifier (or omitting it), which makes WM clients
        // resolve Super-shortcuts to a 0 modifier and grab plain keys.
        let km = test_keymap();
        let (kpm, data) = modifier_mapping_from_keymap(&km);
        assert!(kpm >= 1, "at least one keycode per modifier");
        let kpm = usize::from(kpm);
        let row = |idx: usize| &data[idx * kpm..(idx + 1) * kpm];
        let shift = row(0);
        let control = row(2);
        let mod1 = row(3);
        let mod4 = row(6);
        let mod5 = row(7);
        assert!(shift.contains(&50), "Shift_L (50) on Shift row");
        assert!(control.contains(&37), "Control_L (37) on Control row");
        assert!(mod1.contains(&64), "Alt_L (64) on Mod1 row");
        assert!(
            mod4.contains(&133),
            "Super_L (133) must be on Mod4, got mod4={mod4:?} mod5={mod5:?}"
        );
        assert!(
            !mod5.contains(&133),
            "Super_L must NOT be on Mod5 (the old hardcoded-fallback bug)"
        );
    }

    #[test]
    fn get_controls_field_offsets_match_xkbproto() {
        // Field offsets ground-truthed against
        // /usr/include/X11/extensions/XKBproto.h's
        // xkbGetControlsReply struct. xkbcommon's get_controls
        // asserts `numGroups > 0 && numGroups <= 4` (objdump on
        // libxkbcommon-x11.so.0.13.1 shows the test mask 0x05 →
        // unsigned-greater-than-4 reject after the >0 reject).
        let km = test_keymap();
        let r = reply_get_controls(&km);
        assert_eq!(r[0], 1, "reply type");
        assert_eq!(r[1], 1, "deviceID");
        assert_eq!(
            u32::from_le_bytes([r[4], r[5], r[6], r[7]]),
            15,
            "length = (92 - 32) / 4 = 15"
        );
        assert!(r[9] >= 1 && r[9] <= 4, "numGroups in 1..=4");
        // repeatDelay at offset 20..22, repeatInterval at 22..24.
        assert_eq!(u16::from_le_bytes([r[20], r[21]]), 500);
        assert_eq!(u16::from_le_bytes([r[22], r[23]]), 33);
        // enabledCtrls at offset 56..60, with at least RepeatKeys.
        let enabled = u32::from_le_bytes([r[56], r[57], r[58], r[59]]);
        assert_eq!(enabled & 0x01, 0x01, "RepeatKeys bit set");
    }

    #[test]
    fn get_map_reply_invariants() {
        let km = test_keymap();
        let r = reply_get_map(&km);
        // 40-byte fixed reply + body, 4-byte aligned, length matches.
        assert!(r.len() >= 40);
        assert!(r.len().is_multiple_of(4));
        let length_words = u32::from_le_bytes([r[4], r[5], r[6], r[7]]) as usize;
        assert_eq!(length_words * 4 + 32, r.len());

        let min_kc = r[10];
        let max_kc = r[11];
        assert!(min_kc <= max_kc);
        assert!(min_kc >= 8);
        let n_keys = max_kc - min_kc + 1;

        // present advertises every required map part — xkbcommon's
        // get_map_required_components is a subset of 0xDF.
        let present = u16::from_le_bytes([r[12], r[13]]);
        assert_eq!(present & 0xDF, 0xDF);

        // KeyTypes: 4 types — Xlib's XkbAllocClientMap rejects
        // anything less with BadValue (XkbNumRequiredTypes = 4).
        assert_eq!(r[15], 4, "nTypes = XkbNumRequiredTypes");
        assert_eq!(r[16], 4, "totalTypes = XkbNumRequiredTypes");

        // KeySyms covers full range, firstKeySym >= minKeyCode and
        // firstKeySym + nKeySyms <= maxKeyCode + 1.
        assert_eq!(r[17], min_kc, "firstKeySym = min_kc");
        assert_eq!(r[20], n_keys, "nKeySyms = full range");

        // KeyActions covers full range, firstKeyAction == min_key_code
        // and firstKeyAction + nKeyActions == max_key_code + 1.
        assert_eq!(r[21], min_kc, "firstKeyAction = min_kc");
        assert_eq!(r[24], n_keys, "nKeyActions = full range");

        // ModifierMap covers full range; totalModMapKeys must be ≤ nModMapKeys.
        assert_eq!(r[31], min_kc, "firstModMapKey = min_kc");
        assert_eq!(r[32], n_keys, "nModMapKeys = full range");
        assert!(r[33] <= r[32], "totalModMapKeys ≤ nModMapKeys");
    }

    #[test]
    fn get_map_publishes_real_keysyms_for_letter_keys() {
        // The level-0 keysym for a letter key under the default us
        // layout should be the lowercase ASCII codepoint. Walk the
        // KeySyms section looking for the 'a' keysym (0x61) — at
        // least one key should publish it. Pre-fix the section was
        // empty, so this is the regression guard.
        let km = test_keymap();
        let r = reply_get_map(&km);
        let n_keys = r[11] - r[10] + 1;
        // Skip past KeyTypes (8 + 16 = 24 B) to the start of KeySyms.
        let mut off = 40 + 24;
        let mut found_a = false;
        for _ in 0..n_keys {
            // KeySymMap: kt_index[4], groupInfo, width, nSyms, syms[nSyms]
            let width = r[off + 5] as usize;
            let num_groups = (r[off + 4] & 0x0F) as usize;
            let nsyms = u16::from_le_bytes([r[off + 6], r[off + 7]]) as usize;
            assert_eq!(nsyms, width * num_groups, "nSyms = width * num_groups");
            for s in 0..nsyms {
                let sym_off = off + 8 + s * 4;
                let sym = u32::from_le_bytes([
                    r[sym_off],
                    r[sym_off + 1],
                    r[sym_off + 2],
                    r[sym_off + 3],
                ]);
                if sym == b'a' as u32 {
                    found_a = true;
                }
            }
            off += 8 + nsyms * 4;
        }
        assert!(
            found_a,
            "expected level-0 'a' keysym somewhere in the KeySyms section"
        );
    }

    #[test]
    fn get_map_modifier_map_includes_shift() {
        // Shift_L / Shift_R are mod_map'd to bit 0 — find at least
        // one entry with mods == 0x01 to prove the modmap walk
        // worked.
        let km = test_keymap();
        let r = reply_get_map(&km);
        let n_keys = r[11] - r[10] + 1;
        let total_modmap = r[33] as usize;

        // Compute the offset to the ModifierMap section.
        // KeyTypes: ONE_LEVEL (8) + TWO_LEVEL (16) + 2 stubs (8 each) = 40.
        let mut off = 40 + 40; // past KeyTypes
        // Walk KeySyms to advance.
        for _ in 0..n_keys {
            let nsyms = u16::from_le_bytes([r[off + 6], r[off + 7]]) as usize;
            off += 8 + nsyms * 4;
        }
        // KeyActions: nk + pad.
        let nk = usize::from(n_keys);
        off += nk + ((4 - nk % 4) % 4);
        // VirtualMods/ExplicitComponents are empty (0 bytes).

        let mut found_shift = false;
        for i in 0..total_modmap {
            let mods = r[off + 2 * i + 1];
            if mods == 0x01 {
                found_shift = true;
            }
        }
        assert!(
            found_shift,
            "expected at least one Shift modifier-map entry"
        );
    }

    #[test]
    fn core_keyboard_mapping_uses_real_width() {
        let km = test_keymap();
        let (kpc, rows) = core_keyboard_mapping_from_keymap(&km, 8, 4);
        assert!(kpc > 0, "core map width must not be zero");
        assert_eq!(rows.len(), 4 * usize::from(kpc));
        assert!(
            rows.iter().any(|&sym| sym != 0),
            "core keymap should contain real keysyms"
        );
    }

    #[test]
    fn virtual_mods_bind_super_to_mod4_alt_to_mod1() {
        // Ground-truth against evdev/pc105/us: the "Super" virtual
        // modifier must bind to Mod4 (0x40) and "Alt" to Mod1 (0x08),
        // with Super_L (keycode 133) carrying the Super vmod bit in the
        // VirtualModMap. This is the dead-`p` fix: an empty vmod section
        // made mutter resolve <Super> to 0 and grab bare keys.
        let km = test_keymap();
        let vmod = virtual_mods_from_keymap(&km);
        assert!(vmod.present_mask != 0, "at least one vmod present");

        let super_idx = vmod
            .names
            .iter()
            .find(|(_, n)| *n == "Super")
            .map(|(i, _)| *i)
            .expect("Super vmod present");
        assert_eq!(
            vmod.bindings[usize::from(super_idx)],
            0x40,
            "Super must bind to Mod4"
        );

        if let Some((alt_idx, _)) = vmod.names.iter().find(|(_, n)| *n == "Alt") {
            assert_eq!(
                vmod.bindings[usize::from(*alt_idx)],
                0x08,
                "Alt must bind to Mod1"
            );
        }

        // Super_L (keycode 133) maps to the Super vmod bit.
        let super_bit = 1u16 << super_idx;
        assert!(
            vmod.vmodmap
                .iter()
                .any(|(kc, bits)| *kc == 133 && bits & super_bit != 0),
            "Super_L (133) must carry the Super vmod bit; vmodmap={:?}",
            vmod.vmodmap
        );
    }

    #[test]
    fn get_names_emits_super_vmod_name_atom() {
        // VirtualModNames must carry a non-zero atom for each present
        // vmod so a client can match "Super" by name. Verify the
        // interner is invoked with "Super".
        let km = test_keymap();
        let mut seen: Vec<String> = Vec::new();
        let _ = reply_get_names(&km, &mut |name| {
            seen.push(name.to_owned());
            0x77
        });
        assert!(
            seen.iter().any(|n| n == "Super"),
            "GetNames must intern the Super vmod name; interned={seen:?}"
        );
    }

    #[test]
    fn get_names_advertises_required_bits_with_real_data() {
        let km = test_keymap();
        let r = reply_get_names(&km, &mut |_| 0xFFu32);
        let which = u32::from_le_bytes([r[8], r[9], r[10], r[11]]);
        // Bits 6 (KeyTypeNames=0x40) | 7 (KTLevelNames=0x80) |
        // 9 (VirtualModNames=0x200) | 11 (KeyNames=0x800) = 0xAC0
        // is what xkbcommon-x11's `get_names_required` enforces
        // (verified via objdump on libxkbcommon-x11.so.0.13.1).
        let required = 0x0000_0AC0_u32;
        assert_eq!(
            which & required,
            required,
            "GetNames which (0x{which:08x}) must contain all required name detail bits"
        );
        // Reply must agree with reply_get_map on the keycode range.
        let map = reply_get_map(&km);
        assert_eq!(r[12], map[10], "GetNames minKeyCode == GetMap minKeyCode");
        assert_eq!(r[13], map[11], "GetNames maxKeyCode == GetMap maxKeyCode");
        let n_keys = map[11] - map[10] + 1;
        assert_eq!(r[18], map[10], "firstKey == min_kc");
        assert_eq!(r[19], n_keys, "nKeys covers full range");
        assert_eq!(r[14], map[15], "GetNames nTypes == GetMap's nTypes");
        assert_eq!(r[14], 4, "nTypes == XkbNumRequiredTypes");
    }

    /// Interner fixture: hands out sequential ids from 100 and
    /// records the order names were first seen.
    fn recording_interner(seen: &mut Vec<String>) -> impl FnMut(&str) -> u32 + '_ {
        move |name: &str| {
            if let Some(pos) = seen.iter().position(|n| n == name) {
                100 + u32::try_from(pos).unwrap()
            } else {
                seen.push(name.to_owned());
                100 + u32::try_from(seen.len() - 1).unwrap()
            }
        }
    }

    #[test]
    fn get_names_advertises_xkbcommon_unconditional_read_bits() {
        // xkbcommon-x11's `get_names()` (keymap.c:1139-1146) reads
        // `list.{keycodesName,symbolsName,typesName,compatName}`
        // unconditionally from a stack-uninitialized struct. The
        // xcb-generated `value_list_unpack` only writes those
        // fields when their bit is set in `which`; absent bits
        // leave stack garbage there, which the client then
        // dispatches as `GetAtomName(garbage)` requests. Advertise
        // bits 0|2|4|5 (= 0x35 = Keycodes|Symbols|Types|Compat)
        // so xcb writes real atoms into the fields.
        let km = test_keymap();
        let mut seen = Vec::new();
        let r = reply_get_names(&km, &mut recording_interner(&mut seen));
        let which = u32::from_le_bytes([r[8], r[9], r[10], r[11]]);
        let unconditionally_read = 0x0000_0035_u32;
        assert_eq!(
            which & unconditionally_read,
            unconditionally_read,
            "GetNames which (0x{which:08x}) must include Keycodes|Symbols|Types|Compat so \
             xcb unpacks real atoms into the struct fields xkbcommon-x11 reads unconditionally"
        );
        // The four unconditional ATOMs sit at offsets 32..48 in
        // bit order (Keycodes, Symbols, Types, Compat — Geometry
        // and PhysSymbols bits are not advertised, so xcb skips
        // those slots). Each must be the interned atom of the
        // resolved KcCGST component for the RMLVO the server
        // compiles (evdev/pc105/us — core.rs `KmsCore::new`):
        // `setxkbmap -print -rules evdev -model pc105 -layout us`.
        // Plain-libX11 clients (xdotool, e16) XGetAtomName every
        // one of these — atom 0 → BadAtom → exit (the vng guest
        // blocker), so they must be real interned atoms.
        let expected = [
            "evdev+aliases(qwerty)", // keycodesName
            "pc+us+inet(evdev)",     // symbolsName
            "complete",              // typesName
            "complete",              // compatName
        ];
        let mut check = recording_interner(&mut seen);
        for (i, name) in expected.iter().enumerate() {
            let off = 32 + i * 4;
            let atom = u32::from_le_bytes([r[off], r[off + 1], r[off + 2], r[off + 3]]);
            assert_ne!(
                atom, 0,
                "unconditional name atom at offset {off} must not be 0"
            );
            assert_eq!(
                atom,
                check(name),
                "atom at offset {off} must be the interned id of {name:?}"
            );
        }
    }

    #[test]
    fn get_names_interns_type_and_level_name_atoms() {
        // typeNames[4] (offsets 48..64) carry the canonical XKB
        // names of the four types reply_get_map publishes;
        // ktLevelNames (5 atoms after nLevelsPerType[4]) carry the
        // canonical shift-level names. None may be 0 — libX11
        // XGetAtomName(0)s otherwise.
        let km = test_keymap();
        let mut seen = Vec::new();
        let r = reply_get_names(&km, &mut recording_interner(&mut seen));
        let mut check = recording_interner(&mut seen);

        let type_names = ["ONE_LEVEL", "TWO_LEVEL", "ALPHABETIC", "KEYPAD"];
        for (i, name) in type_names.iter().enumerate() {
            let off = 48 + i * 4;
            let atom = u32::from_le_bytes([r[off], r[off + 1], r[off + 2], r[off + 3]]);
            assert_eq!(
                atom,
                check(name),
                "typeNames[{i}] must be the interned id of {name:?}"
            );
        }
        // nLevelsPerType[4] at 64..68 (already covered elsewhere),
        // then 5 level-name ATOMs at 68..88: ONE_LEVEL[Base],
        // TWO_LEVEL[Base, Shift], ALPHABETIC[Base], KEYPAD[Base].
        let level_names = ["Base", "Base", "Shift", "Base", "Base"];
        for (i, name) in level_names.iter().enumerate() {
            let off = 68 + i * 4;
            let atom = u32::from_le_bytes([r[off], r[off + 1], r[off + 2], r[off + 3]]);
            assert_eq!(
                atom,
                check(name),
                "ktLevelNames[{i}] must be the interned id of {name:?}"
            );
        }
    }

    #[test]
    fn get_names_emits_real_key_names_from_keymap() {
        // KeyNames are char[4] slots (not atoms). They must carry
        // the keymap's canonical key names (xkb_keymap_key_get_name)
        // zero-padded/truncated to 4 bytes — real state-derived
        // data, not anonymous zeros (no-protocol-stubs rule).
        let km = test_keymap();
        let mut seen = Vec::new();
        let r = reply_get_names(&km, &mut recording_interner(&mut seen));
        let min_kc = usize::from(r[12]);
        let n_keys = usize::from(r[19]);
        let vmod_count = virtual_mods_from_keymap(&km).present_mask.count_ones() as usize;
        // 32 header + 16 unconditional + 16 typeNames +
        // (4 nLevelsPerType + 20 ktLevelNames) + vmods.
        let key_names_off = 32 + 16 + 16 + 24 + vmod_count * 4;

        // Spot-check a stable anchor: X keycode 9 is ESC in the
        // evdev keycode set.
        let esc = key_names_off + (9 - min_kc) * 4;
        assert_eq!(
            &r[esc..esc + 4],
            b"ESC\0",
            "X keycode 9 must be named ESC (evdev keycodes)"
        );

        // Every key's wire name must match the keymap's own name.
        for i in 0..n_keys {
            let kc = u32::try_from(min_kc + i).unwrap();
            let name = km.key_get_name(Keycode::new(kc)).unwrap_or("");
            let mut expected = [0u8; 4];
            for (j, b) in name.bytes().take(4).enumerate() {
                expected[j] = b;
            }
            let off = key_names_off + i * 4;
            assert_eq!(
                &r[off..off + 4],
                &expected,
                "key name for X keycode {kc} (keymap says {name:?})"
            );
        }
    }

    #[test]
    fn get_compat_map_reply_size_32() {
        assert_eq!(reply_get_compat_map().len(), 32);
    }

    #[test]
    fn get_device_info_reply_matches_xcb_struct_size() {
        // `sizeof(xcb_xkb_get_device_info_reply_t)` is 36 — verified
        // via gcc on `xcb/xkb.h`. A 32-byte reply makes xcb-based
        // clients (xkbcommon-x11) read past the allocation and pick
        // up uninit heap as `nameLen`/atoms; the fix is to publish
        // the full 36 bytes with `length = 1` and `nameLen = 0`.
        let r = reply_get_device_info();
        assert_eq!(
            r.len(),
            36,
            "matches sizeof(xcb_xkb_get_device_info_reply_t)"
        );
        assert_eq!(r[0], 1, "reply type");
        assert_eq!(r[1], 1, "deviceID");
        assert_eq!(
            u32::from_le_bytes([r[4], r[5], r[6], r[7]]),
            1,
            "length = (36 - 32) / 4 = 1"
        );
        assert_eq!(
            u16::from_le_bytes([r[32], r[33]]),
            0,
            "nameLen = 0 (no name follows)"
        );
    }

    #[test]
    fn per_client_flags_reports_supported_and_requested_flags() {
        let mut body = vec![0u8; 24];
        body[4..8].copy_from_slice(&1u32.to_le_bytes()); // change DetectableAutoRepeat
        body[8..12].copy_from_slice(&1u32.to_le_bytes()); // value DetectableAutoRepeat

        let r = reply_per_client_flags(&body);
        assert_eq!(r.len(), 32);
        assert_eq!(r[0], 1, "reply type");
        assert_eq!(r[1], 1, "deviceID");
        assert_eq!(u32::from_le_bytes(r[4..8].try_into().unwrap()), 0);
        assert_eq!(
            u32::from_le_bytes(r[8..12].try_into().unwrap()),
            0x1f,
            "supported = XkbPCF_AllFlagsMask"
        );
        assert_eq!(
            u32::from_le_bytes(r[12..16].try_into().unwrap()),
            1,
            "value reflects changed supported flags"
        );
        assert_eq!(u32::from_le_bytes(r[16..20].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(r[20..24].try_into().unwrap()), 0);
    }
}
