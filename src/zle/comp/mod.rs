//! The completion system, after zsh's Src/Zle completion files.
//!
//! zsh keeps completion state in C globals spread over zle_tricky.c,
//! complete.c, compcore.c, compmatch.c and compresult.c. Here they are
//! the fields of [`CompState`], reached as `z.comp` from any function
//! given the editor state `z: &mut Zle`. Builtins run from shell code
//! (compadd, compset, ...) reach it through `crate::zle::with_active`.
//!
//! Strings are `String`; every length or offset into one counts
//! characters, not bytes. zsh's NULL strings are `None`.

use std::rc::Rc;

pub mod complete;
pub mod complist;
pub mod computil_args;
pub mod computil_rest;
pub mod core;
pub mod matching;
pub mod result;
pub mod tricky;

// ---- matches ------------------------------------------------------------

// Cmatch flags (CMF_*).
pub const CMF_FILE: u32 = 1 << 0;
pub const CMF_REMOVE: u32 = 1 << 1;
pub const CMF_ISPAR: u32 = 1 << 2;
pub const CMF_PARBR: u32 = 1 << 3;
pub const CMF_PARNEST: u32 = 1 << 4;
pub const CMF_NOLIST: u32 = 1 << 5;
pub const CMF_DISPLINE: u32 = 1 << 6;
pub const CMF_HIDE: u32 = 1 << 7;
pub const CMF_NOSPACE: u32 = 1 << 8;
pub const CMF_PACKED: u32 = 1 << 9;
pub const CMF_ROWS: u32 = 1 << 10;
pub const CMF_MULT: u32 = 1 << 11;
pub const CMF_FMULT: u32 = 1 << 12;
pub const CMF_ALL: u32 = 1 << 13;
pub const CMF_DUMMY: u32 = 1 << 14;
pub const CMF_MORDER: u32 = 1 << 15;
pub const CMF_DELETE: u32 = 1 << 16;

/// One match (zsh's struct cmatch).
#[derive(Clone, Debug, Default)]
pub struct Cmatch {
    pub str: Option<String>,
    pub orig: Option<String>,
    pub ipre: Option<String>,
    pub ripre: Option<String>,
    pub isuf: Option<String>,
    pub ppre: Option<String>,
    pub psuf: Option<String>,
    pub prpre: Option<String>,
    pub pre: Option<String>,
    pub suf: Option<String>,
    pub disp: Option<String>,
    pub autoq: Option<String>,
    pub flags: u32,
    pub brpl: Vec<i32>,
    pub brsl: Vec<i32>,
    pub rems: Option<String>,
    pub remf: Option<String>,
    pub qipl: i32,
    pub qisl: i32,
    pub rnum: i32,
    pub gnum: i32,
    pub mode: u32,
    pub modec: char,
    pub fmode: u32,
    pub fmodec: char,
}

/// Matches are shared between a group's list and the menu and
/// ambiguous-match records, as zsh shares Cmatch pointers.
pub type CmatchRef = Rc<std::cell::RefCell<Cmatch>>;

#[derive(Clone, Debug, Default)]
pub struct Cexpl {
    pub always: bool,
    pub str: Option<String>,
    pub count: i32,
    pub fcount: i32,
}

// Group flags (CGF_*).
pub const CGF_NOSORT: u32 = 1;
pub const CGF_LINES: u32 = 2;
pub const CGF_HASDL: u32 = 4;
pub const CGF_UNIQALL: u32 = 8;
pub const CGF_UNIQCON: u32 = 16;
pub const CGF_PACKED: u32 = 32;
pub const CGF_ROWS: u32 = 64;
pub const CGF_FILES: u32 = 128;
pub const CGF_MATSORT: u32 = 256;
pub const CGF_NUMSORT: u32 = 512;
pub const CGF_REVSORT: u32 = 1024;

/// A group of matches (zsh's struct cmgroup). zsh's doubly linked group
/// lists are `Vec<Cmgroup>` in list order; `prev`/`next` become index
/// arithmetic.
#[derive(Clone, Debug, Default)]
pub struct Cmgroup {
    pub name: Option<String>,
    pub flags: u32,
    pub mcount: i32,
    pub matches: Vec<CmatchRef>,
    pub lcount: i32,
    pub llcount: i32,
    pub ylist: Vec<String>,
    pub ecount: i32,
    pub expls: Vec<Cexpl>,
    pub ccount: i32,
    /// Explanations, matches and fignore-free matches while building.
    pub lexpls: Vec<Cexpl>,
    pub lmatches: Vec<CmatchRef>,
    pub lfmatches: Vec<CmatchRef>,
    pub num: i32,
    pub nbrbeg: i32,
    pub nbrend: i32,
    pub new: i32,
    pub dcount: i32,
    pub cols: i32,
    pub lins: i32,
    pub width: i32,
    pub widths: Vec<i32>,
    pub totl: i32,
    pub shortest: i32,
}

