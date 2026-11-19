//! `zsh/pcre` module backed by `libpcre2-8` via `dlopen`.
//!
//! Provides `pcre_compile`, `pcre_study`, `pcre_match`, and `-pcre-match` / `=~`
//! under `REMATCH_PCRE`, including named captures (`.pcre.match`), `BASH_REMATCH`,
//! `ZPCRE_OP`, `-b`, `-n`, `-d` (DFA), and embedded NUL bytes.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{c_char, c_int, c_void};
use std::sync::OnceLock;

use crate::shell::{
    array_set, builtin_err, bytes_to_zstr, is_c_locale, var_set, with_shell, zstr_to_bytes,
};

const PCRE2_CASELESS: u32 = 0x0000_0008;
const PCRE2_DOTALL: u32 = 0x0000_0020;
const PCRE2_EXTENDED: u32 = 0x0000_0080;
const PCRE2_MULTILINE: u32 = 0x0000_0400;
const PCRE2_UTF: u32 = 0x0008_0000;
const PCRE2_ANCHORED: u32 = 0x8000_0000;

const PCRE2_INFO_CAPTURECOUNT: u32 = 4;
const PCRE2_INFO_NAMECOUNT: u32 = 17;
const PCRE2_INFO_NAMEENTRYSIZE: u32 = 18;
const PCRE2_INFO_NAMETABLE: u32 = 19;

const PCRE2_ERROR_NOMATCH: c_int = -1;
const PCRE2_ERROR_DFA_WSSIZE: c_int = -41;
const PCRE2_UNSET: usize = usize::MAX;

type CompileFn = unsafe extern "C" fn(
    pattern: *const u8,
    length: usize,
    options: u32,
    errorcode: *mut c_int,
    erroroffset: *mut usize,
    ccontext: *mut c_void,
) -> *mut c_void;
type CodeFreeFn = unsafe extern "C" fn(code: *mut c_void);
type MatchDataCreateFn =
    unsafe extern "C" fn(code: *const c_void, gcontext: *mut c_void) -> *mut c_void;
type MatchDataCreateOvecFn =
    unsafe extern "C" fn(ovecsize: u32, gcontext: *mut c_void) -> *mut c_void;
type MatchDataFreeFn = unsafe extern "C" fn(match_data: *mut c_void);
type MatchFn = unsafe extern "C" fn(
    code: *const c_void,
    subject: *const u8,
    length: usize,
    startoffset: usize,
    options: u32,
    match_data: *mut c_void,
    mcontext: *mut c_void,
) -> c_int;
type DfaMatchFn = unsafe extern "C" fn(
    code: *const c_void,
    subject: *const u8,
    length: usize,
    startoffset: usize,
    options: u32,
    match_data: *mut c_void,
    mcontext: *mut c_void,
    workspace: *mut c_int,
    wscount: usize,
) -> c_int;
type GetOvectorPtrFn = unsafe extern "C" fn(match_data: *mut c_void) -> *mut usize;
type PatternInfoFn =
    unsafe extern "C" fn(code: *const c_void, what: u32, where_: *mut c_void) -> c_int;
type GetErrorMsgFn =
    unsafe extern "C" fn(errorcode: c_int, buffer: *mut u8, bufflen: usize) -> c_int;

struct Pcre2Api {
    compile: CompileFn,
    code_free: CodeFreeFn,
    match_data_create_from_pattern: MatchDataCreateFn,
    match_data_create: MatchDataCreateOvecFn,
    match_data_free: MatchDataFreeFn,
    pcre_match: MatchFn,
    dfa_match: DfaMatchFn,
    get_ovector_pointer: GetOvectorPtrFn,
    pattern_info: PatternInfoFn,
    get_error_message: GetErrorMsgFn,
}

unsafe impl Send for Pcre2Api {}
unsafe impl Sync for Pcre2Api {}

