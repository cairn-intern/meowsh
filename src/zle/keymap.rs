//! Keymaps and key bindings, after zsh's zle_keymap.c.
//!
//! A keymap maps byte sequences to a binding: a widget name or a string
//! to push back onto the input. A sequence missing from the map is bound
//! to undefined-key. Several names may share one keymap (`bindkey -A`).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

#[derive(Clone, Debug, PartialEq)]
pub enum Bind {
    Widget(String),
    Str(Vec<u8>),
}

#[derive(Clone, Debug, Default)]
pub struct Keymap {
    pub map: BTreeMap<Vec<u8>, Bind>,
    pub immutable: bool,
    /// The first name this keymap got (other than "main").
    pub primary: Option<String>,
}

pub type KeymapRef = Rc<RefCell<Keymap>>;

struct KeymapName {
    km: KeymapRef,
    immortal: bool,
}

#[derive(Default)]
pub struct Keymaps {
    names: BTreeMap<String, KeymapName>,
}

thread_local! {
    static KEYMAPS: RefCell<Option<Keymaps>> = const { RefCell::new(None) };
}

/// Runs `f` on the keymap table, creating the default keymaps first.
pub fn with_keymaps<R>(f: impl FnOnce(&mut Keymaps) -> R) -> R {
    KEYMAPS.with(|k| {
        let mut k = k.borrow_mut();
        if k.is_none() {
            let mut t = Keymaps::default();
            t.default_bindings();
            *k = Some(t);
        }
        f(k.as_mut().unwrap())
    })
}

pub fn open_keymap(name: &str) -> Option<KeymapRef> {
    with_keymaps(|t| t.open(name))
}

impl Keymap {
    /// The binding of `seq`; None is undefined-key.
    pub fn keybind(&self, seq: &[u8]) -> Option<&Bind> {
        self.map.get(seq)
    }

    /// Whether `seq` is a proper prefix of a bound sequence.
    pub fn is_prefix(&self, seq: &[u8]) -> bool {
        if seq.is_empty() {
            return true;
        }
        use std::ops::Bound;
        self.map
            .range::<[u8], _>((Bound::Excluded(seq), Bound::Unbounded))
            .next()
            .is_some_and(|(k, _)| k.starts_with(seq))
    }

    /// Binds `seq`; `None` unbinds it. Returns 1 for an immutable
    /// keymap and 2 for an empty sequence, as zsh's bindkey() does.
    pub fn bind(&mut self, seq: &[u8], b: Option<Bind>) -> i32 {
        if self.immutable {
            return 1;
        }
        if seq.is_empty() {
            return 2;
        }
        match b {
            Some(Bind::Widget(w)) if w == "undefined-key" => {
                self.map.remove(seq);
            }
            Some(b) => {
                self.map.insert(seq.to_vec(), b);
            }
            None => {
                self.map.remove(seq);
            }
        }
        0
    }

    pub fn bind_widget(&mut self, seq: &[u8], w: &str) {
        self.bind(seq, Some(Bind::Widget(w.to_string())));
    }
}

impl Keymaps {
    pub fn open(&self, name: &str) -> Option<KeymapRef> {
        self.names.get(name).map(|n| n.km.clone())
    }

    pub fn names(&self) -> Vec<String> {
        self.names.keys().cloned().collect()
    }

    /// Links `km` under `name`; 1 if the name is protected.
    pub fn link(&mut self, km: KeymapRef, name: &str, immortal: bool) -> i32 {
        if let Some(n) = self.names.get(name) {
            if n.immortal {
                return 1;
            }
            if Rc::ptr_eq(&n.km, &km) {
                return 0;
            }
            let old = n.km.clone();
            self.names.insert(
                name.to_string(),
                KeymapName {
                    km: km.clone(),
                    immortal: false,
                },
            );
            self.fix_primary(&old, name);
        } else {
            self.names.insert(
                name.to_string(),
                KeymapName {
                    km: km.clone(),
                    immortal,
                },
            );
        }
        let mut k = km.borrow_mut();
        if k.primary.is_none() && name != "main" {
            k.primary = Some(name.to_string());
        }
        0
    }

    // `km` lost its name `gone`: when that was its primary name, the
    // first other name (not "main") becomes it.
    fn fix_primary(&mut self, km: &KeymapRef, gone: &str) {
        if km.borrow().primary.as_deref() != Some(gone) {
            return;
        }
        let next = self
            .names
            .iter()
            .find(|(n, e)| n.as_str() != "main" && Rc::ptr_eq(&e.km, km))
            .map(|(n, _)| n.clone());
        km.borrow_mut().primary = next;
    }

