//! zsh shell options: the full option table, names with `no` prefixes,
//! aliases and single letters, emulation defaults, and `$options`.
//!
//! Option states live in `Shell::named_opts`, keyed by canonical name;
//! the options the executor checks often are mirrored as bits in
//! `Shell::opts`, and every option is mirrored into the `options`
//! associative array as `on`/`off`.

use crate::types::{
    Shell, OPT_ALLEXPORT, OPT_ERREXIT, OPT_HASHALL, OPT_INTERACTIVE, OPT_MONITOR, OPT_NOCLOBBER,
    OPT_NOEXEC, OPT_NOGLOB, OPT_NOUNSET, OPT_PIPEFAIL, OPT_VERBOSE, OPT_XTRACE,
};

/// zsh's sticky emulation: the emulation (with -R and any -o options)
/// an `emulate -c` ran under; functions defined there keep it.
pub type Sticky = (Emulation, bool, Vec<(String, bool)>);

/// The sticky emulation in effect now.
pub static CUR_STICKY: std::sync::Mutex<Option<Sticky>> = std::sync::Mutex::new(None);

/// Functions with a sticky emulation, by name.
pub static STICKY_FUNCS: std::sync::Mutex<Vec<(String, Sticky)>> =
    std::sync::Mutex::new(Vec::new());

pub fn sticky_of(name: &str) -> Option<Sticky> {
    STICKY_FUNCS
        .lock()
        .ok()?
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, s)| s.clone())
}

pub fn set_sticky(name: &str) {
    let cur = CUR_STICKY.lock().ok().and_then(|c| c.clone());
    if let Ok(mut f) = STICKY_FUNCS.lock() {
        f.retain(|(n, _)| n != name);
        if let Some(s) = cur {
            f.push((name.to_string(), s));
        }
    }
}