fn api() -> Option<&'static Pcre2Api> {
    static API: OnceLock<Option<Pcre2Api>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let mut handle = std::ptr::null_mut();
        for name in [
            b"libpcre2-8.so.0\0".as_slice(),
            b"libpcre2-8.so\0".as_slice(),
        ] {
            handle = libc::dlopen(name.as_ptr() as *const c_char, libc::RTLD_LAZY);
            if !handle.is_null() {
                break;
            }
        }
        if handle.is_null() {
            return None;
        }
        macro_rules! sym {
            ($name:literal, $ty:ty) => {{
                let p = libc::dlsym(handle, concat!($name, "\0").as_ptr() as *const c_char);
                if p.is_null() {
                    return None;
                }
                std::mem::transmute::<*mut c_void, $ty>(p)
            }};
        }
        Some(Pcre2Api {
            compile: sym!("pcre2_compile_8", CompileFn),
            code_free: sym!("pcre2_code_free_8", CodeFreeFn),
            match_data_create_from_pattern: sym!(
                "pcre2_match_data_create_from_pattern_8",
                MatchDataCreateFn
            ),
            match_data_create: sym!("pcre2_match_data_create_8", MatchDataCreateOvecFn),
            match_data_free: sym!("pcre2_match_data_free_8", MatchDataFreeFn),
            pcre_match: sym!("pcre2_match_8", MatchFn),
            dfa_match: sym!("pcre2_dfa_match_8", DfaMatchFn),
            get_ovector_pointer: sym!("pcre2_get_ovector_pointer_8", GetOvectorPtrFn),
            pattern_info: sym!("pcre2_pattern_info_8", PatternInfoFn),
            get_error_message: sym!("pcre2_get_error_message_8", GetErrorMsgFn),
        })
    })
    .as_ref()
}

pub fn is_available() -> bool {
    api().is_some()
}

struct CompiledCode {
    ptr: *mut c_void,
}

impl Drop for CompiledCode {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            if let Some(a) = api() {
                unsafe { (a.code_free)(self.ptr) };
            }
            self.ptr = std::ptr::null_mut();
        }
    }
}

thread_local! {
    static COMPILED: RefCell<Option<CompiledCode>> = const { RefCell::new(None) };
}

fn pcre_error_string(a: &Pcre2Api, code: c_int) -> String {
    let mut buf = [0u8; 256];
    let n = unsafe { (a.get_error_message)(code, buf.as_mut_ptr(), buf.len()) };
    if n > 0 {
        String::from_utf8_lossy(&buf[..n as usize]).into_owned()
    } else {
        format!("error {}", code)
    }
}

fn compile_pattern(pat_bytes: &[u8], opts: u32) -> Result<CompiledCode, String> {
    let a = api().ok_or_else(|| "PCRE2 library not available".to_string())?;
    let mut errcode: c_int = 0;
    let mut erroff: usize = 0;
    let ptr = unsafe {
        (a.compile)(
            pat_bytes.as_ptr(),
            pat_bytes.len(),
            opts,
            &mut errcode,
            &mut erroff,
            std::ptr::null_mut(),
        )
    };
    if ptr.is_null() {
        return Err(format!(
            "error in regex at offset {}: {}",
            erroff,
            pcre_error_string(a, errcode)
        ));
    }
    Ok(CompiledCode { ptr })
}

fn utf_opt() -> u32 {
    if crate::options::is_set("multibyte") && !is_c_locale() {
        PCRE2_UTF
    } else {
        0
    }
}

fn byte_to_char_offset(subject: &[u8], byte_off: usize, multibyte: bool) -> usize {
    let clamped = byte_off.min(subject.len());
    if !multibyte {
        return clamped;
    }
    bytes_to_zstr(&subject[..clamped]).chars().count()
}

struct MatchStoreOpts<'a> {
    dfa: bool,
    match_var: &'a str,
    arr_var: &'a str,
    set_zpcre_op: bool,
    bash_rematch: bool,
}