    /// 0 removed, 1 protected, 2 no such keymap.
    pub fn unlink(&mut self, name: &str, ignore_immortal: bool) -> i32 {
        match self.names.get(name) {
            None => 2,
            Some(n) if n.immortal && !ignore_immortal => 1,
            Some(_) => {
                let n = self.names.remove(name).unwrap();
                self.fix_primary(&n.km, name);
                0
            }
        }
    }

    pub fn new_keymap(copy: Option<&KeymapRef>) -> KeymapRef {
        let mut km = Keymap::default();
        if let Some(c) = copy {
            km.map = c.borrow().map.clone();
        }
        Rc::new(RefCell::new(km))
    }

    pub fn primary_name(&self, name: &str) -> Option<String> {
        self.names
            .get(name)
            .and_then(|n| n.km.borrow().primary.clone())
    }

    fn default_bindings(&mut self) {
        self.names.clear();
        let vmap = Self::new_keymap(None);
        let emap = Self::new_keymap(None);
        let amap = Self::new_keymap(None);
        let oppmap = Self::new_keymap(None);
        let vismap = Self::new_keymap(None);
        let smap = Self::new_keymap(None);
        {
            let mut v = vmap.borrow_mut();
            let mut e = emap.borrow_mut();
            for i in 0..32u8 {
                v.bind_widget(&[i], VIINSBIND[i as usize]);
                e.bind_widget(&[i], EMACSBIND[i as usize]);
            }
            for i in 32..=255u8 {
                v.bind_widget(&[i], "self-insert");
                e.bind_widget(&[i], "self-insert");
            }
            v.bind_widget(&[127], VIINSBIND[8]);
            e.bind_widget(&[127], EMACSBIND[8]);
            v.map
                .retain(|_, b| *b != Bind::Widget("undefined-key".into()));
            e.map
                .retain(|_, b| *b != Bind::Widget("undefined-key".into()));
        }
        {
            let mut a = amap.borrow_mut();
            for i in 0..128u8 {
                a.bind_widget(&[i], VICMDBIND[i as usize]);
            }
            a.map
                .retain(|_, b| *b != Bind::Widget("undefined-key".into()));
        }
        {
            let mut s = smap.borrow_mut();
            for i in 0..=255u8 {
                s.bind_widget(&[i], ".self-insert");
            }
            s.bind_widget(b"\n", ".accept-line");
            s.bind_widget(b"\r", ".accept-line");
        }
        let cursor = |km: &KeymapRef, up: &str, down: &str, left: &str, right: &str| {
            let mut k = km.borrow_mut();
            for (c, w) in [('A', up), ('B', down), ('D', left), ('C', right)] {
                k.bind_widget(format!("\x1b[{}", c).as_bytes(), w);
                k.bind_widget(format!("\x1bO{}", c).as_bytes(), w);
            }
        };
        for km in [&vmap, &amap] {
            cursor(
                km,
                "up-line-or-history",
                "down-line-or-history",
                "vi-backward-char",
                "vi-forward-char",
            );
        }
        for km in [&oppmap, &vismap] {
            {
                let mut k = km.borrow_mut();
                for (c, w) in [('A', "up-line"), ('B', "down-line")] {
                    k.bind_widget(format!("\x1b[{}", c).as_bytes(), w);
                    k.bind_widget(format!("\x1bO{}", c).as_bytes(), w);
                }
            }
            let mut k = km.borrow_mut();
            k.bind_widget(b"k", "up-line");
            k.bind_widget(b"j", "down-line");
            k.bind_widget(b"aa", "select-a-shell-word");
            k.bind_widget(b"ia", "select-in-shell-word");
            k.bind_widget(b"aw", "select-a-word");
            k.bind_widget(b"iw", "select-in-word");
            k.bind_widget(b"aW", "select-a-blank-word");
            k.bind_widget(b"iW", "select-in-blank-word");
        }
        oppmap.borrow_mut().bind_widget(b"\x1b", "vi-cmd-mode");
        {
            let mut k = vismap.borrow_mut();
            k.bind_widget(b"\x1b", "deactivate-region");
            k.bind_widget(b"o", "exchange-point-and-mark");
            k.bind_widget(b"p", "put-replace-selection");
            k.bind_widget(b"u", "vi-down-case");
            k.bind_widget(b"U", "vi-up-case");
            k.bind_widget(b"x", "vi-delete");
            k.bind_widget(b"~", "vi-oper-swap-case");
        }
        {
            let mut k = amap.borrow_mut();
            k.bind_widget(b"ga", "what-cursor-position");
            k.bind_widget(b"ge", "vi-backward-word-end");
            k.bind_widget(b"gE", "vi-backward-blank-word-end");
            k.bind_widget(b"gg", "beginning-of-buffer-or-history");
            k.bind_widget(b"gu", "vi-down-case");
            k.bind_widget(b"gU", "vi-up-case");
            k.bind_widget(b"g~", "vi-oper-swap-case");
            k.bind(b"g~~", Some(Bind::Str(b"g~g~".to_vec())));
            k.bind(b"guu", Some(Bind::Str(b"gugu".to_vec())));
            k.bind(b"gUU", Some(Bind::Str(b"gUgU".to_vec())));
        }
        cursor(
            &emap,
            "up-line-or-history",
            "down-line-or-history",
            "backward-char",
            "forward-char",
        );
        {
            let mut e = emap.borrow_mut();
            for (s, w) in [
                (&b"\x18*"[..], "expand-word"),
                (b"\x18g", "list-expand"),
                (b"\x18G", "list-expand"),
                (b"\x18\x0e", "infer-next-history"),
                (b"\x18\x0b", "kill-buffer"),
                (b"\x18\x06", "vi-find-next-char"),
                (b"\x18\x0f", "overwrite-mode"),
                (b"\x18\x15", "undo"),
                (b"\x18\x16", "vi-cmd-mode"),
                (b"\x18\x0a", "vi-join"),
                (b"\x18\x02", "vi-match-bracket"),
                (b"\x18s", "history-incremental-search-forward"),
                (b"\x18r", "history-incremental-search-backward"),
                (b"\x18u", "undo"),
                (b"\x18\x18", "exchange-point-and-mark"),
                (b"\x18=", "what-cursor-position"),
            ] {
                e.bind_widget(s, w);
            }
        }
        for km in [&emap, &vmap, &amap] {
            km.borrow_mut().bind_widget(b"\x1b[200~", "bracketed-paste");
        }
        {
            let mut e = emap.borrow_mut();
            for (i, w) in METABIND.iter().enumerate() {
                if *w != "undefined-key" {
                    e.bind_widget(&[0x1b, i as u8], w);
                }
            }
        }
        self.link(vmap.clone(), "viins", false);
        self.link(emap.clone(), "emacs", false);
        self.link(amap, "vicmd", false);
        self.link(oppmap, "viopp", false);
        self.link(vismap, "visual", false);
        self.link(smap.clone(), ".safe", true);
        if crate::options::is_set("vi") {
            self.link(vmap, "main", false);
        } else {
            self.link(emap, "main", false);
        }
        smap.borrow_mut().immutable = true;
        self.link(Self::new_keymap(None), "isearch", false);
        let cmd = Self::new_keymap(None);
        {
            let mut c = cmd.borrow_mut();
            c.bind_widget(b"\n", "accept-line");
            c.bind_widget(b"\r", "accept-line");
            c.bind_widget(&[7], "send-break");
        }
        self.link(cmd, "command", false);
    }
}