// ---- matchers -----------------------------------------------------------

// Cmatcher flags.
pub const CMF_LINE: u32 = 1;
pub const CMF_LEFT: u32 = 2;
pub const CMF_RIGHT: u32 = 4;
pub const CMF_INTER: u32 = 8;

#[derive(Clone, Debug, PartialEq)]
pub enum Cpattern {
    /// `[...]`, or `[!...]` when negated: the class text as zsh keeps it.
    Class {
        negated: bool,
        text: Vec<char>,
    },
    /// `{...}`: an equivalence class.
    Equiv(Vec<char>),
    /// `?`
    Any,
    Char(char),
}

/// One matcher of a match spec (zsh's struct cmatcher); `next` chains the
/// matchers of one spec.
#[derive(Clone, Debug, Default)]
pub struct Cmatcher {
    pub next: Option<Rc<Cmatcher>>,
    pub flags: u32,
    pub line: Vec<Cpattern>,
    pub llen: i32,
    pub word: Vec<Cpattern>,
    /// Length of `word`, or -1 for one `*`, -2 for two.
    pub wlen: i32,
    pub left: Vec<Cpattern>,
    pub lalen: i32,
    pub right: Vec<Cpattern>,
    pub ralen: i32,
}

/// A list of match specs (zsh's struct cmlist).
#[derive(Clone, Debug, Default)]
pub struct Cmlist {
    pub matcher: Option<Rc<Cmatcher>>,
    pub str: String,
}

// ---- unambiguous-string building ----------------------------------------

pub const CLF_MISS: u32 = 1;
pub const CLF_DIFF: u32 = 2;
pub const CLF_SUF: u32 = 4;
pub const CLF_MID: u32 = 8;
pub const CLF_NEW: u32 = 16;
pub const CLF_LINE: u32 = 32;
pub const CLF_JOIN: u32 = 64;
pub const CLF_MATCHED: u32 = 128;
pub const CLF_SKIP: u32 = 256;

/// zsh's struct cline: a list of parts of the line string shared by
/// matches. `Cline` is the list (zsh's NULL is `None`).
#[derive(Clone, Debug, Default)]
pub struct ClineNode {
    pub next: Cline,
    pub flags: u32,
    pub line: Option<String>,
    pub llen: i32,
    pub word: Option<String>,
    pub wlen: i32,
    pub orig: Option<String>,
    pub olen: i32,
    pub slen: i32,
    pub prefix: Cline,
    pub suffix: Cline,
    pub min: i32,
    pub max: i32,
}

pub type Cline = Option<Box<ClineNode>>;

/// Ambiguous-match information (zsh's struct aminfo).
#[derive(Clone, Debug, Default)]
pub struct Aminfo {
    pub firstm: Option<CmatchRef>,
    pub exact: i32,
    pub exactm: Option<CmatchRef>,
    pub count: i32,
    pub line: Cline,
}

/// Menu completion state (zsh's struct menuinfo). `group` and `cur` are
/// indices into the group list and that group's matches.
#[derive(Clone, Debug, Default)]
pub struct Menuinfo {
    pub group: Option<usize>,
    pub cur: Option<usize>,
    pub pos: i32,
    pub len: i32,
    pub end: i32,
    pub we: i32,
    pub insc: i32,
    pub asked: i32,
    pub prebr: Option<String>,
    pub postbr: Option<String>,
}

/// A brace in the word being completed (zsh's struct brinfo).
#[derive(Clone, Debug, Default)]
pub struct Brinfo {
    pub str: Option<String>,
    pub pos: i32,
    pub qpos: i32,
    pub curpos: i32,
}

// compadd -flags (CAF_*).
pub const CAF_QUOTE: u32 = 1;
pub const CAF_NOSORT: u32 = 2;
pub const CAF_MATCH: u32 = 4;
pub const CAF_UNIQCON: u32 = 8;
pub const CAF_UNIQALL: u32 = 16;
pub const CAF_ARRAYS: u32 = 32;
pub const CAF_KEYS: u32 = 64;
pub const CAF_ALL: u32 = 128;
pub const CAF_MATSORT: u32 = 256;
pub const CAF_NUMSORT: u32 = 512;
pub const CAF_REVSORT: u32 = 1024;