fn store_match_vars(
    a: &Pcre2Api,
    code: *const c_void,
    subject: &[u8],
    ovec: &[usize],
    rc: usize,
    opts: &MatchStoreOpts<'_>,
) {
    let mb = crate::options::is_set("multibyte") && !is_c_locale();
    let ksh = crate::options::is_set("ksharrays");
    let offset_base: i64 = if ksh { 0 } else { 1 };

    let slice_str = |s: usize, e: usize| -> String {
        if s == PCRE2_UNSET || e == PCRE2_UNSET || s > e || e > subject.len() {
            String::new()
        } else {
            bytes_to_zstr(&subject[s..e])
        }
    };

    if opts.set_zpcre_op && rc >= 1 {
        var_set("ZPCRE_OP", &format!("{} {}", ovec[0], ovec[1]), false);
    }

    if opts.dfa {
        let mut items = Vec::with_capacity(rc);
        for i in 0..rc {
            items.push(slice_str(ovec[2 * i], ovec[2 * i + 1]));
        }
        array_set(opts.arr_var, items);
        return;
    }

    let mut capcount: u32 = 0;
    unsafe {
        (a.pattern_info)(
            code,
            PCRE2_INFO_CAPTURECOUNT,
            &mut capcount as *mut u32 as *mut c_void,
        );
    }
    let total_groups = capcount as usize;

    if opts.bash_rematch {
        let mut items = Vec::with_capacity(total_groups + 1);
        for i in 0..=total_groups {
            if i < rc {
                items.push(slice_str(ovec[2 * i], ovec[2 * i + 1]));
            } else {
                items.push(String::new());
            }
        }
        array_set("BASH_REMATCH", items);
        return;
    }

    let full_s = ovec[0];
    let full_e = ovec[1];
    var_set(opts.match_var, &slice_str(full_s, full_e), false);
    let mbegin = byte_to_char_offset(subject, full_s, mb) as i64 + offset_base;
    let mend = if full_e > full_s {
        byte_to_char_offset(subject, full_e, mb) as i64 + offset_base - 1
    } else {
        mbegin - 1
    };
    var_set("MBEGIN", &mbegin.to_string(), false);
    var_set("MEND", &mend.to_string(), false);

    let mut captures = Vec::with_capacity(total_groups);
    let mut mbegins = Vec::with_capacity(total_groups);
    let mut mends = Vec::with_capacity(total_groups);
    for i in 1..=total_groups {
        let (s, e) = if i < rc {
            (ovec[2 * i], ovec[2 * i + 1])
        } else {
            (PCRE2_UNSET, PCRE2_UNSET)
        };
        if s == PCRE2_UNSET || e == PCRE2_UNSET || s > e || e > subject.len() {
            captures.push(String::new());
            mbegins.push("-1".to_string());
            mends.push("-1".to_string());
        } else {
            captures.push(bytes_to_zstr(&subject[s..e]));
            let cb = byte_to_char_offset(subject, s, mb) as i64 + offset_base;
            let ce = if e > s {
                byte_to_char_offset(subject, e, mb) as i64 + offset_base - 1
            } else {
                cb - 1
            };
            mbegins.push(cb.to_string());
            mends.push(ce.to_string());
        }
    }
    array_set(opts.arr_var, captures);
    array_set("mbegin", mbegins);
    array_set("mend", mends);

    // Populate `.pcre.match` for named capture groups.
    let mut ncount: u32 = 0;
    let mut nsize: u32 = 0;
    let mut ntable: *const u8 = std::ptr::null();
    let ok_names = unsafe {
        (a.pattern_info)(
            code,
            PCRE2_INFO_NAMECOUNT,
            &mut ncount as *mut u32 as *mut c_void,
        ) == 0
            && ncount > 0
            && (a.pattern_info)(
                code,
                PCRE2_INFO_NAMEENTRYSIZE,
                &mut nsize as *mut u32 as *mut c_void,
            ) == 0
            && (a.pattern_info)(
                code,
                PCRE2_INFO_NAMETABLE,
                &mut ntable as *mut *const u8 as *mut c_void,
            ) == 0
            && !ntable.is_null()
            && nsize >= 3
    };
    if ok_names {
        let mut named = BTreeMap::new();
        for idx in 0..ncount as usize {
            let entry = unsafe {
                std::slice::from_raw_parts(ntable.add(idx * nsize as usize), nsize as usize)
            };
            let grp = ((entry[0] as usize) << 8) | (entry[1] as usize);
            let name_bytes = &entry[2..];
            let end = name_bytes
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(name_bytes.len());
            let name = bytes_to_zstr(&name_bytes[..end]);
            if grp < rc {
                let s = ovec[2 * grp];
                let e = ovec[2 * grp + 1];
                if s != PCRE2_UNSET && e != PCRE2_UNSET && s <= e && e <= subject.len() {
                    named.insert(name, bytes_to_zstr(&subject[s..e]));
                }
            }
        }
        with_shell(|sh| {
            sh.store(".pcre.match", crate::params::Value::Assoc(named));
        });
    }
}