/// Links "main" to emacs or viins, as setting the vi or emacs option does.
pub fn set_keymap_mode(vi: bool) {
    with_keymaps(|t| {
        if let Some(km) = t.open(if vi { "viins" } else { "emacs" }) {
            t.link(km, "main", false);
        }
    });
}

/// Parses a key string as bindkey does: `\C-x`, `^X`, `\M-x`, `\e` and
/// the usual backslash escapes.
pub fn getkeystring(s: &str) -> Vec<u8> {
    let b: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut meta = false;
    let mut control = false;
    while i < b.len() {
        let c = b[i];
        let mut val: Option<u32> = None;
        if c == '^' && i + 1 < b.len() {
            control = true;
            i += 1;
            continue;
        } else if c == '\\' && i + 1 < b.len() {
            i += 1;
            let e = b[i];
            match e {
                'a' => val = Some(7),
                'b' => val = Some(8),
                'e' | 'E' => val = Some(27),
                'f' => val = Some(12),
                'n' => val = Some(10),
                'r' => val = Some(13),
                't' => val = Some(9),
                'v' => val = Some(11),
                'M' if b.get(i + 1) == Some(&'-') => {
                    meta = true;
                    i += 2;
                    continue;
                }
                'M' => {
                    meta = true;
                    i += 1;
                    continue;
                }
                'C' if b.get(i + 1) == Some(&'-') => {
                    control = true;
                    i += 2;
                    continue;
                }
                'C' => {
                    control = true;
                    i += 1;
                    continue;
                }
                'x' => {
                    let mut n = 0u32;
                    let mut k = 0;
                    while k < 2 && i + 1 < b.len() && b[i + 1].is_ascii_hexdigit() {
                        i += 1;
                        n = n * 16 + b[i].to_digit(16).unwrap();
                        k += 1;
                    }
                    val = Some(n);
                }
                'u' | 'U' => {
                    let max = if e == 'u' { 4 } else { 8 };
                    let mut n = 0u32;
                    let mut k = 0;
                    while k < max && i + 1 < b.len() && b[i + 1].is_ascii_hexdigit() {
                        i += 1;
                        n = n * 16 + b[i].to_digit(16).unwrap();
                        k += 1;
                    }
                    if let Some(ch) = char::from_u32(n) {
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    }
                    i += 1;
                    meta = false;
                    control = false;
                    continue;
                }
                '0'..='7' => {
                    let mut n = e.to_digit(8).unwrap();
                    let mut k = 1;
                    while k < 3 && i + 1 < b.len() && ('0'..='7').contains(&b[i + 1]) {
                        i += 1;
                        n = n * 8 + b[i].to_digit(8).unwrap();
                        k += 1;
                    }
                    val = Some(n);
                }
                _ => val = Some(e as u32),
            }
        }
        let mut bytes: Vec<u8> = match val {
            Some(v) => vec![v as u8],
            None => {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        };
        if control && bytes.len() == 1 {
            bytes[0] = if bytes[0] == b'?' {
                0x7f
            } else {
                bytes[0] & 0x9f
            };
        }
        if meta && bytes.len() == 1 {
            bytes[0] |= 0x80;
        }
        control = false;
        meta = false;
        out.extend(bytes);
        i += 1;
    }
    // A trailing lone `^` is literal.
    if control {
        out.push(b'^');
    }
    out
}

/// A key sequence as bindkey shows it, in double quotes.
pub fn bindztrdup(seq: &[u8]) -> String {
    let mut s = String::new();
    for &b in seq {
        let mut c = b;
        if c & 0x80 != 0 {
            s.push_str("\\M-");
            c &= 0x7f;
        }
        if c < 32 || c == 0x7f {
            s.push('^');
            c ^= 64;
        }
        if c == b'\\' || c == b'^' {
            s.push('\\');
        }
        s.push(c as char);
    }
    dquoted(&s)
}

fn dquoted(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        if matches!(c, '"' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// A name as zsh's quotedzputs writes it.
pub fn quoted(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || "_-./:+@%=,".contains(c))
    {
        s.to_string()
    } else {
        crate::typeset::quote_value(s)
    }
}

/// A name as zsh's nicezputs writes it: control characters as ^X.
pub fn nice(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let u = c as u32;
        if u < 32 {
            out.push('^');
            out.push(char::from_u32(u ^ 64).unwrap_or('?'));
        } else if u == 0x7f {
            out.push_str("^?");
        } else {
            out.push(c);
        }
    }
    out
}

fn warn(name: &str, msg: &str) {
    crate::shell::builtin_err(name, msg);
}

struct BindState<'a> {
    list: bool,
    all: bool,
    kmname: &'a str,
    first: Vec<u8>,
    last: Vec<u8>,
    bind: Option<Bind>,
    out: String,
}