/// What compadd was asked to add (zsh's struct cadata).
#[derive(Clone, Debug, Default)]
pub struct Cadata {
    pub ipre: Option<String>,
    pub isuf: Option<String>,
    pub ppre: Option<String>,
    pub psuf: Option<String>,
    pub prpre: Option<String>,
    pub pre: Option<String>,
    pub suf: Option<String>,
    pub group: Option<String>,
    pub rems: Option<String>,
    pub remf: Option<String>,
    pub ign: Option<String>,
    pub flags: u32,
    pub aflags: u32,
    pub match_: Option<Rc<Cmatcher>>,
    pub exp: Option<String>,
    pub apar: Option<String>,
    pub opar: Option<String>,
    pub dpar: Vec<String>,
    pub disp: Option<String>,
    pub mesg: Option<String>,
    pub dummies: i32,
}

/// Listing layout (zsh's struct cldata).
#[derive(Clone, Debug, Default)]
pub struct Cldata {
    pub zterm_columns: i32,
    pub zterm_lines: i32,
    pub menuacc: i32,
    pub valid: i32,
    pub nlist: i32,
    pub nlines: i32,
    pub hidden: i32,
    pub onlyexpl: i32,
    pub showall: i32,
}

// Completion types (COMP_*).
pub const COMP_COMPLETE: i32 = 0;
pub const COMP_LIST_COMPLETE: i32 = 1;
pub const COMP_SPELL: i32 = 2;
pub const COMP_EXPAND: i32 = 3;
pub const COMP_EXPAND_COMPLETE: i32 = 4;
pub const COMP_LIST_EXPAND: i32 = 5;

pub fn comp_isexpand(x: i32) -> bool {
    x >= COMP_EXPAND
}

// Indices of the special completion parameters (CPN_*); the bits are
// 1 << index (CP_*).
pub const CPN_WORDS: u32 = 0;
pub const CPN_REDIRS: u32 = 1;
pub const CPN_CURRENT: u32 = 2;
pub const CPN_PREFIX: u32 = 3;
pub const CPN_SUFFIX: u32 = 4;
pub const CPN_IPREFIX: u32 = 5;
pub const CPN_ISUFFIX: u32 = 6;
pub const CPN_QIPREFIX: u32 = 7;
pub const CPN_QISUFFIX: u32 = 8;
pub const CPN_COMPSTATE: u32 = 9;
pub const CP_REALPARAMS: u32 = 10;
pub const CP_ALLREALS: u32 = 0x3ff;
pub const CPN_NMATCHES: u32 = 0;
pub const CPN_CONTEXT: u32 = 1;
pub const CPN_PARAMETER: u32 = 2;
pub const CPN_REDIRECT: u32 = 3;
pub const CPN_QUOTE: u32 = 4;
pub const CPN_QUOTING: u32 = 5;
pub const CPN_RESTORE: u32 = 6;
pub const CPN_LIST: u32 = 7;
pub const CPN_INSERT: u32 = 8;
pub const CPN_EXACT: u32 = 9;
pub const CPN_EXACTSTR: u32 = 10;
pub const CPN_PATMATCH: u32 = 11;
pub const CPN_PATINSERT: u32 = 12;
pub const CPN_UNAMBIG: u32 = 13;
pub const CPN_UNAMBIGC: u32 = 14;
pub const CPN_UNAMBIGP: u32 = 15;
pub const CPN_INSERTP: u32 = 16;
pub const CPN_LISTMAX: u32 = 17;
pub const CPN_LASTPROMPT: u32 = 18;
pub const CPN_TOEND: u32 = 19;
pub const CPN_OLDLIST: u32 = 20;
pub const CPN_OLDINS: u32 = 21;
pub const CPN_VARED: u32 = 22;
pub const CPN_LISTLINES: u32 = 23;
pub const CPN_QUOTES: u32 = 24;
pub const CPN_IGNORED: u32 = 25;
pub const CP_KEYPARAMS: u32 = 26;
pub const CP_ALLKEYS: u32 = 0x3ffffff;

/// zsh's completion globals.
#[derive(Clone, Debug, Default)]
pub struct CompState {
    // zle_tricky.c
    pub origline: Option<String>,
    pub origcs: i32,
    pub origll: i32,
    pub clwsize: i32,
    pub clwnum: i32,
    pub clwpos: i32,
    pub clwords: Vec<String>,
    pub offs: i32,
    pub usemenu: i32,
    pub useglob: i32,
    pub wouldinstab: i32,
    pub menucmp: i32,
    pub brbeg: Vec<Brinfo>,
    pub lastbrbeg: Vec<Brinfo>,
    pub brend: Vec<Brinfo>,
    pub lastbrend: Vec<Brinfo>,
    pub nbrbeg: i32,
    pub nbrend: i32,
    pub lastprebr: Option<String>,
    pub lastpostbr: Option<String>,
    pub validlist: i32,
    pub showagain: i32,
    pub qipre: Option<String>,
    pub qisuf: Option<String>,
    pub autoq: Option<String>,
    pub compfunc: Option<String>,
    pub lastambig: i32,
    pub bashlistfirst: i32,
    pub cfargs: Vec<String>,
    pub cfret: i32,
    pub comprecursive: i32,
    pub hascompwidgets: i32,
    pub lincmd: i32,
    pub linredir: i32,
    pub linarr: i32,
    pub rdstr: Option<String>,
    pub rdstrs: Vec<String>,
    pub cmdstr: Option<String>,
    pub varname: Option<String>,
    pub insubscr: i32,
    pub instring: i32,
    pub inbackt: i32,
    /// Set while a completion widget's function runs (zsh's incompfunc).
    pub incompfunc: i32,

