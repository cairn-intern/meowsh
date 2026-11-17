//! The special parameters widgets see ($BUFFER, $CURSOR, ...), after
//! zsh's zle_params.c.
//!
//! They are ordinary local parameters made when a user widget starts.
//! An assignment to one of the linked ones (BUFFER, LBUFFER, RBUFFER,
//! CURSOR, MARK) fixes up the others at once, through [`reconcile`], so
//! reads always agree. The editor reads them back ([`pull`]) after shell
//! code runs and writes them ([`push`]) after it changes the line.

use super::*;
use crate::params::{Param, Value, INTEGER, READONLY, UNSET};
use crate::types::Shell;
use std::sync::atomic::{AtomicUsize, Ordering};

/// How many widget scopes hold the special parameters.
static DEPTH: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Scalar,
    Integer,
    Array,
    Assoc,
}

const SPECIALS: &[(&str, Kind, bool)] = &[
    ("BUFFER", Kind::Scalar, false),
    ("BUFFERLINES", Kind::Integer, true),
    ("CONTEXT", Kind::Scalar, true),
    ("CURSOR", Kind::Integer, false),
    ("CUTBUFFER", Kind::Scalar, false),
    ("HISTNO", Kind::Integer, false),
    ("KEYMAP", Kind::Scalar, true),
    ("KEYS", Kind::Scalar, true),
    ("KEYS_QUEUED_COUNT", Kind::Integer, true),
    ("killring", Kind::Array, false),
    ("LASTABORTEDSEARCH", Kind::Scalar, true),
    ("LASTSEARCH", Kind::Scalar, true),
    ("LASTWIDGET", Kind::Scalar, true),
    ("LBUFFER", Kind::Scalar, false),
    ("MARK", Kind::Integer, false),
    ("NUMERIC", Kind::Integer, false),
    ("PENDING", Kind::Integer, true),
    ("POSTDISPLAY", Kind::Scalar, false),
    ("PREBUFFER", Kind::Scalar, true),
    ("PREDISPLAY", Kind::Scalar, false),
    ("RBUFFER", Kind::Scalar, false),
    ("REGION_ACTIVE", Kind::Integer, false),
    ("region_highlight", Kind::Array, false),
    ("registers", Kind::Assoc, false),
    ("UNDO_CHANGE_NO", Kind::Integer, true),
    ("UNDO_LIMIT_NO", Kind::Integer, false),
    ("WIDGET", Kind::Scalar, true),
    ("WIDGETFUNC", Kind::Scalar, true),
    ("WIDGETSTYLE", Kind::Scalar, true),
    ("YANK_START", Kind::Integer, false),
    ("YANK_END", Kind::Integer, false),
    ("YANK_ACTIVE", Kind::Integer, true),
    ("ISEARCHMATCH_START", Kind::Integer, false),
    ("ISEARCHMATCH_END", Kind::Integer, false),
    ("ISEARCHMATCH_ACTIVE", Kind::Integer, true),
    ("SUFFIX_START", Kind::Integer, false),
    ("SUFFIX_END", Kind::Integer, false),
    ("SUFFIX_ACTIVE", Kind::Integer, true),
    ("ZLE_RECURSIVE", Kind::Integer, true),
    ("ZLE_STATE", Kind::Scalar, true),
];

thread_local! {
    // What push() last wrote for the values compared in pull().
    static PUSHED: std::cell::RefCell<Pushed> = std::cell::RefCell::new(Pushed::default());
}

#[derive(Default, Clone)]
struct Pushed {
    cutbuffer: String,
    killring: Vec<String>,
    region_highlight: Vec<String>,
    numeric: Option<i64>,
    histno: i64,
    registers: std::collections::BTreeMap<String, String>,
}

fn s(v: &[char]) -> String {
    v.iter().collect()
}