/// Puts the shell into a sticky emulation.
pub fn apply_sticky(st: &Sticky) {
    crate::shell::with_shell(|s| emulate_in(s, st.0, st.1));
    for (name, on) in &st.2 {
        set(name, *on);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Emulation {
    Csh = 1,
    Ksh = 2,
    Sh = 4,
    Zsh = 8,
}

impl Emulation {
    pub fn from_name(name: &str) -> Option<Emulation> {
        // zsh looks at the first letter, after an optional `r`.
        let n = name.rsplit('/').next().unwrap_or(name);
        let n = n.strip_prefix('-').unwrap_or(n);
        let n = if n.len() > 1 && n.starts_with('r') {
            &n[1..]
        } else {
            n
        };
        match n.chars().next()? {
            'c' if n.starts_with("csh") || n == "c" => Some(Emulation::Csh),
            'k' => Some(Emulation::Ksh),
            's' | 'b' => Some(Emulation::Sh),
            'z' => Some(Emulation::Zsh),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Emulation::Csh => "csh",
            Emulation::Ksh => "ksh",
            Emulation::Sh => "sh",
            Emulation::Zsh => "zsh",
        }
    }
}

// Options mirrored as bits: (name, bit, bit set when the option is off).
const BITS: &[(&str, u32, bool)] = &[
    ("allexport", OPT_ALLEXPORT, false),
    ("errexit", OPT_ERREXIT, false),
    ("glob", OPT_NOGLOB, true),
    ("hashcmds", OPT_HASHALL, false),
    ("interactive", OPT_INTERACTIVE, false),
    ("monitor", OPT_MONITOR, false),
    ("exec", OPT_NOEXEC, true),
    ("unset", OPT_NOUNSET, true),
    ("verbose", OPT_VERBOSE, false),
    ("xtrace", OPT_XTRACE, false),
    ("clobber", OPT_NOCLOBBER, true),
    ("pipefail", OPT_PIPEFAIL, false),
];

fn normalize(name: &str) -> String {
    name.chars()
        .filter(|&c| c != '_' && c != '-')
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn canonical(n: &str) -> Option<&'static str> {
    OPTIONS.iter().find(|o| o.0 == n).map(|o| o.0)
}

/// Resolves an option name as `setopt` takes it: any case, `_` ignored,
/// optional `no` prefix, aliases. Returns the option and the value that
/// setting this name gives it.
pub fn resolve(name: &str) -> Option<(&'static str, bool)> {
    let n = normalize(name);
    let lookup = |n: &str| -> Option<(&'static str, bool)> {
        if let Some(c) = canonical(n) {
            return Some((c, true));
        }
        ALIASES
            .iter()
            .find(|a| a.0 == n)
            .and_then(|a| canonical(a.1).map(|c| (c, !a.2)))
    };
    if let Some(r) = lookup(&n) {
        return Some(r);
    }
    n.strip_prefix("no").and_then(lookup).map(|(c, v)| (c, !v))
}

// Options read while the shell lock is already held (formatting a
// parameter's value): a copy behind its own lock, which is never held
// while taking the shell lock.
static QUICK: std::sync::RwLock<Vec<&'static str>> = std::sync::RwLock::new(Vec::new());
const QUICK_NAMES: &[&str] = &[
    "kshglob",
    "cbases",
    "octalzeroes",
    "extendedglob",
    "ksharrays",
    "kshzerosubscript",
    "posixidentifiers",
    "shglob",
];

/// For code that may run with the shell lock held: whether one of a few
/// display options (cbases, octalzeroes) is on.
pub fn quick(name: &str) -> bool {
    QUICK.read().map(|q| q.contains(&name)).unwrap_or(false)
}

/// Sets option `name` (canonical) in `s`, keeping the mirrors in step.
pub fn set_in(s: &mut Shell, name: &str, on: bool) {
    if let Some(&q) = QUICK_NAMES.iter().find(|&&q| q == name) {
        if let Ok(mut v) = QUICK.write() {
            v.retain(|&x| x != q);
            if on {
                v.push(q);
            }
        }
    }
    s.named_opts.insert(name.to_string(), on);
    if let Some(&(_, bit, inverted)) = BITS.iter().find(|b| b.0 == name) {
        if on != inverted {
            s.opts |= bit;
        } else {
            s.opts &= !bit;
        }
    }
    s.assoc_entry("options")
        .insert(name.to_string(), if on { "on" } else { "off" }.to_string());
}

/// Puts back a saved option state (LOCAL_OPTIONS, `emulate cmd`),
/// keeping the mirrors and `$options` in step.
pub fn restore_in(s: &mut Shell, named: &std::collections::HashMap<String, bool>) {
    for (k, &v) in named {
        if is_set_in(s, k) != v {
            set_in(s, k, v);
        }
    }
}

pub fn is_set_in(s: &Shell, name: &str) -> bool {
    s.named_opts.get(name).copied().unwrap_or(false)
}

/// True if option `name` (canonical) is on.
pub fn is_set(name: &str) -> bool {
    crate::shell::with_shell(|s| is_set_in(s, name))
}

/// Sets an option by any accepted spelling. False if there is no such
/// option.
pub fn set(name: &str, on: bool) -> bool {
    match resolve(name) {
        Some((c, v)) => {
            crate::shell::with_shell(|s| set_in(s, c, if on { v } else { !v }));
            true
        }
        None => false,
    }
}

/// Puts options into the state of emulation `emu`. With `all`, every
/// option is reset; otherwise only those zsh marks as emulation options.
/// Special options (interactive, login, ...) keep their values.
pub fn emulate_in(s: &mut Shell, emu: Emulation, all: bool) {
    for &(name, defaults, emulated, special) in OPTIONS {
        if special || !(all || emulated) {
            continue;
        }
        set_in(s, name, defaults & emu as u8 != 0);
    }
    s.emulation = emu;
}

/// Initial option state for a shell started as `emu`.
pub fn init(s: &mut Shell, emu: Emulation) {
    for &(name, defaults, _, _) in OPTIONS {
        set_in(s, name, defaults & emu as u8 != 0);
    }
    // Started as sh or bash, meowsh keeps brace expansion as bash does
    // (`emulate sh` still turns it off like zsh).
    if matches!(emu, Emulation::Sh) {
        set_in(s, "ignorebraces", false);
    }
    s.emulation = emu;
}

fn letters(s: &Shell) -> &'static [(char, &'static str, bool)] {
    if is_set_in(s, "shoptionletters") {
        KSH_LETTERS
    } else {
        ZSH_LETTERS
    }
}

/// The option for a single-letter flag, and the value `-letter` gives.
pub fn letter(c: char) -> Option<(&'static str, bool)> {
    crate::shell::with_shell(|s| letters(s).iter().find(|l| l.0 == c).map(|l| (l.1, !l.2)))
}

/// `$-`: the letters of the options that are on.
pub fn flags_string(s: &Shell) -> String {
    letters(s)
        .iter()
        .filter(|l| is_set_in(s, l.1) != l.2)
        .map(|l| l.0)
        .collect()
}

