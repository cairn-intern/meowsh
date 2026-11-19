//! Text attributes, after the attribute half of zsh's prompt.c:
//! zattr values, the current/pending/unknown attribute state that
//! applytextattributes() turns into terminal sequences, mixing with
//! layers and opacity, and zle_highlight style parsing and output.

use super::refresh_term::{self as term, *};
use super::write_bytes;
use crate::shell::{array_get, assoc_get};
use std::cell::{Cell, RefCell};

pub(super) type Zattr = u64;

pub(super) const TXTBOLDFACE: Zattr = 0x0001;
pub(super) const TXTFAINT: Zattr = 0x0002;
pub(super) const TXTSTANDOUT: Zattr = 0x0004;
pub(super) const TXTUNDERLINE: Zattr = 0x0008;
pub(super) const TXTITALIC: Zattr = 0x0010;
pub(super) const TXTFGCOLOUR: Zattr = 0x0020;
pub(super) const TXTBGCOLOUR: Zattr = 0x0040;
pub(super) const TXT_ATTR_ALL: Zattr = 0x007F;
pub(super) const TXT_MULTIWORD_MASK: Zattr = 0x0400;
pub(super) const TXT_ERROR: Zattr = 0xF00000F000000003;
const TXT_ATTR_FONT_WEIGHT: Zattr = TXTBOLDFACE | TXTFAINT;
const TXT_ATTR_FG_COL_MASK: Zattr = 0x000000FFFFFF0000;
pub(super) const TXT_ATTR_FG_COL_SHIFT: u32 = 16;
const TXT_ATTR_BG_COL_MASK: Zattr = 0xFFFFFF0000000000;
pub(super) const TXT_ATTR_BG_COL_SHIFT: u32 = 40;
const TXT_ATTR_FG_24BIT: Zattr = 0x4000;
const TXT_ATTR_BG_24BIT: Zattr = 0x8000;
const TXT_ATTR_FG_MASK: Zattr = TXTFGCOLOUR | TXT_ATTR_FG_COL_MASK | TXT_ATTR_FG_24BIT;
const TXT_ATTR_BG_MASK: Zattr = TXTBGCOLOUR | TXT_ATTR_BG_COL_MASK | TXT_ATTR_BG_24BIT;
pub(super) const TXT_ATTR_COLOUR_MASK: Zattr = TXT_ATTR_FG_MASK | TXT_ATTR_BG_MASK;

const COL_SEQ_FG: usize = 0;
const COL_SEQ_BG: usize = 1;

const TC_COL_FG_START: &[u8] = b"\x1b[3";
const TC_COL_BG_START: &[u8] = b"\x1b[4";
const TC_COL_END: &[u8] = b"m";
const TC_COL_DEFAULT: &[u8] = b"9";

const ANSI_COLOURS: [&str; 9] = [
    "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white", "default",
];

struct Highlight {
    name: &'static str,
    mask_on: Zattr,
    mask_off: Zattr,
}

const HIGHLIGHTS: [Highlight; 6] = [
    Highlight {
        name: "reset",
        mask_on: 0,
        mask_off: TXT_ATTR_ALL,
    },
    Highlight {
        name: "bold",
        mask_on: TXTBOLDFACE,
        mask_off: TXTFAINT,
    },
    Highlight {
        name: "faint",
        mask_on: TXTFAINT,
        mask_off: TXTBOLDFACE,
    },
    Highlight {
        name: "standout",
        mask_on: TXTSTANDOUT,
        mask_off: 0,
    },
    Highlight {
        name: "underline",
        mask_on: TXTUNDERLINE,
        mask_off: 0,
    },
    Highlight {
        name: "italic",
        mask_on: TXTITALIC,
        mask_off: 0,
    },
];

#[derive(Clone)]
struct ColourSeq {
    start: Vec<u8>,
    end: Vec<u8>,
    def: Vec<u8>,
}

thread_local! {
    static CURRENT: Cell<Zattr> = const { Cell::new(0) };
    static PENDING: Cell<Zattr> = const { Cell::new(0) };
    static UNKNOWN: Cell<Zattr> = const { Cell::new(0) };
    // The terminal's own colours, from the startup query.
    static MEMO_TERM_COLOR: Cell<Zattr> = const { Cell::new(0) };
    static SEQS: RefCell<Option<[ColourSeq; 2]>> = const { RefCell::new(None) };
    static COLSEQ_ALLOCS: Cell<i32> = const { Cell::new(0) };
    static HL_ENTERED: Cell<bool> = const { Cell::new(false) };
}

// ---- attribute state -----------------------------------------------------