impl BindState<'_> {
    fn flush(&mut self) {
        if self.bind.is_none() && !self.all {
            return;
        }
        let range = self.first != self.last;
        let o = &mut self.out;
        if self.list {
            let mut nodash = true;
            o.push_str("bindkey ");
            if range {
                o.push_str("-R ");
            }
            if matches!(self.bind, Some(Bind::Str(_))) {
                o.push_str("-s ");
            }
            if self.kmname == "main" {
            } else if self.kmname == "vicmd" {
                o.push_str("-a ");
            } else {
                o.push_str("-M ");
                o.push_str(&quoted(self.kmname));
                o.push(' ');
                nodash = false;
            }
            if nodash && self.first.first() == Some(&b'-') {
                o.push_str("-- ");
            }
        }
        o.push_str(&bindztrdup(&self.first));
        if range {
            o.push('-');
            o.push_str(&bindztrdup(&self.last));
        }
        o.push(' ');
        match &self.bind {
            Some(Bind::Widget(w)) => {
                o.push_str(&if self.list { quoted(w) } else { nice(w) });
            }
            Some(Bind::Str(s)) => o.push_str(&bindztrdup(s)),
            None => o.push_str("undefined-key"),
        }
        o.push('\n');
    }

    fn scan(&mut self, seq: &[u8], bind: &Bind) {
        if Some(bind) == self.bind.as_ref()
            && seq.len() == 1
            && self.last.len() == 1
            && seq[0] as u32 == self.last[0] as u32 + 1
        {
            self.last = seq.to_vec();
            return;
        }
        self.flush();
        self.first = seq.to_vec();
        self.last = seq.to_vec();
        self.bind = Some(bind.clone());
    }
}