pub fn builtin_pcre_compile(words: &[String]) -> i32 {
    let mut opts = utf_opt();
    let mut i = 1;
    while let Some(a) = words.get(i) {
        if a == "--" {
            i += 1;
            break;
        }
        let Some(flags) = a.strip_prefix('-').filter(|s| !s.is_empty()) else {
            break;
        };
        for c in flags.chars() {
            match c {
                'a' => opts |= PCRE2_ANCHORED,
                'i' => opts |= PCRE2_CASELESS,
                'm' => opts |= PCRE2_MULTILINE,
                'x' => opts |= PCRE2_EXTENDED,
                's' => opts |= PCRE2_DOTALL,
                other => {
                    builtin_err("pcre_compile", &format!("bad option: -{}", other));
                    return 1;
                }
            }
        }
        i += 1;
    }
    let Some(pat) = words.get(i) else {
        builtin_err("pcre_compile", "not enough arguments");
        return 1;
    };
    let pat_bytes = zstr_to_bytes(pat);
    match compile_pattern(&pat_bytes, opts) {
        Ok(c) => {
            COMPILED.with(|cell| *cell.borrow_mut() = Some(c));
            0
        }
        Err(msg) => {
            builtin_err("pcre_compile", &msg);
            1
        }
    }
}

pub fn builtin_pcre_study(_words: &[String]) -> i32 {
    let has = COMPILED.with(|c| c.borrow().is_some());
    if !has {
        builtin_err("pcre_study", "no pattern compiled");
        return 1;
    }
    0
}