/// All option names, sorted.
pub fn names() -> impl Iterator<Item = &'static str> {
    OPTIONS.iter().map(|o| o.0)
}

/// Whether `name` is on by default in emulation `emu`.
pub fn default_in(name: &str, emu: Emulation) -> bool {
    OPTIONS
        .iter()
        .find(|o| o.0 == name)
        .is_some_and(|o| o.1 & emu as u8 != 0)
}

// Generated from zsh Src/options.c: name, default in each emulation
// (csh=1, ksh=2, sh=4, zsh=8), reset by `emulate`, special.
pub const OPTIONS: &[(&str, u8, bool, bool)] = &[
    ("aliases", 15, true, false),
    ("aliasfuncdef", 6, true, false),
    ("allexport", 0, true, false),
    ("alwayslastprompt", 15, false, false),
    ("alwaystoend", 0, false, false),
    ("appendcreate", 6, true, false),
    ("appendhistory", 15, false, false),
    ("autocd", 0, true, false),
    ("autocontinue", 0, false, false),
    ("autolist", 15, false, false),
    ("automenu", 15, false, false),
    ("autonamedirs", 0, false, false),
    ("autoparamkeys", 15, false, false),
    ("autoparamslash", 15, false, false),
    ("autopushd", 0, false, false),
    ("autoremoveslash", 15, false, false),
    ("autoresume", 0, false, false),
    ("badpattern", 9, true, false),
    ("banghist", 9, false, false),
    ("bareglobqual", 8, true, false),
    ("bashautolist", 0, false, false),
    ("bashrematch", 0, false, false),
    ("beep", 15, false, false),
    ("bgnice", 9, true, false),
    ("braceccl", 0, true, false),
    ("bsdecho", 4, true, false),
    ("caseglob", 15, false, false),
    ("casematch", 15, false, false),
    ("casepaths", 0, false, false),
    ("cbases", 0, false, false),
    ("cprecedences", 7, true, false),
    ("cdablevars", 0, true, false),
    ("cdsilent", 0, false, false),
    ("chasedots", 0, true, false),
    ("chaselinks", 0, true, false),
    ("checkjobs", 8, true, false),
    ("checkrunningjobs", 8, true, false),
    ("clobber", 15, true, false),
    ("clobberempty", 0, false, false),
    ("combiningchars", 0, false, false),
    ("completealiases", 0, false, false),
    ("completeinword", 0, false, false),
    ("continueonerror", 0, false, false),
    ("correct", 0, false, false),
    ("correctall", 0, false, false),
    ("cshjunkiehistory", 1, true, false),
    ("cshjunkieloops", 1, true, false),
    ("cshjunkiequotes", 1, true, false),
    ("cshnullcmd", 1, true, false),
    ("cshnullglob", 1, true, false),
    ("debugbeforecmd", 15, false, false),
    ("emacs", 0, false, false),
    ("equals", 8, true, false),
    ("errexit", 0, true, false),
    ("errreturn", 0, true, false),
    ("exec", 15, false, false),
    ("extendedglob", 0, true, false),
    ("extendedhistory", 1, false, false),
    ("evallineno", 8, true, false),
    ("flowcontrol", 15, false, false),
    ("forcefloat", 0, false, false),
    ("functionargzero", 9, true, false),
    ("glob", 15, true, false),
    ("globalexport", 8, true, false),
    ("globalrcs", 15, false, false),
    ("globassign", 1, true, false),
    ("globcomplete", 0, false, false),
    ("globdots", 0, true, false),
    ("globstarshort", 0, true, false),
    ("globsubst", 7, true, false),
    ("hashcmds", 15, false, false),
    ("hashdirs", 15, false, false),
    ("hashexecutablesonly", 0, false, false),
    ("hashlistall", 15, false, false),
    ("histallowclobber", 0, false, false),
    ("histbeep", 15, false, false),
    ("histexpiredupsfirst", 0, false, false),
    ("histfcntllock", 0, false, false),
    ("histfindnodups", 0, false, false),
    ("histignorealldups", 0, false, false),
    ("histignoredups", 0, false, false),
    ("histignorespace", 0, false, false),
    ("histlexwords", 0, false, false),
    ("histnofunctions", 0, false, false),
    ("histnostore", 0, false, false),
    ("histsubstpattern", 0, true, false),
    ("histreduceblanks", 0, false, false),
    ("histsavebycopy", 15, false, false),
    ("histsavenodups", 0, false, false),
    ("histverify", 0, false, false),
    ("hup", 8, true, false),
    ("ignorebraces", 4, true, false),
    ("ignoreclosebraces", 0, true, false),
    ("ignoreeof", 0, false, false),
    ("incappendhistory", 0, false, false),
    ("incappendhistorytime", 0, false, false),
    ("interactive", 0, false, true),
    ("interactivecomments", 6, false, false),
    ("ksharrays", 6, true, false),
    ("kshautoload", 6, true, false),
    ("kshglob", 2, true, false),
    ("kshoptionprint", 2, true, false),
    ("kshtypeset", 0, false, false),
    ("kshzerosubscript", 0, false, false),
    ("listambiguous", 15, false, false),
    ("listbeep", 15, false, false),
    ("listpacked", 0, false, false),
    ("listrowsfirst", 0, false, false),
    ("listtypes", 15, false, false),
    ("localoptions", 2, true, false),
    ("localloops", 0, true, false),
    ("localpatterns", 0, true, false),
    ("localtraps", 2, true, false),
    ("login", 0, false, true),
    ("longlistjobs", 0, false, false),
    ("magicequalsubst", 0, true, false),
    ("mailwarning", 0, false, false),
    ("markdirs", 0, false, false),
    ("menucomplete", 0, false, false),
    ("monitor", 0, false, true),
    ("multibyte", 0, false, false),
    ("multifuncdef", 8, true, false),
    ("multios", 8, true, false),
    ("nomatch", 9, true, false),
    ("notify", 8, false, false),
    ("nullglob", 0, true, false),
    ("numericglobsort", 0, true, false),
    ("octalzeroes", 4, true, false),
    ("overstrike", 0, false, false),
    ("pathdirs", 0, true, false),
    ("pathscript", 6, true, false),
    ("pipefail", 0, true, false),
    ("posixaliases", 6, true, false),
    ("posixargzero", 0, true, false),
    ("posixbuiltins", 6, true, false),
    ("posixcd", 6, true, false),
    ("posixidentifiers", 6, true, false),
    ("posixjobs", 6, true, false),
    ("posixstrings", 6, true, false),
    ("posixtraps", 6, true, false),
    ("printeightbit", 0, false, false),
    ("printexitvalue", 0, false, false),
    ("privileged", 0, false, true),
    ("promptbang", 2, false, false),
    ("promptcr", 15, false, false),
    ("promptpercent", 9, false, false),
    ("promptsp", 15, false, false),
    ("promptsubst", 6, false, false),
    ("pushdignoredups", 0, true, false),
    ("pushdminus", 0, true, false),
    ("pushdsilent", 0, false, false),
    ("pushdtohome", 0, true, false),
    ("rcexpandparam", 0, true, false),
    ("rcquotes", 0, true, false),
    ("rcs", 15, false, false),
    ("recexact", 0, false, false),
    ("rematchpcre", 0, false, false),
    ("rmstarsilent", 6, false, false),
    ("rmstarwait", 0, false, false),
    ("sharehistory", 2, false, false),
    ("shfileexpansion", 6, true, false),
    ("shglob", 6, true, false),
    ("shinstdin", 0, false, true),
    ("shnullcmd", 6, true, false),
    ("shoptionletters", 6, true, false),
    ("shortloops", 9, true, false),
    ("shortrepeat", 0, true, false),
    ("shwordsplit", 6, true, false),
    ("singlecommand", 0, false, true),
    ("singlelinezle", 2, false, false),
    ("sourcetrace", 0, false, false),
    ("sunkeyboardhack", 0, false, false),
    ("transientrprompt", 0, false, false),
    ("trapsasync", 0, false, false),
    ("typesetsilent", 6, true, false),
    ("typesettounset", 6, true, false),
    ("unset", 14, true, false),
    ("verbose", 0, false, false),
    ("vi", 0, false, false),
    ("warncreateglobal", 0, true, false),
    ("warnnestedvar", 0, true, false),
    ("xtrace", 0, false, false),
    ("zle", 0, false, true),
    ("dvorak", 0, false, false),
];