fn values(z: &mut Zle) -> Vec<(&'static str, Value, bool)> {
    let i = |n: i64| Value::Scalar(n.to_string());
    let widget = z.bindk.clone().unwrap_or_default();
    let w = thingy::lookup(&widget);
    let (wfunc, wstyle) = match w.as_ref().map(|w| w.kind.clone()) {
        Some(thingy::WidgetKind::User(f)) => (f, String::new()),
        Some(thingy::WidgetKind::Complete { builtin, func }) => (func, builtin),
        Some(thingy::WidgetKind::Internal(_)) => (widget.clone(), String::new()),
        None => (String::new(), String::new()),
    };
    let numeric_set = z.zmod.flags & (MOD_MULT | MOD_TMULT) != 0;
    let context = match z.zlecontext {
        ZLCON_LINE_CONT => "cont",
        ZLCON_SELECT => "select",
        ZLCON_VARED => "vared",
        _ => "start",
    };
    let mut state = String::from(if z.insmode { "insert" } else { "overwrite" });
    if z.region_active != 0 {
        state.push_str(" region-active");
    }
    let lines = 1 + z.line.iter().filter(|c| **c == '\n').count() as i64;
    let undo_no = z.undo_current_change();
    let registers: std::collections::BTreeMap<String, String> = (0..36)
        .filter(|&n| z.vibuf[n].set)
        .map(|n| (register_name(n), s(&z.vibuf[n].buf)))
        .collect();
    vec![
        ("BUFFER", Value::Scalar(s(&z.line)), true),
        ("BUFFERLINES", i(lines), true),
        ("CONTEXT", Value::Scalar(context.into()), true),
        ("CURSOR", i(z.cs as i64), true),
        ("CUTBUFFER", Value::Scalar(s(&z.cutbuf.buf)), true),
        ("HISTNO", i(z.histline as i64), true),
        ("KEYMAP", Value::Scalar(z.curkeymapname.clone()), true),
        (
            "KEYS",
            Value::Scalar(String::from_utf8_lossy(&z.keybuf).into_owned()),
            true,
        ),
        ("KEYS_QUEUED_COUNT", i(z.unget.len() as i64), true),
        ("killring", Value::Array(killring(z)), true),
        (
            "LASTABORTEDSEARCH",
            Value::Scalar(z.lastabortedsearch.clone().unwrap_or_default()),
            true,
        ),
        (
            "LASTSEARCH",
            Value::Scalar(z.lastsearch.clone().unwrap_or_default()),
            true,
        ),
        (
            "LASTWIDGET",
            Value::Scalar(z.lbindk.clone().unwrap_or_default()),
            true,
        ),
        ("LBUFFER", Value::Scalar(s(&z.line[..z.cs])), true),
        ("MARK", i(z.mark as i64), true),
        ("NUMERIC", i(z.zmod.mult), numeric_set),
        ("PENDING", i(pending(z) as i64), true),
        ("POSTDISPLAY", Value::Scalar(s(&z.postdisplay)), true),
        ("PREBUFFER", Value::Scalar(super::prebuffer()), true),
        ("PREDISPLAY", Value::Scalar(s(&z.predisplay)), true),
        ("RBUFFER", Value::Scalar(s(&z.line[z.cs..])), true),
        ("REGION_ACTIVE", i(z.region_active as i64), true),
        (
            "region_highlight",
            Value::Array(refresh::get_region_highlight(z)),
            true,
        ),
        ("registers", Value::Assoc(registers), true),
        ("UNDO_CHANGE_NO", i(undo_no), true),
        ("UNDO_LIMIT_NO", i(z.undo_limitno), true),
        ("WIDGET", Value::Scalar(widget), true),
        ("WIDGETFUNC", Value::Scalar(wfunc), true),
        ("WIDGETSTYLE", Value::Scalar(wstyle), true),
        ("YANK_START", i(z.yankb as i64), true),
        ("YANK_END", i(z.yanke as i64), true),
        (
            "YANK_ACTIVE",
            i(i64::from(z.lastcmd & ZLE_YANK != 0)
                * if z.lastcmd & ZLE_YANKAFTER != 0 { 2 } else { 1 }),
            true,
        ),
        (
            "ISEARCHMATCH_START",
            i(z.isearch.0.unwrap_or(0) as i64),
            true,
        ),
        ("ISEARCHMATCH_END", i(z.isearch.1.unwrap_or(0) as i64), true),
        ("ISEARCHMATCH_ACTIVE", i(i64::from(z.isearch.2)), true),
        ("SUFFIX_START", i(z.suffix.start as i64), true),
        ("SUFFIX_END", i(z.suffix.end as i64), true),
        ("SUFFIX_ACTIVE", i(i64::from(z.suffix.active)), true),
        ("ZLE_RECURSIVE", i(z.recursive as i64), true),
        ("ZLE_STATE", Value::Scalar(state), true),
    ]
}