pub fn builtin_pcre_match(words: &[String]) -> i32 {
    let Some(a) = api() else {
        builtin_err("pcre_match", "PCRE2 library not available");
        return 1;
    };
    let mut match_var = "MATCH".to_string();
    let mut arr_var = "match".to_string();
    let mut set_zpcre_op = false;
    let mut dfa = false;
    let mut start_offset: usize = 0;

    let mut i = 1;
    while let Some(arg) = words.get(i) {
        if arg == "--" {
            i += 1;
            break;
        }
        let Some(flags) = arg.strip_prefix('-').filter(|s| !s.is_empty()) else {
            break;
        };
        let mut chars = flags.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                'b' => set_zpcre_op = true,
                'd' => dfa = true,
                'v' | 'a' | 'n' => {
                    let rest: String = chars.collect();
                    let val = if !rest.is_empty() {
                        rest
                    } else {
                        i += 1;
                        match words.get(i) {
                            Some(v) => v.clone(),
                            None => {
                                builtin_err(
                                    "pcre_match",
                                    &format!("argument expected after -{}", c),
                                );
                                return 1;
                            }
                        }
                    };
                    match c {
                        'v' => match_var = val,
                        'a' => arr_var = val,
                        'n' => {
                            start_offset = match val.parse::<usize>() {
                                Ok(n) => n,
                                Err(_) => {
                                    builtin_err("pcre_match", &format!("invalid offset: {}", val));
                                    return 1;
                                }
                            };
                        }
                        _ => {}
                    }
                    break;
                }
                other => {
                    builtin_err("pcre_match", &format!("bad option: -{}", other));
                    return 1;
                }
            }
        }
        i += 1;
    }
    let Some(subj) = words.get(i) else {
        builtin_err("pcre_match", "not enough arguments");
        return 1;
    };
    let subject = zstr_to_bytes(subj);
    if start_offset > subject.len() {
        return 1;
    }

    COMPILED.with(|cell| {
        let borrow = cell.borrow();
        let Some(compiled) = borrow.as_ref() else {
            builtin_err("pcre_match", "no pattern compiled");
            return 1;
        };
        if dfa {
            let mut wscount: usize = 128;
            let mut capcount: u32 = 128;
            loop {
                let md = unsafe { (a.match_data_create)(capcount, std::ptr::null_mut()) };
                if md.is_null() {
                    return 1;
                }
                let mut ws = vec![0 as c_int; wscount];
                let rc = unsafe {
                    (a.dfa_match)(
                        compiled.ptr,
                        subject.as_ptr(),
                        subject.len(),
                        start_offset,
                        0,
                        md,
                        std::ptr::null_mut(),
                        ws.as_mut_ptr(),
                        wscount,
                    )
                };
                if rc == PCRE2_ERROR_DFA_WSSIZE {
                    unsafe { (a.match_data_free)(md) };
                    wscount *= 2;
                    continue;
                }
                if rc == 0 {
                    unsafe { (a.match_data_free)(md) };
                    capcount *= 2;
                    continue;
                }
                if rc > 0 {
                    let ovec_ptr = unsafe { (a.get_ovector_pointer)(md) };
                    let ovec = unsafe { std::slice::from_raw_parts(ovec_ptr, (rc as usize) * 2) };
                    store_match_vars(
                        a,
                        compiled.ptr,
                        &subject,
                        ovec,
                        rc as usize,
                        &MatchStoreOpts {
                            dfa: true,
                            match_var: &match_var,
                            arr_var: &arr_var,
                            set_zpcre_op,
                            bash_rematch: false,
                        },
                    );
                    unsafe { (a.match_data_free)(md) };
                    return 0;
                }
                unsafe { (a.match_data_free)(md) };
                return 1;
            }
        }
        let md = unsafe { (a.match_data_create_from_pattern)(compiled.ptr, std::ptr::null_mut()) };
        if md.is_null() {
            return 1;
        }
        let rc = unsafe {
            (a.pcre_match)(
                compiled.ptr,
                subject.as_ptr(),
                subject.len(),
                start_offset,
                0,
                md,
                std::ptr::null_mut(),
            )
        };
        let status = if rc >= 0 {
            let count = if rc == 0 { 1 } else { rc as usize };
            let ovec_ptr = unsafe { (a.get_ovector_pointer)(md) };
            let ovec = unsafe { std::slice::from_raw_parts(ovec_ptr, count * 2) };
            store_match_vars(
                a,
                compiled.ptr,
                &subject,
                ovec,
                count,
                &MatchStoreOpts {
                    dfa: false,
                    match_var: &match_var,
                    arr_var: &arr_var,
                    set_zpcre_op,
                    bash_rematch: false,
                },
            );
            0
        } else {
            if rc != PCRE2_ERROR_NOMATCH {
                builtin_err("pcre_match", &pcre_error_string(a, rc));
            }
            1
        };
        unsafe { (a.match_data_free)(md) };
        status
    })
}

pub fn cond_pcre_match(lhs: &str, rhs: &str) -> Result<bool, String> {
    let a = api().ok_or_else(|| "PCRE2 library not available".to_string())?;
    let mut opts = utf_opt();
    if !crate::options::is_set("casematch") {
        opts |= PCRE2_CASELESS;
    }
    let pat_bytes = zstr_to_bytes(rhs);
    let compiled = compile_pattern(&pat_bytes, opts)?;
    let subject = zstr_to_bytes(lhs);
    let md = unsafe { (a.match_data_create_from_pattern)(compiled.ptr, std::ptr::null_mut()) };
    if md.is_null() {
        return Ok(false);
    }
    let rc = unsafe {
        (a.pcre_match)(
            compiled.ptr,
            subject.as_ptr(),
            subject.len(),
            0,
            0,
            md,
            std::ptr::null_mut(),
        )
    };
    let matched = if rc >= 0 {
        let count = if rc == 0 { 1 } else { rc as usize };
        let ovec_ptr = unsafe { (a.get_ovector_pointer)(md) };
        let ovec = unsafe { std::slice::from_raw_parts(ovec_ptr, count * 2) };
        let bash_rematch = crate::options::is_set("bashrematch");
        store_match_vars(
            a,
            compiled.ptr,
            &subject,
            ovec,
            count,
            &MatchStoreOpts {
                dfa: false,
                match_var: "MATCH",
                arr_var: "match",
                set_zpcre_op: false,
                bash_rematch,
            },
        );
        Ok(true)
    } else if rc == PCRE2_ERROR_NOMATCH {
        Ok(false)
    } else {
        Err(pcre_error_string(a, rc))
    };
    unsafe { (a.match_data_free)(md) };
    matched
}