// Alternative names: (alias, option, negated).
pub const ALIASES: &[(&str, &str, bool)] = &[
    ("braceexpand", "ignorebraces", true),
    ("dotglob", "globdots", false),
    ("hashall", "hashcmds", false),
    ("histappend", "appendhistory", false),
    ("histexpand", "banghist", false),
    ("log", "histnofunctions", true),
    ("mailwarn", "mailwarning", false),
    ("onecmd", "singlecommand", false),
    ("physical", "chaselinks", false),
    ("promptvars", "promptsubst", false),
    ("stdin", "shinstdin", false),
    ("trackall", "hashcmds", false),
];

pub const ZSH_LETTERS: &[(char, &str, bool)] = &[
    ('0', "correct", false),
    ('1', "printexitvalue", false),
    ('2', "badpattern", true),
    ('3', "nomatch", true),
    ('4', "globdots", false),
    ('5', "notify", false),
    ('6', "bgnice", false),
    ('7', "ignoreeof", false),
    ('8', "markdirs", false),
    ('9', "autolist", false),
    ('B', "beep", true),
    ('C', "clobber", true),
    ('D', "pushdtohome", false),
    ('E', "pushdsilent", false),
    ('F', "glob", true),
    ('G', "nullglob", false),
    ('H', "rmstarsilent", false),
    ('I', "ignorebraces", false),
    ('J', "autocd", false),
    ('K', "banghist", true),
    ('L', "sunkeyboardhack", false),
    ('M', "singlelinezle", false),
    ('N', "autopushd", false),
    ('O', "correctall", false),
    ('P', "rcexpandparam", false),
    ('Q', "pathdirs", false),
    ('R', "longlistjobs", false),
    ('S', "recexact", false),
    ('T', "cdablevars", false),
    ('U', "mailwarning", false),
    ('V', "promptcr", true),
    ('W', "autoresume", false),
    ('X', "listtypes", false),
    ('Y', "menucomplete", false),
    ('Z', "zle", false),
    ('a', "allexport", false),
    ('d', "globalrcs", true),
    ('e', "errexit", false),
    ('f', "rcs", true),
    ('g', "histignorespace", false),
    ('h', "histignoredups", false),
    ('i', "interactive", false),
    ('k', "interactivecomments", false),
    ('l', "login", false),
    ('m', "monitor", false),
    ('n', "exec", true),
    ('p', "privileged", false),
    ('s', "shinstdin", false),
    ('t', "singlecommand", false),
    ('u', "unset", true),
    ('v', "verbose", false),
    ('w', "chaselinks", false),
    ('x', "xtrace", false),
    ('y', "shwordsplit", false),
];