/// The bindkey builtin.
pub fn builtin_bindkey(words: &[String]) -> i32 {
    let name = "bindkey";
    let mut opts = std::collections::HashSet::new();
    let mut m_arg: Option<String> = None;
    let mut i = 1;
    while i < words.len() {
        let w = &words[i];
        if w == "--" {
            i += 1;
            break;
        }
        if !w.starts_with('-') || w == "-" {
            break;
        }
        let cs: Vec<char> = w[1..].chars().collect();
        let mut j = 0;
        while j < cs.len() {
            let c = cs[j];
            if !"evaMldDANmrsLRp".contains(c) {
                warn(name, &format!("bad option: -{}", c));
                return 1;
            }
            if c == 'M' {
                let rest: String = cs[j + 1..].iter().collect();
                if !rest.is_empty() {
                    m_arg = Some(rest);
                } else {
                    i += 1;
                    match words.get(i) {
                        Some(a) => m_arg = Some(a.clone()),
                        None => {
                            warn(name, "argument expected: -M");
                            return 1;
                        }
                    }
                }
                opts.insert('M');
                break;
            }
            opts.insert(c);
            j += 1;
        }
        i += 1;
    }
    let argv: Vec<String> = words[i..].to_vec();
    let isset = |c: char| opts.contains(&c);
    let ops: [(char, bool, usize, i32); 9] = [
        ('l', false, 0, -1),
        ('d', false, 0, 0),
        ('D', false, 1, -1),
        ('A', false, 2, 2),
        ('N', false, 1, 2),
        ('m', true, 0, 0),
        ('r', true, 1, -1),
        ('s', true, 2, -1),
        ('\0', true, 0, -1),
    ];
    let mut op = ops.len() - 1;
    for (k, o) in ops.iter().enumerate().take(ops.len() - 1) {
        if isset(o.0) {
            op = k;
            break;
        }
    }
    if ops[op].0 != '\0' {
        for o in &ops[op + 1..ops.len() - 1] {
            if isset(o.0) {
                warn(name, "incompatible operation selection options");
                return 1;
            }
        }
    }
    let (opc, selp, min, max) = ops[op];
    let n = ['e', 'v', 'a', 'M'].iter().filter(|c| isset(**c)).count();
    if !selp && n > 0 {
        warn(name, &format!("keymap cannot be selected with -{}", opc));
        return 1;
    }
    if n > 1 {
        warn(name, "incompatible keymap selection options");
        return 1;
    }
    let mut kmname = String::new();
    let mut km: Option<KeymapRef> = None;
    if selp {
        kmname = if isset('e') {
            "emacs".into()
        } else if isset('v') {
            "viins".into()
        } else if isset('a') {
            "vicmd".into()
        } else if let Some(m) = &m_arg {
            m.clone()
        } else {
            "main".into()
        };
        km = open_keymap(&kmname);
        let Some(k) = &km else {
            warn(name, &format!("no such keymap `{}'", kmname));
            return 1;
        };
        if isset('e') || isset('v') {
            let k = k.clone();
            with_keymaps(|t| t.link(k, "main", false));
        }
    }
    if opc == '\0' && argv.len() < 2 {
        if isset('e') || isset('v') {
            return 0;
        }
        return bindkey_list(&kmname, km.as_ref().unwrap(), &argv, isset('L'), isset('p'));
    }
    if argv.len() < min {
        warn(name, &format!("not enough arguments for -{}", opc));
        return 1;
    }
    if max != -1 && argv.len() > max as usize {
        warn(name, &format!("too many arguments for -{}", opc));
        return 1;
    }
    match opc {
        'l' => {
            let mut out = String::new();
            let mut ret = 0;
            let list = |n: &str, out: &mut String| {
                with_keymaps(|t| {
                    if isset('L') {
                        if n == ".safe" {
                            return;
                        }
                        let km = t.open(n).unwrap();
                        out.push_str("bindkey -");
                        let p = km.borrow().primary.clone();
                        match p {
                            Some(p) if p != n => {
                                out.push_str("A ");
                                if p.starts_with('-') {
                                    out.push_str("-- ");
                                }
                                out.push_str(&quoted(&p));
                                out.push(' ');
                            }
                            _ => {
                                out.push_str("N ");
                                if n.starts_with('-') {
                                    out.push_str("-- ");
                                }
                            }
                        }
                        out.push_str(&quoted(n));
                    } else {
                        out.push_str(&nice(n));
                    }
                    out.push('\n');
                })
            };
            if argv.is_empty() {
                for n in with_keymaps(|t| t.names()) {
                    list(&n, &mut out);
                }
            } else {
                for a in &argv {
                    if open_keymap(a).is_none() {
                        warn(name, &format!("no such keymap: `{}'", a));
                        ret = 1;
                    } else {
                        list(a, &mut out);
                    }
                }
            }
            crate::sys::out(&out);
            ret
        }
        'd' => {
            with_keymaps(|t| t.default_bindings());
            0
        }
        'D' => {
            let mut ret = 0;
            for a in &argv {
                match with_keymaps(|t| t.unlink(a, false)) {
                    1 => {
                        warn(name, &format!("keymap name `{}' is protected", a));
                        ret = 1;
                    }
                    2 => {
                        warn(name, &format!("no such keymap `{}'", a));
                        ret = 1;
                    }
                    _ => {}
                }
            }
            ret
        }
        'A' => {
            let Some(k) = open_keymap(&argv[0]) else {
                warn(name, &format!("no such keymap `{}'", argv[0]));
                return 1;
            };
            if with_keymaps(|t| t.link(k, &argv[1], false)) != 0 {
                warn(name, &format!("keymap name `{}' is protected", argv[1]));
                return 1;
            }
            0
        }
        'N' => {
            let prot = with_keymaps(|t| t.names.get(&argv[0]).is_some_and(|n| n.immortal));
            if prot {
                warn(name, &format!("keymap name `{}' is protected", argv[0]));
                return 1;
            }
            let copy = match argv.get(1) {
                Some(src) => match open_keymap(src) {
                    Some(k) => Some(k),
                    None => {
                        warn(name, &format!("no such keymap `{}'", src));
                        return 1;
                    }
                },
                None => None,
            };
            let new = Keymaps::new_keymap(copy.as_ref());
            with_keymaps(|t| t.link(new, &argv[0], false));
            0
        }
        'm' => {
            let km = km.unwrap();
            if km.borrow().immutable {
                warn(name, &format!("keymap `{}' is protected", kmname));
                return 1;
            }
            warn(name, "warning: `bindkey -m' disables multibyte support");
            let mut k = km.borrow_mut();
            for i in 128..=255u8 {
                let w = METABIND[(i - 128) as usize];
                if w != "undefined-key" {
                    let cur = k.keybind(&[i]).cloned();
                    if matches!(&cur, Some(Bind::Widget(x)) if x == "self-insert") || cur.is_none()
                    {
                        k.bind_widget(&[i], w);
                    }
                }
            }
            0
        }
        _ => bindkey_bind(
            name,
            &kmname,
            km.unwrap(),
            &argv,
            opc,
            isset('R'),
            isset('p'),
        ),
    }
}