fn register_name(n: usize) -> String {
    if n < 26 {
        ((b'a' + n as u8) as char).to_string()
    } else {
        ((b'0' + (n - 26) as u8) as char).to_string()
    }
}

fn pending(z: &Zle) -> usize {
    let mut n: libc::c_int = 0;
    unsafe {
        libc::ioctl(z.fd, libc::FIONREAD, &mut n);
    }
    z.unget.len() + n.max(0) as usize
}

fn killring(z: &mut Zle) -> Vec<String> {
    if z.kring.is_empty() {
        z.kringsize = KRINGCTDEF;
        z.kring = vec![CutBuffer::default(); KRINGCTDEF];
    }
    let mut out = Vec::new();
    let mut kpos = z.kringnum;
    for _ in 0..z.kringsize {
        out.push(s(&z.kring[kpos].buf));
        kpos = (kpos + z.kringsize - 1) % z.kringsize;
    }
    out
}

/// Makes the special parameters as locals of the scope just pushed.
pub fn make_params(z: &mut Zle) {
    let vals = values(z);
    remember(&vals);
    crate::shell::with_shell(|sh| {
        for (name, value, set) in vals {
            let kind = SPECIALS.iter().find(|x| x.0 == name).unwrap();
            sh.make_local(name);
            let level = sh.local_scopes.len();
            let mut p = Param::with(value);
            p.level = level;
            if kind.1 == Kind::Integer {
                p.attrs |= INTEGER;
            }
            if kind.2 {
                p.attrs |= READONLY;
            }
            if !set {
                p.attrs |= UNSET;
            }
            p.attrs |= crate::params::SPECIAL;
            sh.params.insert(name.to_string(), p);
        }
    });
    DEPTH.fetch_add(1, Ordering::SeqCst);
}