pub const KSH_LETTERS: &[(char, &str, bool)] = &[
    ('C', "clobber", true),
    ('T', "trapsasync", false),
    ('X', "markdirs", false),
    ('a', "allexport", false),
    ('b', "notify", false),
    ('e', "errexit", false),
    ('f', "glob", true),
    ('i', "interactive", false),
    ('l', "login", false),
    ('m', "monitor", false),
    ('n', "exec", true),
    ('p', "privileged", false),
    ('s', "shinstdin", false),
    ('t', "singlecommand", false),
    ('u', "unset", true),
    ('v', "verbose", false),
    ('x', "xtrace", false),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_spellings() {
        assert_eq!(resolve("SH_WORD_SPLIT"), Some(("shwordsplit", true)));
        assert_eq!(resolve("noglob"), Some(("glob", false)));
        assert_eq!(resolve("NO_NOMATCH"), Some(("nomatch", false)));
        assert_eq!(resolve("notify"), Some(("notify", true)));
        assert_eq!(resolve("dotglob"), Some(("globdots", true)));
        assert_eq!(resolve("braceexpand"), Some(("ignorebraces", false)));
        assert_eq!(resolve("bogus"), None);
    }

    #[test]
    fn emulation_defaults() {
        let mut s = Shell::new();
        init(&mut s, Emulation::Zsh);
        assert!(is_set_in(&s, "nomatch"));
        assert!(!is_set_in(&s, "shwordsplit"));
        emulate_in(&mut s, Emulation::Sh, true);
        assert!(is_set_in(&s, "shwordsplit"));
        assert!(!is_set_in(&s, "nomatch"));
        assert_eq!(s.assoc("options").unwrap()["shwordsplit"], "on");
    }

    #[test]
    fn letters_follow_the_emulation() {
        assert_eq!(Emulation::from_name("-zsh"), Some(Emulation::Zsh));
        assert_eq!(Emulation::from_name("/bin/sh"), Some(Emulation::Sh));
        assert_eq!(Emulation::from_name("bash"), Some(Emulation::Sh));
        assert_eq!(Emulation::from_name("meowsh"), None);
    }
}