/// txtcurrentattrs = txtpendingattrs = `a`.
pub(super) fn set_current(a: Zattr) {
    CURRENT.with(|c| c.set(a));
    PENDING.with(|c| c.set(a));
}

pub(super) fn set_unknown(a: Zattr) {
    UNKNOWN.with(|c| c.set(a));
}

pub(super) fn treplaceattrs(newattrs: Zattr) {
    if newattrs == TXT_ERROR {
        return;
    }
    let unknown = UNKNOWN.with(|c| c.get());
    if unknown != 0 {
        // Unknown attributes look changed, so they get set again.
        CURRENT.with(|c| c.set((c.get() & !unknown) | (unknown & !newattrs)));
    }
    PENDING.with(|c| c.set(newattrs));
}

pub(super) fn applytextattributes() {
    let current = CURRENT.with(|c| c.get());
    let pending = PENDING.with(|c| c.get());
    let mut unknown = UNKNOWN.with(|c| c.get());
    let mut change = current ^ pending;
    let mut keepon = !change & pending & TXT_ATTR_ALL;
    let mut turnoff = change & !pending & TXT_ATTR_ALL;
    let mut keepcount = 0i32;
    let mut turncount = 0i32;

    if change == 0 {
        return;
    }
    if unknown != 0 {
        unknown &= !change;
        keepcount = 1;
    } else {
        while keepon != 0 {
            keepon &= keepon - 1;
            keepcount += 1;
        }
        while turnoff != 0 {
            turnoff &= turnoff - 1;
            turncount += 1;
        }
    }
    // Bold reliably turns faint off; the converse does not hold.
    if current & TXTFAINT != 0 && pending & TXTBOLDFACE != 0 {
        turncount -= 1;
        change &= !TXTFAINT;
    }
    if keepcount < turncount || change & !pending & TXT_ATTR_FONT_WEIGHT != 0 {
        tsetcap(TCALLATTRSOFF);
        change = pending & TXT_ATTR_ALL & !unknown;
        unknown = 0;
    } else {
        if change & !pending & TXTSTANDOUT != 0 {
            tsetcap(TCSTANDOUTEND);
            change = (pending & TXT_ATTR_ALL & !unknown) | (TXTUNDERLINE & change);
        }
        if change & !pending & TXTUNDERLINE != 0 {
            tsetcap(TCUNDERLINEEND);
            change = pending & TXT_ATTR_ALL & !unknown;
        }
        if change & !pending & TXTITALIC != 0 {
            tsetcap(TCITALICSEND);
        }
    }
    UNKNOWN.with(|c| c.set(unknown));
    if change & pending & TXTBOLDFACE != 0 {
        tsetcap(TCBOLDFACEBEG);
    }
    if change & pending & TXTFAINT != 0 {
        tsetcap(TCFAINTBEG);
    }
    if change & pending & TXTSTANDOUT != 0 {
        tsetcap(TCSTANDOUTBEG);
    }
    if change & pending & TXTUNDERLINE != 0 {
        tsetcap(TCUNDERLINEBEG);
    }
    if change & pending & TXTITALIC != 0 {
        tsetcap(TCITALICSBEG);
    }
    if change & TXT_ATTR_FG_MASK != 0 {
        set_colour_attribute(pending, COL_SEQ_FG);
    }
    if change & TXT_ATTR_BG_MASK != 0 {
        set_colour_attribute(pending, COL_SEQ_BG);
    }
    CURRENT.with(|c| c.set(pending));
}

// ---- colours -------------------------------------------------------------

/// Records a colour the terminal reported for itself (zsh's
/// memo_term_color, set by termquery.c's handle_color): `bg` false for
/// the foreground.
pub fn set_memo_term_color(bg: bool, red: u8, green: u8, blue: u8) {
    let rgb = ((red as Zattr) << 16) | ((green as Zattr) << 8) | blue as Zattr;
    MEMO_TERM_COLOR.with(|m| {
        let mut v = m.get();
        if bg {
            v &= !TXT_ATTR_BG_MASK;
            v |= TXT_ATTR_BG_24BIT | (rgb << TXT_ATTR_BG_COL_SHIFT);
        } else {
            v &= !TXT_ATTR_FG_MASK;
            v |= TXT_ATTR_FG_24BIT | (rgb << TXT_ATTR_FG_COL_SHIFT);
        }
        m.set(v);
    });
}

fn truecolor_terminal() -> bool {
    for f in array_get(".term.extensions").unwrap_or_default() {
        let on = !f.starts_with('-');
        if f[usize::from(!on)..] == *"truecolor" {
            return on;
        }
    }
    false
}

fn nearcolor_loaded() -> bool {
    crate::modules::modules_param()
        .get("zsh/nearcolor")
        .is_some_and(|s| s == "loaded")
}