fn bindkey_bind(
    name: &str,
    kmname: &str,
    km: KeymapRef,
    argv: &[String],
    func: char,
    range: bool,
    prefix: bool,
) -> i32 {
    if (func == '\0' || func == 's') && !argv.len().is_multiple_of(2) {
        warn(name, "even number of arguments required");
        return 1;
    }
    if km.borrow().immutable {
        warn(name, &format!("keymap `{}' is protected", kmname));
        return 1;
    }
    let mut km = km.borrow_mut();
    if func == 'r' && prefix {
        for a in argv {
            let p = getkeystring(a);
            let keys: Vec<Vec<u8>> = km
                .map
                .keys()
                .filter(|k| k.len() > p.len() && k.starts_with(&p))
                .cloned()
                .collect();
            for k in keys {
                km.map.remove(&k);
            }
        }
        return 0;
    }
    let mut ret = 0;
    let mut i = 0;
    while i < argv.len() {
        let useq = &argv[i];
        let b = match func {
            'r' => None,
            's' => {
                i += 1;
                Some(Bind::Str(getkeystring(&argv[i])))
            }
            _ => {
                i += 1;
                Some(Bind::Widget(argv[i].clone()))
            }
        };
        let seq = getkeystring(useq);
        if range {
            let len = seq.len();
            if len < 2 || len > 2 + usize::from(seq[1] == b'-') || seq[0] > seq[len - 1] {
                warn(name, &format!("malformed key range `{}'", useq));
                ret = 1;
            } else {
                for c in seq[0]..=seq[len - 1] {
                    km.bind(&[c], b.clone());
                }
            }
        } else if km.bind(&seq, b) != 0 {
            warn(name, "cannot bind to an empty key sequence");
            ret = 1;
        }
        i += 1;
    }
    ret
}