    // complete.c: the values behind the special parameters.
    pub compcurrent: i64,
    pub complistmax: i64,
    pub complistlines: i64,
    pub compignored: i64,
    pub compwords: Vec<String>,
    pub compredirs: Vec<String>,
    pub compprefix: Option<String>,
    pub compsuffix: Option<String>,
    pub complastprefix: Option<String>,
    pub complastsuffix: Option<String>,
    pub compisuffix: Option<String>,
    pub compqiprefix: Option<String>,
    pub compqisuffix: Option<String>,
    pub compquote: Option<String>,
    pub compqstack: Option<String>,
    pub comppatmatch: Option<String>,
    pub complastprompt: Option<String>,
    pub compiprefix: Option<String>,
    pub compcontext: Option<String>,
    pub compparameter: Option<String>,
    pub compredirect: Option<String>,
    pub compquoting: Option<String>,
    pub comprestore: Option<String>,
    pub complist: Option<String>,
    pub compinsert: Option<String>,
    pub compexact: Option<String>,
    pub compexactstr: Option<String>,
    pub comppatinsert: Option<String>,
    pub comptoend: Option<String>,
    pub compoldlist: Option<String>,
    pub compoldins: Option<String>,
    pub compvared: Option<String>,
    /// Which special parameters are set (CP_* bits), as comp_setunset.
    pub comp_set_reals: u32,
    pub comp_set_keys: u32,

    // compcore.c
    pub useexact: i32,
    pub useline: i32,
    pub uselist: i32,
    pub forcelist: i32,
    pub startauto: i32,
    pub iforcemenu: i32,
    pub dolastprompt: i32,
    pub oldlist: i32,
    pub oldins: i32,
    pub origlpre: i32,
    pub origlsuf: i32,
    pub lenchanged: i32,
    pub movetoend: i32,
    pub insmnum: i32,
    pub insspace: i32,
    pub minfo: Menuinfo,
    pub menuacc: i32,
    pub useqbr: i32,
    pub brpcs: i32,
    pub brscs: i32,
    pub ispar: i32,
    pub linwhat: i32,
    pub parpre: Option<String>,
    pub parflags: i32,
    pub mflags: i32,
    pub parq: i32,
    pub eparq: i32,
    pub ipre: Option<String>,
    pub ripre: Option<String>,
    pub isuf: Option<String>,
    pub matches: Vec<CmatchRef>,
    pub fmatches: Vec<CmatchRef>,
    /// The groups of the last completion, kept for menu cycling.
    pub lastmatches: Vec<Cmgroup>,
    /// The permanent groups (zsh's pmatches).
    pub pmatches: Vec<Cmgroup>,
    /// The groups being built.
    pub amatches: Vec<Cmgroup>,
    pub lmatches: Vec<Cmgroup>,
    pub lastlmatches: Vec<Cmgroup>,
    pub hasoldlist: i32,
    pub hasperm: i32,
    pub hasallmatch: i32,
    pub newmatches: i32,
    pub permmnum: i32,
    pub permgnum: i32,
    pub lastpermmnum: i32,
    pub lastpermgnum: i32,
    pub nmatches: i32,
    pub smatches: i32,
    pub diffmatches: i32,
    pub nmessages: i32,
    pub onlyexpl: i32,
    pub listdat: Cldata,
    pub ispattern: i32,
    pub haspattern: i32,
    pub hasmatched: i32,
    pub hasunmatched: i32,
    /// The group being added to (an index into `amatches`).
    pub mgroup: Option<usize>,
    pub mnum: i32,
    pub unambig_mnum: i32,
    pub maxmlen: i32,
    pub minmlen: i32,
    pub expls: Vec<Cexpl>,
    pub curexpl: Option<Cexpl>,
    pub mstack: Vec<Cmlist>,
    pub bmatchers: Vec<Cmlist>,
    pub matchers: Vec<Rc<Cmatcher>>,
    pub ainfo: Option<Aminfo>,
    pub fainfo: Option<Aminfo>,
    pub fromcomp: i32,
    pub lastend: i32,

    // compmatch.c
    pub matchbuf: Vec<char>,
    pub matchbufadded: i32,
    pub matchparts: Cline,
    pub matchsubs: Cline,

    // compresult.c
    pub invcount: i32,
}