// load_module("zsh/nearcolor"): zsh loads it on demand.
fn load_nearcolor() -> bool {
    if nearcolor_loaded() {
        return true;
    }
    crate::modules::builtin_zmodload(&["zmodload".into(), "zsh/nearcolor".into()]) == 0
}

/// Formats `#rrggbb` / `#rgb` for `%F{...}` / `%K{...}` prompts, autoloading
/// `zsh/nearcolor` when the terminal is not truecolor.
pub fn prompt_hex_color(r: u8, g: u8, b: u8, is_fg: bool) -> String {
    let prefix = if is_fg { 38 } else { 48 };
    if truecolor_terminal() {
        return format!("\x1b[{};2;{};{};{}m", prefix, r, g, b);
    }
    if nearcolor_loaded() || load_nearcolor() {
        let c = getcolorattr(r as u32, g as u32, b as u32);
        if c > 0 {
            return format!("\x1b[{};5;{}m", prefix, c - 1);
        }
    }
    if is_fg {
        "\x1b[39m".to_string()
    } else {
        "\x1b[49m".to_string()
    }
}

/// runhookdef(GETCOLORATTR): 0 when no hook is installed, else the
/// nearest colour plus one, or -1 when there is none.
fn getcolorattr(red: u32, green: u32, blue: u32) -> i32 {
    if !nearcolor_loaded() {
        return 0;
    }
    match tccolours() {
        256 => map_rgb_to_256(red, green, blue) + 1,
        88 => map_rgb_to_88(red, green, blue) + 1,
        _ => -1,
    }
}

fn rgb_to_lab(red: u32, green: u32, blue: u32) -> (f64, f64, f64) {
    let lin = |c: u32| {
        let c = c as f64 / 255.0;
        100.0
            * if c > 0.04045 {
                ((c + 0.055) / 1.055).powf(2.4)
            } else {
                c / 12.92
            }
    };
    let (r, g, b) = (lin(red), lin(green), lin(blue));
    let f = |v: f64| {
        if v > 0.008856 {
            v.powf(1.0 / 3.0)
        } else {
            7.787 * v + 16.0 / 116.0
        }
    };
    let x = f((r * 0.4124 + g * 0.3576 + b * 0.1805) / 95.047);
    let y = f((r * 0.2126 + g * 0.7152 + b * 0.0722) / 100.0);
    let z = f((r * 0.0193 + g * 0.1192 + b * 0.9505) / 108.883);
    (116.0 * y - 16.0, 500.0 * (x - y), 200.0 * (y - z))
}

fn deltae(a: (f64, f64, f64), b: (f64, f64, f64)) -> f64 {
    (a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)
}

// nearcolor.c's search: `ncube` cube components, then greys.
fn nearest(
    red: u32,
    green: u32,
    blue: u32,
    component: &[u32],
    ncube: usize,
) -> (usize, usize, usize) {
    let orig = rgb_to_lab(red, green, blue);
    let mut best = -1.0f64;
    let mut found = (0, 0, 0);
    for r in 0..component.len() {
        let mut g = 0;
        while g < ncube {
            let mut b = 0;
            while b < ncube {
                if r >= ncube {
                    g = r;
                    b = r;
                }
                let next = rgb_to_lab(component[r], component[g], component[b]);
                let d = deltae(orig, next);
                if d < best || best < 0.0 {
                    best = d;
                    found = (r, g, b);
                }
                b += 1;
            }
            g += 1;
        }
    }
    found
}

fn map_rgb_to_88(red: u32, green: u32, blue: u32) -> i32 {
    const C: [u32; 12] = [
        0, 0x8b, 0xcd, 0xff, 0x2e, 0x5c, 0x73, 0x8b, 0xa2, 0xb9, 0xd0, 0xe7,
    ];
    let (r, g, b) = nearest(red, green, blue, &C, 4);
    if r > 3 {
        76 + r as i32
    } else {
        16 + (r * 16 + g * 4 + b) as i32
    }
}

fn map_rgb_to_256(red: u32, green: u32, blue: u32) -> i32 {
    const C: [u32; 30] = [
        0, 0x5f, 0x87, 0xaf, 0xd7, 0xff, 0x8, 0x12, 0x1c, 0x26, 0x30, 0x3a, 0x44, 0x4e, 0x58, 0x62,
        0x6c, 0x76, 0x80, 0x8a, 0x94, 0x9e, 0xa8, 0xb2, 0xbc, 0xc6, 0xd0, 0xda, 0xe4, 0xee,
    ];
    let (r, g, b) = nearest(red, green, blue, &C, 6);
    if r > 5 {
        226 + r as i32
    } else {
        16 + (r * 36 + g * 6 + b) as i32
    }
}