fn bindkey_list(kmname: &str, km: &KeymapRef, argv: &[String], list: bool, prefix: bool) -> i32 {
    let km = km.borrow();
    let mut bs = BindState {
        list,
        all: false,
        kmname,
        first: Vec::new(),
        last: Vec::new(),
        bind: None,
        out: String::new(),
    };
    if let (Some(a), false) = (argv.first(), prefix) {
        let seq = getkeystring(a);
        bs.all = true;
        bs.first = seq.clone();
        bs.last = seq.clone();
        bs.bind = km.keybind(&seq).cloned();
        bs.flush();
    } else {
        let pfx = if prefix {
            match argv.first() {
                None => {
                    warn("bindkey", "option -p requires a prefix string");
                    return 1;
                }
                Some(a) if a.is_empty() => None,
                Some(a) => Some(getkeystring(a)),
            }
        } else {
            None
        };
        for (seq, b) in km.map.iter() {
            if let Some(p) = &pfx {
                if !(seq.len() > p.len() && seq.starts_with(p)) {
                    continue;
                }
            }
            bs.scan(seq, b);
        }
        bs.flush();
    }
    crate::sys::out(&bs.out);
    0
}

pub const EMACSBIND: [&str; 32] = [
    "set-mark-command",
    "beginning-of-line",
    "backward-char",
    "undefined-key",
    "delete-char-or-list",
    "end-of-line",
    "forward-char",
    "send-break",
    "backward-delete-char",
    "expand-or-complete",
    "accept-line",
    "kill-line",
    "clear-screen",
    "accept-line",
    "down-line-or-history",
    "accept-line-and-down-history",
    "up-line-or-history",
    "push-line",
    "history-incremental-search-backward",
    "history-incremental-search-forward",
    "transpose-chars",
    "kill-whole-line",
    "quoted-insert",
    "backward-kill-word",
    "undefined-key",
    "yank",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undo",
];

pub const VIINSBIND: [&str; 32] = [
    "undefined-key",
    "self-insert",
    "self-insert",
    "self-insert",
    "list-choices",
    "self-insert",
    "self-insert",
    "list-expand",
    "vi-backward-delete-char",
    "expand-or-complete",
    "accept-line",
    "self-insert",
    "clear-screen",
    "accept-line",
    "self-insert",
    "self-insert",
    "self-insert",
    "vi-quoted-insert",
    "redisplay",
    "self-insert",
    "self-insert",
    "vi-kill-line",
    "vi-quoted-insert",
    "vi-backward-kill-word",
    "undefined-key",
    "self-insert",
    "self-insert",
    "vi-cmd-mode",
    "self-insert",
    "self-insert",
    "self-insert",
    "self-insert",
];