fn remember(vals: &[(&'static str, Value, bool)]) {
    let get = |n: &str| vals.iter().find(|v| v.0 == n).map(|v| v.1.clone());
    let p = Pushed {
        cutbuffer: match get("CUTBUFFER") {
            Some(Value::Scalar(s)) => s,
            _ => String::new(),
        },
        killring: match get("killring") {
            Some(Value::Array(a)) => a,
            _ => Vec::new(),
        },
        region_highlight: match get("region_highlight") {
            Some(Value::Array(a)) => a,
            _ => Vec::new(),
        },
        numeric: vals
            .iter()
            .find(|v| v.0 == "NUMERIC")
            .filter(|v| v.2)
            .and_then(|v| match &v.1 {
                Value::Scalar(s) => s.parse().ok(),
                _ => None,
            }),
        histno: match get("HISTNO") {
            Some(Value::Scalar(s)) => s.parse().unwrap_or(0),
            _ => 0,
        },
        registers: match get("registers") {
            Some(Value::Assoc(m)) => m,
            _ => Default::default(),
        },
    };
    PUSHED.with(|x| *x.borrow_mut() = p);
}

/// Writes the editor's state into the special parameters, when a widget
/// scope has them.
pub fn push(z: &mut Zle) {
    if DEPTH.load(Ordering::SeqCst) == 0 {
        return;
    }
    let vals = values(z);
    remember(&vals);
    crate::shell::with_shell(|sh| {
        for (name, value, set) in vals {
            if let Some(p) = sh.params.get_mut(name) {
                if p.attrs & crate::params::SPECIAL == 0 {
                    continue;
                }
                p.value = value;
                if set {
                    p.attrs &= !UNSET;
                } else {
                    p.attrs |= UNSET;
                }
            }
        }
    });
}

/// Reads the special parameters back into the editor.
pub fn pull(z: &mut Zle) {
    if DEPTH.load(Ordering::SeqCst) == 0 {
        return;
    }
    let get = |sh: &Shell, n: &str| -> Option<Param> {
        sh.params
            .get(n)
            .filter(|p| p.attrs & crate::params::SPECIAL != 0)
            .cloned()
    };
    let (buffer, cursor, mark, ra, cut, kr, rh, pre, post, num, hist, ulimit, ys, ye, regs) =
        crate::shell::with_shell(|sh| {
            (
                get(sh, "BUFFER"),
                get(sh, "CURSOR"),
                get(sh, "MARK"),
                get(sh, "REGION_ACTIVE"),
                get(sh, "CUTBUFFER"),
                get(sh, "killring"),
                get(sh, "region_highlight"),
                get(sh, "PREDISPLAY"),
                get(sh, "POSTDISPLAY"),
                get(sh, "NUMERIC"),
                get(sh, "HISTNO"),
                get(sh, "UNDO_LIMIT_NO"),
                get(sh, "YANK_START"),
                get(sh, "YANK_END"),
                get(sh, "registers"),
            )
        });
    let int = |p: &Option<Param>| -> Option<i64> {
        p.as_ref().and_then(|p| match &p.value {
            Value::Scalar(s) => s.trim().parse().ok().or_else(|| crate::arith::eval(s).ok()),
            _ => None,
        })
    };
    let text = |p: &Option<Param>| -> Option<String> {
        p.as_ref().map(|p| match &p.value {
            Value::Scalar(s) => s.clone(),
            Value::Array(a) => a.join(" "),
            Value::Assoc(_) => String::new(),
        })
    };
    let pushed = PUSHED.with(|x| x.borrow().clone());
    if let Some(h) = int(&hist) {
        if h != pushed.histno {
            widgets::hist::zle_goto_hist(z, h.max(0) as usize);
        }
    }
    if let Some(b) = text(&buffer) {
        let new: Vec<char> = b.chars().collect();
        if new != z.line {
            z.line = new;
            z.viinsbegin = 0;
        }
    }
    if let Some(c) = int(&cursor) {
        z.cs = c.clamp(0, z.line.len() as i64) as usize;
    }
    if z.cs > z.line.len() {
        z.cs = z.line.len();
    }
    if let Some(m) = int(&mark) {
        z.mark = m.clamp(0, z.line.len() as i64) as usize;
    }
    if let Some(r) = int(&ra) {
        z.region_active = if r != 0 { r.clamp(1, 2) as i32 } else { 0 };
    }
    if let Some(c) = text(&cut) {
        if c != pushed.cutbuffer {
            z.cutbuf = CutBuffer {
                buf: c.chars().collect(),
                flags: 0,
                set: true,
            };
        }
    }
    if let Some(Param {
        value: Value::Array(a),
        ..
    }) = &kr
    {
        if *a != pushed.killring {
            z.kring.clear();
            z.kringnum = 0;
            z.kringsize = a.len();
            if !a.is_empty() {
                z.kring = vec![CutBuffer::default(); a.len()];
                let mut kpos = 0;
                for e in a {
                    z.kring[kpos] = CutBuffer {
                        buf: e.chars().collect(),
                        flags: 0,
                        set: true,
                    };
                    kpos = (kpos + a.len() - 1) % a.len();
                }
            }
        }
    }
    if let Some(p) = &rh {
        let a = match &p.value {
            Value::Array(a) => a.clone(),
            Value::Scalar(s) if !s.is_empty() => vec![s.clone()],
            _ => Vec::new(),
        };
        if a != pushed.region_highlight || p.attrs & UNSET != 0 {
            refresh::set_region_highlight(z, if p.attrs & UNSET != 0 { &[] } else { &a });
        }
    }
    if let Some(t) = text(&pre) {
        z.predisplay = t.chars().collect();
    }
    if let Some(t) = text(&post) {
        z.postdisplay = t.chars().collect();
    }
    match &num {
        Some(p) if p.attrs & UNSET != 0 => {
            if pushed.numeric.is_some() {
                z.zmod.flags &= !MOD_MULT;
                z.zmod.mult = 1;
            }
        }
        Some(_) => {
            if let Some(n) = int(&num) {
                if Some(n) != pushed.numeric {
                    z.zmod.mult = n;
                    z.zmod.flags |= MOD_MULT;
                }
            }
        }
        None => {}
    }
    if let Some(n) = int(&ulimit) {
        z.undo_limitno = n;
    }
    if let Some(n) = int(&ys) {
        z.yankb = n.clamp(0, z.line.len() as i64) as usize;
    }
    if let Some(n) = int(&ye) {
        z.yanke = n.clamp(0, z.line.len() as i64) as usize;
    }
    if let Some(Param {
        value: Value::Assoc(m),
        ..
    }) = &regs
    {
        if *m != pushed.registers {
            for (n, slot) in z.vibuf.iter_mut().enumerate().take(36) {
                let name = register_name(n);
                match m.get(&name) {
                    Some(v) => {
                        *slot = CutBuffer {
                            buf: v.chars().collect(),
                            flags: 0,
                            set: true,
                        }
                    }
                    None => *slot = CutBuffer::default(),
                }
            }
        }
    }
}

/// Called when the scope holding the parameters ends.
pub fn end_scope() {
    let _ = DEPTH.try_update(Ordering::SeqCst, Ordering::SeqCst, |d| d.checked_sub(1));
}

/// Whether an assignment to `name` needs [`reconcile`].
pub fn is_linked(name: &str) -> bool {
    DEPTH.load(Ordering::Relaxed) > 0
        && matches!(
            name,
            "BUFFER" | "LBUFFER" | "RBUFFER" | "CURSOR" | "MARK" | "REGION_ACTIVE"
        )
}

/// After `name` was assigned, brings the linked parameters in line.
pub fn reconcile(sh: &mut Shell, name: &str) {
    let special = |sh: &Shell, n: &str| {
        sh.params
            .get(n)
            .is_some_and(|p| p.attrs & crate::params::SPECIAL != 0)
    };
    if !special(sh, name) {
        return;
    }
    let text =
        |sh: &Shell, n: &str| -> String { sh.params.get(n).map(|p| p.text()).unwrap_or_default() };
    let int = |sh: &Shell, n: &str| -> i64 {
        let t = text(sh, n);
        t.trim().parse().unwrap_or(0)
    };
    let set = |sh: &mut Shell, n: &str, v: String| {
        if let Some(p) = sh.params.get_mut(n) {
            p.value = Value::Scalar(v);
            p.attrs &= !UNSET;
        }
    };
    let buf: Vec<char> = text(sh, "BUFFER").chars().collect();
    let (line, cs) = match name {
        "BUFFER" => {
            let cs = (int(sh, "CURSOR").max(0) as usize).min(buf.len());
            (buf, cs)
        }
        "LBUFFER" => {
            let l: Vec<char> = text(sh, "LBUFFER").chars().collect();
            let r: Vec<char> = text(sh, "RBUFFER").chars().collect();
            let cs = l.len();
            ([l, r].concat(), cs)
        }
        "RBUFFER" => {
            let cs = (int(sh, "CURSOR").max(0) as usize).min(buf.len());
            let r: Vec<char> = text(sh, "RBUFFER").chars().collect();
            ([buf[..cs].to_vec(), r].concat(), cs)
        }
        "CURSOR" => {
            let cs = int(sh, "CURSOR").clamp(0, buf.len() as i64) as usize;
            (buf, cs)
        }
        "MARK" => {
            let m = int(sh, "MARK").clamp(0, buf.len() as i64);
            set(sh, "MARK", m.to_string());
            return;
        }
        "REGION_ACTIVE" => {
            let r = i64::from(int(sh, "REGION_ACTIVE") != 0);
            set(sh, "REGION_ACTIVE", r.to_string());
            return;
        }
        _ => return,
    };
    set(sh, "BUFFER", line.iter().collect());
    set(sh, "CURSOR", cs.to_string());
    set(sh, "LBUFFER", line[..cs].iter().collect());
    set(sh, "RBUFFER", line[cs..].iter().collect());
    let lines = 1 + line.iter().filter(|c| **c == '\n').count();
    set(sh, "BUFFERLINES", lines.to_string());
}