fn map256_to_rgb(atr: &mut Zattr, shift: u32, set24: Zattr) {
    if *atr & set24 != 0 {
        return;
    }
    let mut colour = ((*atr >> shift) & 0xff) as u32;
    if colour < 16 {
        return;
    }
    let (red, green, blue);
    if colour < 232 {
        colour -= 16;
        blue = u32::from(colour != 0) * 0x37 + 40 * (colour % 6);
        colour /= 6;
        green = u32::from(colour != 0) * 0x37 + 40 * (colour % 6);
        colour /= 6;
        red = u32::from(colour != 0) * 0x37 + 40 * colour;
    } else {
        red = 8 + 10 * (colour - 232);
        green = red;
        blue = red;
    }
    *atr &= !(0xffffff_u64 << shift);
    *atr |= set24 | ((((((red << 8) + green) << 8) + blue) as Zattr) << shift);
}

/// zsh's mixattrs(): `primary` over `secondary`, `mask` saying which of
/// primary's attributes were set explicitly, with any opacity.
pub(super) fn mixattrs(primary: Zattr, mask: Zattr, secondary: Zattr) -> Zattr {
    let mut mix: Zattr = 0;
    let mut replace = mask & TXT_ATTR_ALL;
    let mut toset = TXT_ATTR_FG_MASK;
    let mut isset = TXTFGCOLOUR;
    let mut istrue = TXT_ATTR_FG_24BIT;
    let mut shift = TXT_ATTR_FG_COL_SHIFT;

    if mask & TXT_ATTR_FONT_WEIGHT != 0 {
        replace |= TXT_ATTR_FONT_WEIGHT;
    }
    if mask & TXTFGCOLOUR != 0 {
        replace |= TXT_ATTR_FG_MASK;
    }
    if mask & TXTBGCOLOUR != 0 {
        replace |= TXT_ATTR_BG_MASK;
    }
    let mut keep = !replace;

    loop {
        let opacity = (mask >> shift) & 127;
        if mask & isset != 0 && opacity != 0 {
            let memo = MEMO_TERM_COLOR.with(|m| m.get());
            let mut aatt = if primary & isset != 0 { primary } else { memo };
            let mut batt = if secondary & isset != 0 {
                secondary
            } else {
                memo
            };
            keep &= !toset;
            replace &= !toset;
            if tccolours() == 256 {
                map256_to_rgb(&mut aatt, shift, istrue);
                map256_to_rgb(&mut batt, shift, istrue);
            }
            if aatt & batt & istrue != 0 {
                mix |= istrue | isset;
                for i in [0u32, 8, 16] {
                    let argb = (aatt >> (shift + i)) & 0xff;
                    let brgb = (batt >> (shift + i)) & 0xff;
                    mix |= ((argb * (100 - opacity) + brgb * opacity) / 100) << (shift + i);
                }
                if !truecolor_terminal() && (nearcolor_loaded() || load_nearcolor()) {
                    let idx = getcolorattr(
                        ((mix >> (shift + 16)) & 0xff) as u32,
                        ((mix >> (shift + 8)) & 0xff) as u32,
                        ((mix >> shift) & 0xff) as u32,
                    ) - 1;
                    if idx >= 0 {
                        mix &= !toset;
                        mix |= isset | ((idx as Zattr) << shift);
                    }
                }
            } else if opacity <= 50 {
                replace |= toset;
            } else {
                keep |= toset;
            }
        }
        if isset == TXTBGCOLOUR {
            break;
        }
        shift = TXT_ATTR_BG_COL_SHIFT;
        toset = TXT_ATTR_BG_COL_MASK;
        isset = TXTBGCOLOUR;
        istrue = TXT_ATTR_BG_24BIT;
    }
    (primary & replace) | (secondary & keep) | mix
}

fn default_seqs() -> [ColourSeq; 2] {
    [
        ColourSeq {
            start: TC_COL_FG_START.to_vec(),
            end: TC_COL_END.to_vec(),
            def: TC_COL_DEFAULT.to_vec(),
        },
        ColourSeq {
            start: TC_COL_BG_START.to_vec(),
            end: TC_COL_END.to_vec(),
            def: TC_COL_DEFAULT.to_vec(),
        },
    ]
}