pub const METABIND: [&str; 128] = [
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "list-choices",
    "undefined-key",
    "undefined-key",
    "send-break",
    "backward-kill-word",
    "self-insert-unmeta",
    "self-insert-unmeta",
    "undefined-key",
    "clear-screen",
    "self-insert-unmeta",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "copy-prev-word",
    "expand-history",
    "expand-history",
    "quote-region",
    "undefined-key",
    "spell-word",
    "undefined-key",
    "undefined-key",
    "quote-line",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "neg-argument",
    "insert-last-word",
    "undefined-key",
    "digit-argument",
    "digit-argument",
    "digit-argument",
    "digit-argument",
    "digit-argument",
    "digit-argument",
    "digit-argument",
    "digit-argument",
    "digit-argument",
    "digit-argument",
    "undefined-key",
    "undefined-key",
    "beginning-of-buffer-or-history",
    "undefined-key",
    "end-of-buffer-or-history",
    "which-command",
    "undefined-key",
    "accept-and-hold",
    "backward-word",
    "capitalize-word",
    "kill-word",
    "undefined-key",
    "forward-word",
    "get-line",
    "run-help",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "down-case-word",
    "undefined-key",
    "history-search-forward",
    "undefined-key",
    "history-search-backward",
    "push-line",
    "undefined-key",
    "spell-word",
    "transpose-words",
    "up-case-word",
    "undefined-key",
    "copy-region-as-kill",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "insert-last-word",
    "undefined-key",
    "accept-and-hold",
    "backward-word",
    "capitalize-word",
    "kill-word",
    "undefined-key",
    "forward-word",
    "get-line",
    "run-help",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "down-case-word",
    "undefined-key",
    "history-search-forward",
    "undefined-key",
    "history-search-backward",
    "push-line",
    "undefined-key",
    "spell-word",
    "transpose-words",
    "up-case-word",
    "undefined-key",
    "copy-region-as-kill",
    "execute-named-cmd",
    "yank-pop",
    "execute-last-named-cmd",
    "undefined-key",
    "vi-goto-column",
    "undefined-key",
    "undefined-key",
    "backward-kill-word",
];

pub const VICMDBIND: [&str; 128] = [
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "list-choices",
    "undefined-key",
    "undefined-key",
    "list-expand",
    "vi-backward-char",
    "undefined-key",
    "accept-line",
    "undefined-key",
    "clear-screen",
    "accept-line",
    "down-history",
    "undefined-key",
    "up-history",
    "undefined-key",
    "redo",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "beep",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "vi-forward-char",
    "undefined-key",
    "vi-set-buffer",
    "pound-insert",
    "vi-end-of-line",
    "vi-match-bracket",
    "undefined-key",
    "vi-goto-mark-line",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "vi-down-line-or-history",
    "vi-rev-repeat-find",
    "vi-up-line-or-history",
    "vi-repeat-change",
    "vi-history-search-backward",
    "vi-digit-or-beginning-of-line",
    "digit-argument",
    "digit-argument",
    "digit-argument",
    "digit-argument",
    "digit-argument",
    "digit-argument",
    "digit-argument",
    "digit-argument",
    "digit-argument",
    "execute-named-cmd",
    "vi-repeat-find",
    "vi-unindent",
    "list-choices",
    "vi-indent",
    "vi-history-search-forward",
    "undefined-key",
    "vi-add-eol",
    "vi-backward-blank-word",
    "vi-change-eol",
    "vi-kill-eol",
    "vi-forward-blank-word-end",
    "vi-find-prev-char",
    "vi-fetch-history",
    "undefined-key",
    "vi-insert-bol",
    "vi-join",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "vi-rev-repeat-search",
    "vi-open-line-above",
    "vi-put-before",
    "undefined-key",
    "vi-replace",
    "vi-change-whole-line",
    "vi-find-prev-char-skip",
    "undefined-key",
    "visual-line-mode",
    "vi-forward-blank-word",
    "vi-backward-delete-char",
    "vi-yank-whole-line",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "undefined-key",
    "vi-first-non-blank",
    "vi-first-non-blank",
    "vi-goto-mark",
    "vi-add-next",
    "vi-backward-word",
    "vi-change",
    "vi-delete",
    "vi-forward-word-end",
    "vi-find-next-char",
    "undefined-key",
    "vi-backward-char",
    "vi-insert",
    "down-line-or-history",
    "up-line-or-history",
    "vi-forward-char",
    "vi-set-mark",
    "vi-repeat-search",
    "vi-open-line-below",
    "vi-put-after",
    "undefined-key",
    "vi-replace-chars",
    "vi-substitute",
    "vi-find-next-char-skip",
    "undo",
    "visual-mode",
    "vi-forward-word",
    "vi-delete-char",
    "vi-yank",
    "undefined-key",
    "undefined-key",
    "vi-goto-column",
    "undefined-key",
    "vi-swap-case",
    "vi-backward-char",
];