/// allocate_colour_buffer(): on the outermost call, takes the colour
/// codes from $zle_highlight. They stay set once given, as in zsh.
pub(super) fn allocate_colour_buffer() {
    let n = COLSEQ_ALLOCS.with(|c| c.replace(c.get() + 1));
    if n != 0 {
        return;
    }
    let atrs = array_get("zle_highlight").unwrap_or_default();
    SEQS.with(|s| {
        let mut s = s.borrow_mut();
        let seqs = s.get_or_insert_with(default_seqs);
        for a in &atrs {
            let code = |p: &str| a.strip_prefix(p).map(crate::zle::keymap::getkeystring);
            if let Some(v) = code("fg_start_code:") {
                seqs[COL_SEQ_FG].start = v;
            } else if let Some(v) = code("fg_default_code:") {
                seqs[COL_SEQ_FG].def = v;
            } else if let Some(v) = code("fg_end_code:") {
                seqs[COL_SEQ_FG].end = v;
            } else if let Some(v) = code("bg_start_code:") {
                seqs[COL_SEQ_BG].start = v;
            } else if let Some(v) = code("bg_default_code:") {
                seqs[COL_SEQ_BG].def = v;
            } else if let Some(v) = code("bg_end_code:") {
                seqs[COL_SEQ_BG].end = v;
            }
        }
    });
}

pub(super) fn free_colour_buffer() {
    COLSEQ_ALLOCS.with(|c| c.set((c.get() - 1).max(0)));
}

fn set_colour_attribute(atr: Zattr, fg_bg: usize) {
    let (colour, tc, def, use_truecolor) = if fg_bg == COL_SEQ_FG {
        (
            ((atr & TXT_ATTR_FG_COL_MASK) >> TXT_ATTR_FG_COL_SHIFT) as i64,
            TCFGCOLOUR,
            atr & TXTFGCOLOUR == 0,
            atr & TXT_ATTR_FG_24BIT != 0,
        )
    } else {
        (
            ((atr & TXT_ATTR_BG_COL_MASK) >> TXT_ATTR_BG_COL_SHIFT) as i64,
            TCBGCOLOUR,
            atr & TXTBGCOLOUR == 0,
            atr & TXT_ATTR_BG_24BIT != 0,
        )
    };
    let seq = SEQS.with(|s| s.borrow_mut().get_or_insert_with(default_seqs)[fg_bg].clone());
    let std_start = if fg_bg == COL_SEQ_FG {
        TC_COL_FG_START
    } else {
        TC_COL_BG_START
    };
    let is_default = seq.start == std_start && seq.def == TC_COL_DEFAULT && seq.end == TC_COL_END;

    if !def && !use_truecolor && is_default {
        let tccolours = tccolours();
        if tccan(tc) && (tccolours < 0 || colour < tccolours as i64) {
            write_bytes(term::tgoto_str(tc, colour as i32).as_bytes());
            return;
        }
        if colour > 255 {
            return;
        }
    }
    let do_free = COLSEQ_ALLOCS.with(|c| c.get()) == 0;
    if do_free {
        // Moving the cursor in trashzle() can get here outside a refresh.
        allocate_colour_buffer();
    }
    // Allocating may have read new codes from $zle_highlight.
    let seq = SEQS.with(|s| s.borrow_mut().get_or_insert_with(default_seqs)[fg_bg].clone());
    {
        let mut buf: Vec<u8> = if use_truecolor {
            std_start.to_vec()
        } else {
            seq.start.clone()
        };
        if def {
            if use_truecolor {
                buf.extend_from_slice(TC_COL_DEFAULT);
            } else {
                buf.extend_from_slice(&seq.def);
            }
        } else if use_truecolor {
            buf.extend_from_slice(
                format!(
                    "8;2;{};{};{}",
                    colour >> 16,
                    (colour >> 8) & 0xff,
                    colour & 0xff
                )
                .as_bytes(),
            );
        } else if (8..=255).contains(&colour) {
            buf.extend_from_slice(colour.to_string().as_bytes());
        } else {
            buf.push((colour as u8).wrapping_add(b'0'));
        }
        if use_truecolor {
            buf.extend_from_slice(TC_COL_END);
        } else {
            buf.extend_from_slice(&seq.end);
        }
        write_bytes(&buf);
    }
    if do_free {
        free_colour_buffer();
    }
}

// ---- parsing and printing styles ---------------------------------------

fn inblank(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

/// zstrtol(): optional blanks and sign, then digits in `base`. Returns
/// the value and the index after it (`i` itself when there were none).
pub(super) fn zstrtol(s: &[u8], mut i: usize, base: u32) -> (i64, usize) {
    let start = i;
    while i < s.len() && inblank(s[i]) {
        i += 1;
    }
    let neg = match s.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let digits = i;
    let mut v: i64 = 0;
    while let Some(d) = s.get(i).and_then(|&c| (c as char).to_digit(base)) {
        v = v.wrapping_mul(base as i64).wrapping_add(d as i64);
        i += 1;
    }
    if i == digits {
        return (0, start);
    }
    (if neg { -v } else { v }, i)
}

fn match_named_colour(s: &[u8], i: &mut usize) -> i32 {
    let start = *i;
    while *i < s.len() && s[*i].is_ascii_alphabetic() {
        *i += 1;
    }
    let word = &s[start..*i];
    ANSI_COLOURS
        .iter()
        .position(|c| c.as_bytes().starts_with(word))
        .map_or(-1, |p| p as i32)
}

/// zsh's match_colour() for the text at `s[*i..]`.
fn match_colour(s: &[u8], i: &mut usize, is_fg: bool) -> Zattr {
    let (shft, on, tc) = if is_fg {
        (TXT_ATTR_FG_COL_SHIFT, TXTFGCOLOUR, TCFGCOLOUR)
    } else {
        (TXT_ATTR_BG_COL_SHIFT, TXTBGCOLOUR, TCBGCOLOUR)
    };
    let colour: i64 =
        if s.get(*i) == Some(&b'#') && s.get(*i + 1).is_some_and(|c| c.is_ascii_hexdigit()) {
            let (col, end) = zstrtol(s, *i + 1, 16);
            let (red, green, blue);
            if end - *i == 4 {
                red = (col >> 8) | ((col >> 8) << 4);
                let g = (col & 0xf0) >> 4;
                green = g | (g << 4);
                let b = col & 0xf;
                blue = b | (b << 4);
            } else if end - *i == 7 {
                red = col >> 16;
                green = (col & 0xff00) >> 8;
                blue = col & 0xff;
            } else {
                return TXT_ERROR;
            }
            *i = end;
            let (r, g, b) = (red as u32, green as u32, blue as u32);
            let mut c = getcolorattr(r, g, b) as i64 - 1;
            if c == -1 && !truecolor_terminal() && load_nearcolor() {
                c = getcolorattr(r, g, b) as i64 - 1;
            }
            if c == -1 {
                let rgb = (((r as Zattr) << 8) + g as Zattr) << 8 | b as Zattr;
                let t24 = if is_fg {
                    TXT_ATTR_FG_24BIT
                } else {
                    TXT_ATTR_BG_24BIT
                };
                return on | t24 | (rgb << shft);
            } else if c <= -2 {
                return TXT_ERROR;
            }
            c
        } else if s.get(*i).is_some_and(|c| c.is_ascii_alphabetic()) {
            let c = match_named_colour(s, i);
            if c == 8 {
                return 0;
            }
            if c < 0 {
                return TXT_ERROR;
            }
            c as i64
        } else {
            let (c, end) = zstrtol(s, *i, 10);
            *i = end;
            if !(0..256).contains(&c) {
                return TXT_ERROR;
            }
            c
        };
    if tccan(tc) && colour > 7 && colour >= tccolours() as i64 {
        return TXT_ERROR;
    }
    on | (colour as Zattr) << shft
}

// parsehighlight(): an `hl=` group from ${.zle.hlgroups}. Returns the
// attributes (TXT_ERROR when there is no such group), the mask to use if
// any, and where parsing continues.
fn parsehighlight(s: &[u8], i: usize, endchar: u8) -> (Zattr, Option<Zattr>, usize) {
    let end = s[i..].iter().position(|&c| c == endchar).map(|p| i + p);
    let arg = String::from_utf8_lossy(&s[i..end.unwrap_or(s.len())]).into_owned();
    let next = end.map_or(s.len(), |e| e + 1);
    if HL_ENTERED.with(|e| e.get()) {
        return (TXT_ERROR, None, next);
    }
    let Some(attrs) = assoc_get(".zle.hlgroups", &arg) else {
        return (TXT_ERROR, None, next);
    };
    HL_ENTERED.with(|e| e.set(true));
    let m = match_highlight(&attrs, false);
    HL_ENTERED.with(|e| e.set(false));
    let atr = if m.consumed == 0 { TXT_ERROR } else { m.atr };
    (atr, Some(m.mask), next)
}

pub(super) struct Matched {
    pub atr: Zattr,
    pub mask: Zattr,
    pub layer: Option<i32>,
    pub consumed: usize,
}

/// zsh's match_highlight(): attributes, the mask of those given
/// explicitly, the layer (when `want_layer`) and how much was parsed.
pub(super) fn match_highlight(teststr: &str, want_layer: bool) -> Matched {
    let s = teststr.as_bytes();
    let mut i = 0usize;
    let mut found = true;
    let mut mask: Zattr = 0;
    let mut on: Zattr = 0;
    let mut layer = None;
    let rest_ok = |i: &mut usize| -> bool {
        match s.get(*i) {
            Some(b',') => {
                *i += 1;
                true
            }
            None | Some(b' ') => true,
            _ => false,
        }
    };
    while found && i < s.len() {
        found = false;
        let t = &s[i..];
        if t.starts_with(b"hl=") {
            let (atr, m, next) = parsehighlight(s, i + 3, b',');
            if let Some(m) = m {
                mask = m;
            }
            if atr != TXT_ERROR {
                on = atr;
            }
            i = next;
            found = true;
        } else if t.starts_with(b"fg=") || t.starts_with(b"bg=") {
            let is_fg = t[0] == b'f';
            i += 3;
            let atr = match_colour(s, &mut i, is_fg);
            if !rest_ok(&mut i) {
                break;
            }
            found = true;
            if atr != TXT_ERROR {
                on &= if is_fg {
                    !TXT_ATTR_FG_MASK
                } else {
                    !TXT_ATTR_BG_MASK
                };
                on |= atr;
                mask |= if is_fg { TXTFGCOLOUR } else { TXTBGCOLOUR };
            }
        } else if want_layer && t.starts_with(b"layer=") {
            let (v, end) = zstrtol(s, i + 6, 10);
            i = end;
            layer = Some(v as i32);
            if !rest_ok(&mut i) {
                break;
            }
            found = true;
        } else if t.starts_with(b"opacity=") {
            let (op, end) = zstrtol(s, i + 8, 10);
            i = end;
            if !(0..=100).contains(&op) {
                break;
            }
            if s.get(i) == Some(&b'%') {
                i += 1;
            }
            // Stored inverted, so 0 is fully opaque.
            mask |= ((100 - op) as Zattr) << TXT_ATTR_FG_COL_SHIFT;
            let mut op = op;
            if s.get(i) == Some(&b'/') {
                let (o2, end) = zstrtol(s, i + 1, 10);
                i = end;
                if !(0..=100).contains(&o2) {
                    break;
                }
                if s.get(i) == Some(&b'%') {
                    i += 1;
                }
                op = o2;
            }
            mask |= ((100 - op) as Zattr) << TXT_ATTR_BG_COL_SHIFT;
            if !rest_ok(&mut i) {
                break;
            }
            found = true;
        } else {
            let mut turn_off = false;
            for (n, hl) in HIGHLIGHTS.iter().enumerate() {
                if found {
                    break;
                }
                if s[i..].starts_with(hl.name.as_bytes()) {
                    let mut val = i + hl.name.len();
                    match s.get(val) {
                        Some(b',') => val += 1,
                        None | Some(b' ') => {}
                        _ => break,
                    }
                    if turn_off {
                        on &= !hl.mask_on & !hl.mask_off;
                    } else {
                        on |= hl.mask_on;
                        on &= !hl.mask_off;
                    }
                    mask |= hl.mask_on | hl.mask_off;
                    i = val;
                    found = true;
                }
                // "noreset" is not valid, so `no` is looked for after it.
                if n == 0 && s[i..].starts_with(b"no") {
                    turn_off = true;
                    i += 2;
                }
            }
        }
    }
    Matched {
        atr: on,
        mask,
        layer,
        consumed: i,
    }
}

fn output_colour(colour: u64, fg: bool, truecol: bool) -> String {
    let mut s = String::from(if fg { "fg=" } else { "bg=" });
    if truecol {
        s.push_str(&format!(
            "#{:02x}{:02x}{:02x}",
            colour >> 16,
            (colour >> 8) & 0xff,
            colour & 0xff
        ));
    } else if colour > 7 {
        s.push_str(&colour.to_string());
    } else {
        s.push_str(ANSI_COLOURS[colour as usize]);
    }
    s
}

/// zsh's output_highlight(): the style text for `atr` under `mask`.
pub(super) fn output_highlight(atr: Zattr, mut mask: Zattr) -> String {
    let mut out = String::new();
    let sep = |out: &mut String| {
        if !out.is_empty() {
            out.push(',');
        }
    };
    if mask == TXT_ATTR_ALL {
        let mut threebits = !atr & TXT_ATTR_ALL;
        threebits &= threebits.wrapping_sub(1);
        threebits &= threebits.wrapping_sub(1);
        if threebits != 0 {
            mask &= atr;
            out.push_str("reset");
        }
    }
    if mask & TXTFGCOLOUR != 0 {
        sep(&mut out);
        if atr & TXTFGCOLOUR != 0 {
            out.push_str(&output_colour(
                (atr & TXT_ATTR_FG_COL_MASK) >> TXT_ATTR_FG_COL_SHIFT,
                true,
                atr & TXT_ATTR_FG_24BIT != 0,
            ));
        } else {
            out.push_str("fg=default");
        }
    }
    if mask & TXTBGCOLOUR != 0 {
        sep(&mut out);
        if atr & TXTBGCOLOUR != 0 {
            out.push_str(&output_colour(
                (atr & TXT_ATTR_BG_COL_MASK) >> TXT_ATTR_BG_COL_SHIFT,
                false,
                atr & TXT_ATTR_BG_24BIT != 0,
            ));
        } else {
            out.push_str("bg=default");
        }
    }
    for hp in &HIGHLIGHTS {
        if hp.mask_on & mask != 0 && hp.mask_off & mask & atr == 0 {
            mask &= !hp.mask_off;
            sep(&mut out);
            if hp.mask_on & atr == 0 {
                out.push_str("no");
            }
            out.push_str(hp.name);
        }
    }
    if out.is_empty() {
        return "none".into();
    }
    if mask & (TXT_ATTR_FG_COL_MASK | TXT_ATTR_BG_COL_MASK) != 0 {
        let fg_op = (mask >> TXT_ATTR_FG_COL_SHIFT) & 127;
        let bg_op = (mask >> TXT_ATTR_BG_COL_SHIFT) & 127;
        out.push_str(&format!(",opacity={}%", 100u64.wrapping_sub(fg_op)));
        if fg_op != bg_op {
            out.push_str(&format!("/{}%", 100u64.wrapping_sub(bg_op)));
        }
    }
    out
}

/// The attributes in force after the SGR sequences in `s`, starting
/// from `a`. meowsh prompts carry raw escapes where zsh's promptexpand
/// tracks attributes as it emits them.
pub(super) fn sgr_attrs(s: &str, mut a: Zattr) -> Zattr {
    let b = s.as_bytes();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] != 0x1b || b[i + 1] != b'[' {
            i += 1;
            continue;
        }
        let start = i + 2;
        let mut j = start;
        while j < b.len() && (b[j].is_ascii_digit() || b[j] == b';' || b[j] == b':') {
            j += 1;
        }
        if j >= b.len() || b[j] != b'm' {
            i = j.max(i + 1);
            continue;
        }
        let params: Vec<i64> = std::str::from_utf8(&b[start..j])
            .unwrap_or("")
            .split([';', ':'])
            .map(|p| p.parse().unwrap_or(0))
            .collect();
        let mut k = 0;
        while k < params.len() {
            let p = params[k];
            let colour = |a: &mut Zattr, fg: bool, val: Zattr, truecol: bool| {
                let (mask, on, shift, t24) = if fg {
                    (
                        TXT_ATTR_FG_MASK,
                        TXTFGCOLOUR,
                        TXT_ATTR_FG_COL_SHIFT,
                        TXT_ATTR_FG_24BIT,
                    )
                } else {
                    (
                        TXT_ATTR_BG_MASK,
                        TXTBGCOLOUR,
                        TXT_ATTR_BG_COL_SHIFT,
                        TXT_ATTR_BG_24BIT,
                    )
                };
                *a &= !mask;
                *a |= on | (val << shift) | (if truecol { t24 } else { 0 });
            };
            match p {
                0 => a = 0,
                1 => a = (a & !TXTFAINT) | TXTBOLDFACE,
                2 => a = (a & !TXTBOLDFACE) | TXTFAINT,
                3 => a |= TXTITALIC,
                4 => a |= TXTUNDERLINE,
                7 => a |= TXTSTANDOUT,
                22 => a &= !TXT_ATTR_FONT_WEIGHT,
                23 => a &= !TXTITALIC,
                24 => a &= !TXTUNDERLINE,
                27 => a &= !TXTSTANDOUT,
                30..=37 => colour(&mut a, true, (p - 30) as Zattr, false),
                40..=47 => colour(&mut a, false, (p - 40) as Zattr, false),
                90..=97 => colour(&mut a, true, (p - 82) as Zattr, false),
                100..=107 => colour(&mut a, false, (p - 92) as Zattr, false),
                39 => a &= !TXT_ATTR_FG_MASK,
                49 => a &= !TXT_ATTR_BG_MASK,
                38 | 48 => {
                    let fg = p == 38;
                    match params.get(k + 1) {
                        Some(5) => {
                            let n = params.get(k + 2).copied().unwrap_or(0) as Zattr & 0xff;
                            colour(&mut a, fg, n, false);
                            k += 2;
                        }
                        Some(2) => {
                            let c =
                                |n: usize| params.get(k + n).copied().unwrap_or(0) as Zattr & 0xff;
                            colour(&mut a, fg, (c(2) << 16) | (c(3) << 8) | c(4), true);
                            k += 4;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            k += 1;
        }
        i = j + 1;
    }
    a
}
