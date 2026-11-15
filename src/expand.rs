//! Word expansion: brace, tilde, parameter, command substitution,
//! arithmetic, field splitting, pathname expansion and quote removal.
//!
//! Words arrive raw from the parser, quotes included. Expansion walks a
//! word once and produces characters tagged with how they were produced,
//! so later stages know which characters may be split on or globbed.

use crate::lexer::{
    scan_arith, scan_bquote, scan_brace, scan_dollar_squote, scan_dquote, scan_paren, scan_squote,
};
use crate::shell::{
    assoc_get, assoc_keys, assoc_values, opt_set, var_get, var_get_opt, var_set, with_shell,
};
use crate::types::{OPT_NOGLOB, OPT_NOUNSET};

/// An expansion failed. The message has already been printed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExpError {
    pub status: i32,
}

pub type ExpResult<T> = Result<T, ExpError>;

fn fail<T>(msg: &str) -> ExpResult<T> {
    crate::shell::shell_err(msg);
    Err(ExpError { status: 1 })
}

// Character tags.
const Q: u8 = 1; // quoted: never split or globbed
const S: u8 = 2; // result of an unquoted expansion: subject to field splitting
const MARK: u8 = 4; // zero-width marker: the field exists even if empty
const NG: u8 = 8; // expansion result that pathname expansion leaves literal
const B: u8 = 16; // unquoted literal text: its braces and commas brace-expand

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Unquoted,
    Dq,
    Heredoc,
}

struct Ctx {
    fields: Vec<Vec<(char, u8)>>,
    // Lists such as "$@" produce separate fields.
    split: bool,
    // Assignment value: tilde expansion also follows ':'.
    assign: bool,
    // Count of non-list emissions; used to decide whether a pair of
    // double quotes that contained only "$@" produces a field.
    content: usize,
    at_lists: usize,
    // Tag for unquoted literal characters: 0 normally, S inside the word
    // of an unquoted `${x-word}`, whose text is part of the expansion.
    unquoted_lit: u8,
    // After `${^arr}`: later text is appended to every field from here.
    rc_from: Option<usize>,
    // Tags for unquoted parameter and command substitution results, from
    // SH_WORD_SPLIT and GLOB_SUBST: zsh splits parameters only with
    // SH_WORD_SPLIT, always splits command substitution, and globs
    // neither unless GLOB_SUBST.
    param_tag: u8,
    subst_tag: u8,
}

impl Ctx {
    fn new(split: bool, assign: bool) -> Self {
        Ctx {
            fields: vec![Vec::new()],
            split,
            assign,
            content: 0,
            at_lists: 0,
            unquoted_lit: 0,
            rc_from: None,
            param_tag: 0,
            subst_tag: 0,
        }
        .with_option_tags()
    }

    fn with_option_tags(mut self) -> Self {
        let (split, glob) = with_shell(|s| {
            (
                crate::options::is_set_in(s, "shwordsplit"),
                crate::options::is_set_in(s, "globsubst"),
            )
        });
        let no_glob = if glob { 0 } else { NG };
        self.param_tag = if split { S } else { 0 } | no_glob;
        self.subst_tag = S | no_glob;
        self
    }

    fn push(&mut self, c: char, flag: u8) {
        match self.rc_from {
            Some(start) => self.fields[start..]
                .iter_mut()
                .for_each(|f| f.push((c, flag))),
            None => self.fields.last_mut().unwrap().push((c, flag)),
        }
        self.content += 1;
    }

    fn mark(&mut self) {
        self.push('\0', MARK);
    }

    fn new_field(&mut self) {
        self.rc_from = None;
        self.fields.push(Vec::new());
    }

    fn joined(&self) -> String {
        self.fields
            .iter()
            .flatten()
            .filter(|(_, f)| f & MARK == 0)
            .map(|(c, _)| *c)
            .collect()
    }
}

enum Val {
    Unset,
    Str(String),
    // Items and whether the list joins into one word inside quotes
    // (`$*`, `${a[*]}`) rather than staying separate (`$@`).
    List(Vec<String>, bool),
    // `${^arr}`: each element combines with the surrounding text.
    Rc(Vec<String>),
    // Expansion of a `${x-word}` word, quoting tags kept.
    Tagged(Vec<Vec<(char, u8)>>),
}

fn ifs() -> String {
    var_get_opt("IFS").unwrap_or_else(|| " \t\n".to_string())
}

fn ifs_first() -> String {
    ifs()
        .chars()
        .next()
        .map(|c| c.to_string())
        .unwrap_or_default()
}

fn pos_params() -> Vec<String> {
    with_shell(|s| s.pos_params.clone())
}

fn lookup(name: &str) -> Val {
    match name {
        "@" => return Val::List(pos_params(), false),
        "*" => return Val::List(pos_params(), true),
        _ => {}
    }
    let resolved = crate::shell::resolve_ref(name);
    if resolved != name {
        return match resolved.find('[') {
            Some(k) if resolved.ends_with(']') => {
                param_value(&resolved[..k], Some(&resolved[k + 1..resolved.len() - 1]))
                    .unwrap_or(Val::Unset)
            }
            _ => lookup(&resolved),
        };
    }
    if let Some(items) = with_shell(|s| s.array(name).cloned()) {
        return Val::List(items, true);
    }
    if let Some(items) = assoc_values(name) {
        return Val::List(items, true);
    }
    match var_get_opt(name) {
        Some(v) => Val::Str(v),
        None => Val::Unset,
    }
}

fn check_set(name: &str, val: &Val) -> ExpResult<()> {
    if matches!(val, Val::Unset) && name != "@" && name != "*" && opt_set(OPT_NOUNSET) {
        return fail(&format!("{}: unbound variable", name));
    }
    Ok(())
}

fn emit_str(ctx: &mut Ctx, s: &str, quoted: bool) {
    let flag = if quoted { Q } else { ctx.param_tag };
    emit_tagged(ctx, s, flag);
}

// Command substitution output.
fn emit_subst(ctx: &mut Ctx, s: &str, quoted: bool) {
    let flag = if quoted { Q } else { ctx.subst_tag };
    emit_tagged(ctx, s, flag);
}

fn emit_tagged(ctx: &mut Ctx, s: &str, flag: u8) {
    for c in s.chars() {
        ctx.push(c, flag);
    }
    ctx.content += 1;
}

fn emit_list(ctx: &mut Ctx, items: &[String], star: bool, quoted: bool) {
    if !ctx.split || (quoted && star) {
        let sep = if star { ifs_first() } else { " ".to_string() };
        emit_str(ctx, &items.join(&sep), quoted);
        return;
    }
    if quoted {
        ctx.at_lists += 1;
        for (k, item) in items.iter().enumerate() {
            if k > 0 {
                ctx.new_field();
            }
            ctx.mark();
            for c in item.chars() {
                ctx.push(c, Q);
            }
        }
    } else {
        for (k, item) in items.iter().enumerate() {
            if k > 0 {
                ctx.new_field();
            }
            let tag = ctx.param_tag;
            for c in item.chars() {
                ctx.push(c, tag);
            }
        }
    }
}

fn emit_val(ctx: &mut Ctx, val: &Val, quoted: bool) {
    match val {
        Val::Unset => emit_str(ctx, "", quoted),
        Val::Str(s) => emit_str(ctx, s, quoted),
        Val::List(items, star) => emit_list(ctx, items, *star, quoted),
        Val::Tagged(fields) => {
            for (k, field) in fields.iter().enumerate() {
                if k > 0 {
                    ctx.new_field();
                }
                for &(c, t) in field {
                    ctx.push(c, t);
                }
            }
            ctx.content += 1;
        }
        Val::Rc(items) => {
            if quoted || items.is_empty() {
                emit_str(ctx, &items.join(" "), quoted);
                return;
            }
            let prefix = ctx.fields.last().cloned().unwrap_or_default();
            let start = ctx.fields.len() - 1;
            let tag = ctx.param_tag;
            for (k, item) in items.iter().enumerate() {
                if k > 0 {
                    ctx.fields.push(prefix.clone());
                }
                for c in item.chars() {
                    ctx.fields.last_mut().unwrap().push((c, tag));
                }
            }
            ctx.rc_from = Some(start);
            ctx.content += 1;
        }
    }
}

// Splits `chars[i..]` for an unterminated construct: returns the end of
// the inner text and the index to resume at.
fn bounds(found: Option<usize>, len: usize) -> (usize, usize) {
    match found {
        Some(e) => (e - 1, e),
        None => (len, len),
    }
}

fn walk(chars: &[char], mode: Mode, ctx: &mut Ctx) -> ExpResult<()> {
    let n = chars.len();
    let lit = if mode == Mode::Unquoted {
        ctx.unquoted_lit
    } else {
        Q
    };
    let mut i = 0;
    while i < n {
        let c = chars[i];
        match c {
            '\\' => {
                let next = chars.get(i + 1).copied();
                let escapes = match mode {
                    Mode::Unquoted => next.is_some(),
                    Mode::Dq => matches!(next, Some('$' | '`' | '"' | '\\' | '\n')),
                    Mode::Heredoc => matches!(next, Some('$' | '`' | '\\' | '\n')),
                };
                if escapes {
                    let e = next.unwrap();
                    if e != '\n' {
                        ctx.push(e, Q);
                    }
                    i += 2;
                } else {
                    ctx.push('\\', Q);
                    i += 1;
                }
            }
            '\'' if mode == Mode::Unquoted => {
                let (ie, next) = bounds(scan_squote(chars, i), n);
                for &q in &chars[i + 1..ie.max(i + 1)] {
                    ctx.push(q, Q);
                }
                ctx.mark();
                i = next;
            }
            '"' if mode == Mode::Unquoted => {
                let (ie, next) = bounds(scan_dquote(chars, i), n);
                let before_content = ctx.content;
                let before_at = ctx.at_lists;
                walk(&chars[i + 1..ie.max(i + 1)], Mode::Dq, ctx)?;
                if ctx.content > before_content || ctx.at_lists == before_at {
                    ctx.mark();
                }
                i = next;
            }
            // Quotes nested inside `${...}` within double quotes.
            '"' if mode == Mode::Dq => i += 1,
            '`' => {
                let (ie, next) = bounds(scan_bquote(chars, i), n);
                let src = unescape_bquote(&chars[i + 1..ie.max(i + 1)], mode == Mode::Dq);
                let out = command_subst(&src)?;
                emit_subst(ctx, &out, mode != Mode::Unquoted);
                i = next;
            }
            '$' => i = expand_dollar(chars, i, mode, ctx)?,
            '<' | '>'
                if mode == Mode::Unquoted
                    && i == 0
                    && ctx.unquoted_lit == 0
                    && chars.get(1) == Some(&'(') =>
            {
                let (ie, next) = bounds(scan_paren(chars, 1), n);
                let src: String = chars[2..ie.max(2)].iter().collect();
                match crate::exec::process_subst(&src, c == '<') {
                    Some(path) => {
                        for d in path.chars() {
                            ctx.push(d, Q);
                        }
                    }
                    None => return Err(ExpError { status: 1 }),
                }
                i = next;
            }
            '~' if mode == Mode::Unquoted && (i == 0 || (ctx.assign && chars[i - 1] == ':')) => {
                match tilde(chars, i, ctx.assign) {
                    Some((dir, next)) => {
                        for d in dir.chars() {
                            ctx.push(d, Q);
                        }
                        i = next;
                    }
                    None => {
                        ctx.push('~', 0);
                        i += 1;
                    }
                }
            }
            _ => {
                ctx.push(c, if mode == Mode::Unquoted { lit | B } else { lit });
                i += 1;
            }
        }
    }
    Ok(())
}

fn tilde(chars: &[char], i: usize, assign: bool) -> Option<(String, usize)> {
    let mut j = i + 1;
    while j < chars.len() && chars[j] != '/' && !(assign && chars[j] == ':') {
        if matches!(chars[j], '\'' | '"' | '\\' | '$' | '`') {
            return None;
        }
        j += 1;
    }
    let user: String = chars[i + 1..j].iter().collect();
    let dir = match user.as_str() {
        "" => var_get_opt("HOME").or_else(|| home_of(None))?,
        "+" => var_get_opt("PWD")?,
        "-" => var_get_opt("OLDPWD")?,
        u => home_of(Some(u))?,
    };
    Some((dir, j))
}

fn home_of(user: Option<&str>) -> Option<String> {
    unsafe {
        let pw = match user {
            Some(u) => {
                let cu = std::ffi::CString::new(u).ok()?;
                libc::getpwnam(cu.as_ptr())
            }
            None => libc::getpwuid(libc::getuid()),
        };
        if pw.is_null() || (*pw).pw_dir.is_null() {
            return None;
        }
        Some(
            std::ffi::CStr::from_ptr((*pw).pw_dir)
                .to_string_lossy()
                .into_owned(),
        )
    }
}

fn unescape_bquote(inner: &[char], in_dq: bool) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < inner.len() {
        if inner[i] == '\\' && i + 1 < inner.len() {
            let n = inner[i + 1];
            if n == '$' || n == '`' || n == '\\' || (in_dq && n == '"') {
                out.push(n);
                i += 2;
                continue;
            }
        }
        out.push(inner[i]);
        i += 1;
    }
    out
}

fn command_subst(src: &str) -> ExpResult<String> {
    // `$(<file)` reads the file without running a command.
    let trimmed = src.trim();
    if let Some(file) = trimmed
        .strip_prefix('<')
        .filter(|f| !f.starts_with(['<', '&', '(']))
    {
        let path = expand_string(file.trim())?;
        let (mut out, status) = match std::fs::read(&path) {
            Ok(b) => (String::from_utf8_lossy(&b).into_owned(), 0),
            Err(e) => {
                eprintln!("meowsh: {}: {}", path, e);
                (String::new(), 1)
            }
        };
        while out.ends_with('\n') {
            out.pop();
        }
        with_shell(|s| s.last_cmdsub_status = Some(status));
        return Ok(out);
    }
    let (mut out, status) = crate::exec::command_subst(src);
    while out.ends_with('\n') {
        out.pop();
    }
    with_shell(|s| s.last_cmdsub_status = Some(status));
    Ok(out)
}

fn eval_arith_str(expr: &str) -> ExpResult<i64> {
    match crate::arith::eval(expr) {
        Ok(v) => Ok(v),
        Err(e) => fail(&e),
    }
}

fn arith_num(expr: &[char]) -> ExpResult<crate::arith::Num> {
    let mut ctx = Ctx::new(false, false);
    walk(expr, Mode::Dq, &mut ctx)?;
    let text = ctx.joined();
    crate::arith::eval_num(&text).or_else(|e| fail(&e))
}

/// Expands and evaluates the text of an arithmetic command; the result
/// is true when non-zero.
pub fn eval_arith(expr: &str) -> ExpResult<bool> {
    let chars: Vec<char> = expr.chars().collect();
    arith_num(&chars).map(crate::arith::Num::truthy)
}

fn is_special_param(c: char) -> bool {
    matches!(c, '@' | '*' | '#' | '?' | '-' | '$' | '!' | '0'..='9')
}

fn expand_dollar(chars: &[char], i: usize, mode: Mode, ctx: &mut Ctx) -> ExpResult<usize> {
    let n = chars.len();
    let quoted = mode != Mode::Unquoted;
    let lit = if quoted { Q } else { 0 };
    let Some(&next) = chars.get(i + 1) else {
        ctx.push('$', lit);
        return Ok(i + 1);
    };
    match next {
        '{' => {
            let (ie, end) = bounds(scan_brace(chars, i + 1, mode == Mode::Dq), n);
            if ie == n {
                return fail("bad substitution");
            }
            expand_braced(&chars[i + 2..ie], mode, ctx)?;
            Ok(end)
        }
        '(' => {
            let arith_end = scan_arith(chars, i + 1);
            let Some(end) = arith_end.or_else(|| scan_paren(chars, i + 1)) else {
                return fail("unterminated command substitution");
            };
            if arith_end.is_some() {
                let v = arith_num(&chars[i + 3..end - 2])?;
                emit_str(ctx, &v.to_string(), quoted);
            } else {
                let src: String = chars[i + 2..end - 1].iter().collect();
                let out = command_subst(&src)?;
                emit_subst(ctx, &out, quoted);
            }
            Ok(end)
        }
        '\'' if mode == Mode::Unquoted => {
            let (ie, end) = bounds(scan_dollar_squote(chars, i + 1), n);
            let s = ansi_c(&chars[i + 2..ie.max(i + 2)]);
            for c in s.chars() {
                ctx.push(c, Q);
            }
            ctx.mark();
            Ok(end)
        }
        // zsh short forms: $+name, $#name, $=name, $^name, $~name, each
        // optionally with a subscript.
        '+' | '#' | '=' | '^' | '~'
            if chars
                .get(i + 2)
                .is_some_and(|&c| c == '_' || c.is_ascii_alphabetic()) =>
        {
            let end = name_with_subscript(chars, i + 2);
            expand_braced(&chars[i + 1..end], mode, ctx)?;
            Ok(end)
        }
        c if c == '_' || c.is_ascii_alphabetic() => {
            let mut j = i + 1;
            while j < n && (chars[j] == '_' || chars[j].is_ascii_alphanumeric()) {
                j += 1;
            }
            // zsh: `$name[sub]` subscripts without braces.
            let end = name_with_subscript(chars, i + 1);
            if end > j {
                expand_braced(&chars[i + 1..end], mode, ctx)?;
                return Ok(end);
            }
            let name: String = chars[i + 1..j].iter().collect();
            let val = ksh_first(&name, lookup(&name));
            check_set(&name, &val)?;
            emit_val(ctx, &val, quoted);
            Ok(j)
        }
        c if is_special_param(c) => {
            // zsh reads all the digits of `$11`; sh and ksh only one.
            let mut end = i + 2;
            if c.is_ascii_digit()
                && with_shell(|s| {
                    matches!(
                        s.emulation,
                        crate::options::Emulation::Zsh | crate::options::Emulation::Csh
                    )
                })
            {
                while end < n && chars[end].is_ascii_digit() {
                    end += 1;
                }
            }
            let name: String = chars[i + 1..end].iter().collect();
            let val = lookup(&name);
            check_set(&name, &val)?;
            emit_val(ctx, &val, quoted);
            Ok(end)
        }
        _ => {
            ctx.push('$', lit);
            Ok(i + 1)
        }
    }
}

// End of a name starting at `i`, plus a `[...]` subscript directly after
// it when the brackets close.
fn name_with_subscript(chars: &[char], i: usize) -> usize {
    let mut j = i;
    while j < chars.len() && (chars[j] == '_' || chars[j].is_ascii_alphanumeric()) {
        j += 1;
    }
    if chars.get(j) != Some(&'[') {
        return j;
    }
    let mut depth = 0;
    let mut k = j;
    while k < chars.len() {
        match chars[k] {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return k + 1;
                }
            }
            '\\' => k += 1,
            '"' | '\'' | ' ' | '\t' | '\n' => return j,
            _ => {}
        }
        k += 1;
    }
    j
}

// Parses a parameter name and optional subscript at the start of `body`.
// Returns (name, subscript, index after them).
fn parse_param(body: &[char]) -> Option<(String, Option<String>, usize)> {
    let first = *body.first()?;
    let mut j;
    if first == '_' || first.is_ascii_alphabetic() {
        j = 1;
        while j < body.len() && (body[j] == '_' || body[j].is_ascii_alphanumeric()) {
            j += 1;
        }
    } else if first.is_ascii_digit() {
        j = 1;
        while j < body.len() && body[j].is_ascii_digit() {
            j += 1;
        }
    } else if is_special_param(first) {
        j = 1;
    } else {
        return None;
    }
    let name: String = body[..j].iter().collect();
    let mut sub = None;
    if j < body.len() && body[j] == '[' {
        let mut depth = 0;
        let mut k = j;
        while k < body.len() {
            match body[k] {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                '\\' => k += 1,
                _ => {}
            }
            k += 1;
        }
        if k < body.len() {
            sub = Some(body[j + 1..k].iter().collect());
            j = k + 1;
        }
    }
    Some((name, sub, j))
}

// ---- ${...}: zsh-style parameter expansion --------------------------------
//
// `braced` computes the value of a `${...}` body in zsh's order: nested
// substitution or name lookup, subscripts, operators or `:` modifiers,
// then flags, length and the `+` set test. Values from the word of
// `${x:-word}` stay tagged so their quoting survives until emission.

#[derive(Default)]
struct PFlags {
    at: bool,
    keys: bool,
    values: bool,
    join: Option<String>,
    split: Option<String>,
    zsplit: bool,
    quote: u8,
    quote_min: bool,
    unquote: bool,
    sort: Option<char>,
    numeric: bool,
    icase: bool,
    unique: bool,
    case: Option<char>,
    prompt: u8,
    visible: bool,
    count_chars: bool,
    words: bool,
    words_empty: bool,
    gflags: Option<String>,
    // (A): `=` assigns an array; (AA) an associative array.
    assign_array: u8,
    backslash: bool,
    eval: bool,
    indirect: bool,
    type_of: bool,
    // (l:n::fill::once:) / (r...): width, repeated fill, one-off string.
    pad_left: Option<(usize, Option<String>, Option<String>)>,
    pad_right: Option<(usize, Option<String>, Option<String>)>,
    escapes: bool,
    tilde_dirs: bool,
    matching: bool,
    substr: bool,
    rest_part: bool,
    bind: bool,
    eind: bool,
    mlen: bool,
    nth: Option<String>,
    // (-): numeric sort of signed numbers.
    signed: bool,
    // `${=name}`: split on IFS.
    ifs_split: bool,
    any: bool,
}

fn closing_delim(c: char) -> char {
    match c {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '<' => '>',
        c => c,
    }
}

// Parses `(flags)` with `body[0] == '('`. Returns the flags and the index
// after the closing `)`.
fn parse_flags(body: &[char]) -> Option<(PFlags, usize)> {
    let mut f = PFlags {
        any: true,
        ..PFlags::default()
    };
    let mut i = 1;
    let arg = |i: &mut usize| -> Option<String> {
        let open = *body.get(*i)?;
        let close = closing_delim(open);
        let start = *i + 1;
        let end = (start..body.len()).find(|&k| body[k] == close)?;
        *i = end + 1;
        Some(body[start..end].iter().collect())
    };
    while i < body.len() {
        let c = body[i];
        i += 1;
        match c {
            ')' => return Some((f, i)),
            '@' => f.at = true,
            'k' => f.keys = true,
            'v' => f.values = true,
            'j' => f.join = Some(arg(&mut i)?),
            's' => f.split = Some(arg(&mut i)?),
            'f' => f.split = Some("\n".into()),
            'F' => f.join = Some("\n".into()),
            '0' => f.split = Some("\0".into()),
            'z' => f.zsplit = true,
            'q' => {
                if matches!(body.get(i), Some('-' | '+')) {
                    f.quote_min = true;
                    i += 1;
                } else {
                    f.quote += 1;
                }
            }
            'Q' => f.unquote = true,
            'o' | 'O' => f.sort = Some(c),
            'a' => {}
            'n' => f.numeric = true,
            '-' => f.signed = true,
            'i' => f.icase = true,
            'u' => f.unique = true,
            'U' | 'L' | 'C' => f.case = Some(c),
            '%' => f.prompt += 1,
            'V' => f.visible = true,
            'c' => f.count_chars = true,
            'w' => f.words = true,
            'W' => f.words_empty = true,
            'g' => f.gflags = Some(arg(&mut i)?),
            'b' => f.backslash = true,
            'e' => f.eval = true,
            'P' => f.indirect = true,
            't' => f.type_of = true,
            'D' => f.tilde_dirs = true,
            'M' => f.matching = true,
            'S' => f.substr = true,
            'R' => f.rest_part = true,
            'B' => f.bind = true,
            'E' => f.eind = true,
            'N' => f.mlen = true,
            'I' => f.nth = Some(arg(&mut i)?),
            'p' => f.escapes = true,
            'l' | 'r' => {
                let d = *body.get(i)?;
                let n_text = arg(&mut i)?;
                let n = eval_arith_str(n_text.trim()).ok()?.max(0) as usize;
                // Further strings must use the same delimiter.
                let mut more = Vec::new();
                while more.len() < 2 && body.get(i) == Some(&d) {
                    let s = arg(&mut i)?;
                    more.push(if f.escapes { decode_escapes(&s) } else { s });
                }
                let mut it = more.into_iter();
                let entry = (n, it.next(), it.next());
                if c == 'l' {
                    f.pad_left = Some(entry);
                } else {
                    f.pad_right = Some(entry);
                }
            }
            // Accepted without effect: A (assignment), R/B/E/N (match
            // parts), V, w, W, c, X, ~, #, m, S, I, p, g.
            'A' => f.assign_array += 1,
            'X' | '~' | '#' | 'm' => {}
            _ => return None,
        }
    }
    None
}

fn val_items(v: &Val) -> Vec<String> {
    match v {
        Val::Unset => Vec::new(),
        Val::Str(s) => vec![s.clone()],
        Val::List(items, _) | Val::Rc(items) => items.clone(),
        Val::Tagged(fields) => fields.iter().map(|f| tagged_text(f)).collect(),
    }
}

fn tagged_text(f: &[(char, u8)]) -> String {
    f.iter()
        .filter(|(_, t)| t & MARK == 0)
        .map(|(c, _)| *c)
        .collect()
}

// Drops quoting tags: one field becomes a scalar, several a list.
fn plain(v: Val) -> Val {
    match v {
        Val::Tagged(fields) => {
            if fields.len() == 1 {
                Val::Str(tagged_text(&fields[0]))
            } else {
                Val::List(fields.iter().map(|f| tagged_text(f)).collect(), false)
            }
        }
        other => other,
    }
}

fn is_list(v: &Val) -> bool {
    matches!(v, Val::List(..) | Val::Rc(_))
}

fn map_items(v: Val, f: impl Fn(&str) -> String) -> Val {
    match plain(v) {
        Val::Unset => Val::Unset,
        Val::Str(s) => Val::Str(f(&s)),
        Val::List(items, star) => Val::List(items.iter().map(|s| f(s)).collect(), star),
        Val::Rc(items) => Val::Rc(items.iter().map(|s| f(s)).collect()),
        Val::Tagged(_) => unreachable!(),
    }
}

// Expands the word operand of `${x-word}` and friends, keeping tags.
fn word_val(word: &[char], mode: Mode) -> ExpResult<Val> {
    let mut tmp = Ctx::new(true, false);
    if mode == Mode::Unquoted {
        tmp.unquoted_lit = tmp.param_tag;
    }
    let m = if mode == Mode::Heredoc {
        Mode::Dq
    } else {
        mode
    };
    walk(word, m, &mut tmp)?;
    Ok(Val::Tagged(tmp.fields))
}

fn sh_mode() -> bool {
    with_shell(|s| matches!(s.emulation, crate::options::Emulation::Sh))
}

fn ksh_arrays() -> bool {
    crate::options::quick("ksharrays")
}

// KSH_ARRAYS subscripts count from 0: shifts a non-negative index to
// zsh's 1-based numbering.
fn ksh_index(i: i64) -> i64 {
    if i >= 0 && ksh_arrays() {
        i + 1
    } else {
        i
    }
}

// KSH_ARRAYS: `$arr` without a subscript is the first element.
fn ksh_first(name: &str, val: Val) -> Val {
    if !ksh_arrays() || !with_shell(|s| s.array(name).is_some()) {
        return val;
    }
    match val {
        Val::List(items, _) => items.into_iter().next().map(Val::Str).unwrap_or(Val::Unset),
        v => v,
    }
}

// 1-based zsh index into `len` items (negative counts from the end).
fn index_of(i: i64, len: usize) -> Option<usize> {
    let len = len as i64;
    let k = if i > 0 {
        i - 1
    } else if i < 0 {
        len + i
    } else if sh_mode() {
        0
    } else {
        // zsh: element 0 does not exist (KSH_ZERO_SUBSCRIPT aside).
        -1
    };
    (0..len).contains(&k).then_some(k as usize)
}

// `[a,b]` range bounds, inclusive, as a Rust range.
fn range_of(a: i64, b: i64, len: usize) -> std::ops::Range<usize> {
    let l = len as i64;
    let norm = |x: i64| if x < 0 { l + x + 1 } else { x };
    let (s, e) = (norm(a).max(1), norm(b).min(l));
    if s > e {
        0..0
    } else {
        (s - 1) as usize..e as usize
    }
}

// Applies a subscript to a value that is not a named associative array.
fn apply_subscript(val: Val, sub: &str) -> ExpResult<Val> {
    let val = plain(val);
    if sub == "@" || sub == "*" {
        let items = val_items(&val);
        return Ok(Val::List(items, sub == "*"));
    }
    if let Some(v) = search_items(&val_items(&val), sub, is_list(&val))? {
        return Ok(v);
    }
    let text = expand_string(sub)?;
    let (a, b) = match find_unquoted(&text.chars().collect::<Vec<_>>(), ',') {
        Some(k) => (
            ksh_index(eval_arith_str(&text[..k])?),
            Some(ksh_index(eval_arith_str(&text[k + 1..])?)),
        ),
        None => (ksh_index(eval_arith_str(&text)?), None),
    };
    Ok(match val {
        Val::Unset => Val::Unset,
        Val::List(items, _) | Val::Rc(items) if !items.is_empty() || b.is_some() => match b {
            Some(b) => Val::List(items[range_of(a, b, items.len())].to_vec(), false),
            None => match index_of(
                if a == 0 && crate::options::quick("kshzerosubscript") {
                    1
                } else {
                    a
                },
                items.len(),
            ) {
                Some(k) => Val::Str(items[k].clone()),
                None => Val::Unset,
            },
        },
        Val::List(..) | Val::Rc(_) => Val::Unset,
        Val::Str(s) => {
            let chars: Vec<char> = s.chars().collect();
            match b {
                Some(b) => Val::Str(chars[range_of(a, b, chars.len())].iter().collect()),
                // Index 0: bash's `${scalar[0]}` is the value; zsh's is
                // empty, or the first character with KSH_ZERO_SUBSCRIPT.
                None if a == 0 && crate::options::quick("kshzerosubscript") => {
                    Val::Str(chars.first().map(|c| c.to_string()).unwrap_or_default())
                }
                None if a == 0 && sh_mode() => Val::Str(s),
                None => match index_of(a, chars.len()) {
                    Some(k) => Val::Str(chars[k].to_string()),
                    None => Val::Unset,
                },
            }
        }
        Val::Tagged(_) => unreachable!(),
    })
}

// zsh subscript flags on a list: `(i)`/`(I)` first/last matching index,
// `(r)`/`(R)` first/last matching value, `(e)` literal match. None when
// `sub` has no such flags.
fn search_items(items: &[String], sub: &str, list: bool) -> ExpResult<Option<Val>> {
    let Some((flags, word)) = search_flags(sub) else {
        return Ok(None);
    };
    let hit = matcher(word, flags.contains('e'))?;
    let last = flags.contains('I') || flags.contains('R');
    let want_value = flags.contains('r') || flags.contains('R');
    let items: Vec<String> = if list {
        items.to_vec()
    } else if flags.contains('w') {
        // A scalar searched word by word.
        items
            .first()
            .map(|s| s.split_whitespace().map(|w| w.to_string()).collect())
            .unwrap_or_default()
    } else {
        // A scalar searches its characters.
        items
            .first()
            .map(|s| s.chars().map(|c| c.to_string()).collect())
            .unwrap_or_default()
    };
    let mut idx = items
        .iter()
        .enumerate()
        .filter(|(_, v)| hit(v))
        .map(|(k, _)| k);
    let found = if last { idx.next_back() } else { idx.next() };
    Ok(Some(match (found, want_value) {
        (Some(k), true) => Val::Str(items[k].clone()),
        (None, true) => Val::Unset,
        (Some(k), false) => Val::Str((k + usize::from(!ksh_arrays())).to_string()),
        (None, false) => Val::Str(
            if last {
                0
            } else {
                items.len() + usize::from(!ksh_arrays())
            }
            .to_string(),
        ),
    }))
}

fn search_flags(sub: &str) -> Option<(&str, &str)> {
    let rest = sub.strip_prefix('(')?;
    let close = rest.find(')')?;
    let flags = &rest[..close];
    (!flags.is_empty() && flags.chars().all(|c| "iIrReEkKw".contains(c)))
        .then(|| (flags, &rest[close + 1..]))
}

// A leading `^` negates the pattern (zsh EXTENDED_GLOB).
fn matcher(word: &str, exact: bool) -> ExpResult<impl Fn(&str) -> bool> {
    let (negate, word) = match word.strip_prefix('^') {
        Some(rest) if !exact => (true, rest),
        _ => (false, word),
    };
    let pat = if exact {
        expand_string(word)?
    } else {
        expand_pattern(word)?
    };
    Ok(move |v: &str| {
        let m = if exact {
            v == pat
        } else {
            crate::pattern::matches(&pat, v)
        };
        m != negate
    })
}

// Value of `name[sub]` for a named parameter.
fn param_value(name: &str, sub: Option<&str>) -> ExpResult<Val> {
    let Some(sub) = sub else {
        return Ok(lookup(name));
    };
    let resolved = crate::shell::resolve_ref(name);
    let name = if resolved.contains('[') {
        name
    } else {
        resolved.as_str()
    };
    if name == "commands" && !with_shell(|s| s.assoc("commands").is_some()) {
        let key = expand_string(sub)?;
        return Ok(match crate::shell::find_command(&key) {
            Some(p) => Val::Str(p),
            None => Val::Unset,
        });
    }
    let is_assoc = with_shell(|s| s.assoc(name).is_some());
    if !is_assoc {
        return apply_subscript(lookup(name), sub);
    }
    if sub == "@" || sub == "*" {
        return Ok(Val::List(
            assoc_values(name).unwrap_or_default(),
            sub == "*",
        ));
    }
    if let Some((flags, word)) = search_flags(sub) {
        let hit = matcher(word, flags.contains('e'))?;
        let last = flags.contains('I') || flags.contains('R');
        let want_value = flags.contains('r') || flags.contains('R');
        let keys = assoc_keys(name).unwrap_or_default();
        let mut found = keys.iter().filter_map(|k| {
            let v = assoc_get(name, k).unwrap_or_default();
            if want_value {
                hit(&v).then(|| (k.clone(), v))
            } else {
                hit(k).then(|| (k.clone(), v))
            }
        });
        let r = if last {
            found.next_back()
        } else {
            found.next()
        };
        // (r)/(R) give the value, (i)/(I) the key; `(k)` on the whole
        // expansion turns a value search into its key (handled by caller
        // through `keys` flag).
        return Ok(match r {
            Some((k, v)) => Val::Str(if want_value { v } else { k }),
            None => Val::Unset,
        });
    }
    let key = expand_string(sub)?;
    Ok(match assoc_get(name, &key) {
        Some(v) => Val::Str(v),
        None => Val::Unset,
    })
}

fn modifier_letters(c: char) -> bool {
    "aAcehlPqQrstugx&".contains(c)
}

// Applies `:` history-style modifiers starting at `rest[0] == ':'`.
// Returns None if `rest` is not a modifier list.
fn apply_modifiers(val: Val, rest: &[char]) -> ExpResult<Option<Val>> {
    if !rest.get(1).is_some_and(|&c| modifier_letters(c)) {
        return Ok(None);
    }
    let mut val = plain(val);
    let mut i = 0;
    while i < rest.len() {
        if rest[i] != ':' {
            return Ok(None);
        }
        i += 1;
        let mut global = false;
        if rest.get(i) == Some(&'g') {
            global = true;
            i += 1;
        }
        let Some(&m) = rest.get(i) else {
            return Ok(None);
        };
        i += 1;
        match m {
            's' => {
                let Some(&d) = rest.get(i) else {
                    return Ok(None);
                };
                i += 1;
                let mut parts = vec![String::new()];
                while i < rest.len() && parts.len() <= 2 {
                    if rest[i] == d {
                        parts.push(String::new());
                    } else {
                        parts.last_mut().unwrap().push(rest[i]);
                    }
                    i += 1;
                }
                if parts.len() < 2 {
                    return Ok(None);
                }
                // A third delimiter is optional at the end.
                let (old, new) = (parts[0].clone(), parts[1].clone());
                if parts.len() == 3 && !parts[2].is_empty() {
                    i -= parts[2].chars().count();
                }
                val = map_items(val, |s| {
                    let new = new.replace('&', &old);
                    if global {
                        s.replace(&old, &new)
                    } else {
                        s.replacen(&old, &new, 1)
                    }
                });
            }
            'h' | 't' => {
                let mut n = 0usize;
                while rest.get(i).is_some_and(|c| c.is_ascii_digit()) {
                    n = n * 10 + rest[i].to_digit(10).unwrap() as usize;
                    i += 1;
                }
                val = map_items(val, |s| {
                    if m == 'h' {
                        path_head(s, n)
                    } else {
                        path_tail(s, n)
                    }
                });
            }
            'r' => val = map_items(val, path_root),
            'e' => val = map_items(val, path_ext),
            'a' => val = map_items(val, absolute_path),
            'A' | 'P' => {
                val = map_items(val, |s| {
                    std::fs::canonicalize(absolute_path(s))
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| absolute_path(s))
                })
            }
            'c' => {
                val = map_items(val, |s| {
                    crate::shell::find_command(s).unwrap_or_else(|| s.to_string())
                })
            }
            'l' => val = map_items(val, |s| s.to_lowercase()),
            'u' => val = map_items(val, |s| s.to_uppercase()),
            'q' => val = map_items(val, backslash_quote),
            'Q' => val = map_items(val, unquote),
            'x' => {
                val = map_items(val, |s| {
                    s.split_whitespace()
                        .map(crate::builtins::shell_quote)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
            }
            _ => return Ok(None),
        }
    }
    Ok(Some(val))
}

fn path_head(s: &str, n: usize) -> String {
    if n > 0 {
        let parts: Vec<&str> = s.split('/').collect();
        return parts[..n.min(parts.len())].join("/");
    }
    match s.trim_end_matches('/').rfind('/') {
        Some(0) => "/".to_string(),
        Some(k) => s[..k].to_string(),
        None => ".".to_string(),
    }
}

fn path_tail(s: &str, n: usize) -> String {
    let t = s.trim_end_matches('/');
    let parts: Vec<&str> = t.split('/').collect();
    let n = n.max(1).min(parts.len());
    parts[parts.len() - n..].join("/")
}

fn path_root(s: &str) -> String {
    let base = s.rfind('/').map(|k| k + 1).unwrap_or(0);
    match s[base..].rfind('.') {
        Some(k) => s[..base + k].to_string(),
        None => s.to_string(),
    }
}

fn path_ext(s: &str) -> String {
    let base = s.rfind('/').map(|k| k + 1).unwrap_or(0);
    match s[base..].rfind('.') {
        Some(k) => s[base + k + 1..].to_string(),
        None => String::new(),
    }
}

fn absolute_path(s: &str) -> String {
    let full = if s.starts_with('/') {
        s.to_string()
    } else {
        format!("{}/{}", var_get("PWD"), s)
    };
    let mut parts: Vec<&str> = Vec::new();
    for p in full.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    format!("/{}", parts.join("/"))
}

// zsh `:q` / `(q)`: backslash before the characters special to the
// shell; `~` only where it would expand (at the start).
// (V): control characters as ^X, DEL as ^?.
fn visible(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '\x7f' => out.push_str("^?"),
            c if (c as u32) < 32 => {
                out.push('^');
                out.push(char::from(b'@' + c as u8));
            }
            c => out.push(c),
        }
    }
    out
}

// (qqqq): $'...' quoting.
fn dollar_quote(s: &str) -> String {
    let mut out = String::from("$'");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\x1b' => out.push_str("\\e"),
            c if (c as u32) < 32 || c == '\x7f' => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

// (g:opts:): echo/print-style escapes; `e` adds \e and \M-, `c` ^X.
fn g_escapes(s: &str, opts: &str) -> String {
    let mut out = decode_escapes(s);
    if opts.contains('c') {
        let chars: Vec<char> = out.chars().collect();
        let mut r = String::new();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '^' && i + 1 < chars.len() {
                let c = chars[i + 1];
                r.push(if c == '?' {
                    '\x7f'
                } else {
                    char::from((c.to_ascii_uppercase() as u8) & 0x1f)
                });
                i += 2;
            } else {
                r.push(chars[i]);
                i += 1;
            }
        }
        out = r;
    }
    out
}

fn backslash_quote(s: &str) -> String {
    let mut out = String::new();
    for (k, c) in s.chars().enumerate() {
        if " \t\n\\'\"`$&|;<>()[]{}*?#^=!".contains(c) || (c == '~' && k == 0) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// One level of shell quote removal, without expansion.
fn unquote(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' if i + 1 < chars.len() => {
                out.push(chars[i + 1]);
                i += 2;
            }
            '\'' => {
                let end = scan_squote(&chars, i).unwrap_or(chars.len() + 1);
                out.extend(&chars[i + 1..(end - 1).min(chars.len())]);
                i = end;
            }
            '$' if chars.get(i + 1) == Some(&'\'') => {
                let end = scan_dollar_squote(&chars, i + 1).unwrap_or(chars.len() + 1);
                out.push_str(&ansi_c(&chars[i + 2..(end - 1).min(chars.len())]));
                i = end;
            }
            '"' => {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\'
                        && chars
                            .get(i + 1)
                            .is_some_and(|c| matches!(c, '$' | '`' | '"' | '\\'))
                    {
                        i += 1;
                    }
                    out.push(chars[i]);
                    i += 1;
                }
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

// `(z)`: splits like the shell lexer, keeping quotes in the words.
fn shell_words(s: &str) -> Vec<String> {
    let mut lx = crate::lexer::Lexer::new(s, true);
    let mut out = Vec::new();
    loop {
        match lx.next_token() {
            Ok(crate::lexer::Tok::Word(w)) => out.push(w),
            Ok(crate::lexer::Tok::Op(o)) => out.push(o.to_string()),
            Ok(crate::lexer::Tok::IoNumber(n)) => out.push(n.to_string()),
            Ok(crate::lexer::Tok::Arith(e)) => out.push(format!("(({}))", e)),
            Ok(crate::lexer::Tok::Newline) => out.push(";".to_string()),
            Ok(crate::lexer::Tok::Eof) | Err(_) => break,
        }
    }
    out
}

// `${(t)name}`, in zsh's format: the type, then -local and attributes.
fn type_of(name: &str) -> String {
    use crate::params::*;
    with_shell(|s| {
        let Some(p) = s.param(name) else {
            return String::new();
        };
        let a = p.attrs;
        let mut t = String::from(match &p.value {
            Value::Array(_) => "array",
            Value::Assoc(_) => "association",
            Value::Scalar(_) if a & NAMEREF != 0 => "nameref",
            Value::Scalar(_) if a & INTEGER != 0 => "integer",
            Value::Scalar(_) if a & (FLOAT_E | FLOAT_F) != 0 => "float",
            Value::Scalar(_) => "scalar",
        });
        if p.level > 0 {
            t.push_str("-local");
        }
        for (bit, word) in [
            (LEFT, "-left"),
            (RIGHT_B, "-right_blanks"),
            (RIGHT_Z, "-right_zeros"),
            (LOWER, "-lower"),
            (UPPER, "-upper"),
            (READONLY, "-readonly"),
            (TAG, "-tag"),
        ] {
            if a & bit != 0 {
                t.push_str(word);
            }
        }
        if p.tie.is_some() {
            t.push_str("-tied");
        }
        for (bit, word) in [
            (EXPORT, "-export"),
            (UNIQUE, "-unique"),
            (HIDE, "-hide"),
            (HIDEVAL, "-hideval"),
            (SPECIAL, "-special"),
        ] {
            if a & bit != 0 {
                t.push_str(word);
            }
        }
        t
    })
}

// zsh's dopadding (Src/subst.c): pads or truncates `s` to `prenum`
// characters on the left and/or `postnum` on the right. `*one` strings
// go next to the text once; `*mul` strings repeat to fill.
fn dopadding(
    s: &str,
    prenum: usize,
    postnum: usize,
    preone: Option<&str>,
    postone: Option<&str>,
    premul: Option<&str>,
    postmul: Option<&str>,
) -> String {
    let def: Vec<char> = ifs().chars().next().map(|c| vec![c]).unwrap_or_default();
    let one = |o: Option<&str>| {
        o.map(|x| {
            if x.is_empty() {
                def.clone()
            } else {
                x.chars().collect::<Vec<_>>()
            }
        })
    };
    let mul = |m: Option<&str>| match m {
        Some(x) if !x.is_empty() => x.chars().collect::<Vec<_>>(),
        _ => def.clone(),
    };
    let (preone, postone, premul, postmul) = (one(preone), one(postone), mul(premul), mul(postmul));
    let s: Vec<char> = s.chars().collect();
    let ls = s.len();
    if prenum + postnum == ls {
        return s.iter().collect();
    }
    let mut r: Vec<char> = Vec::new();
    // Left fill of width f before text (f > 0).
    let left_fill = |r: &mut Vec<char>, f: usize| {
        let lone = preone.as_ref().map(|p| p.len()).unwrap_or(0);
        if f <= lone {
            if let Some(p) = &preone {
                r.extend(&p[lone - f..]);
            }
        } else {
            let f = f - lone;
            let lm = premul.len();
            if let Some(reps) = f.checked_div(lm) {
                let m = f % lm;
                if m > 0 {
                    r.extend(&premul[lm - m..]);
                }
                for _ in 0..reps {
                    r.extend(&premul);
                }
            }
            if let Some(p) = &preone {
                r.extend(p);
            }
        }
    };
    let right_fill = |r: &mut Vec<char>, f: usize| {
        let lone = postone.as_ref().map(|p| p.len()).unwrap_or(0);
        if f <= lone {
            if let Some(p) = &postone {
                r.extend(&p[..f]);
            }
        } else {
            let f = f - lone;
            if let Some(p) = &postone {
                r.extend(p);
            }
            let lm = postmul.len();
            if let Some(reps) = f.checked_div(lm) {
                for _ in 0..reps {
                    r.extend(&postmul);
                }
                r.extend(&postmul[..f % lm]);
            }
        }
    };
    let mut pos = 0;
    if prenum > 0 {
        let first = if postnum > 0 { ls / 2 } else { ls };
        if prenum <= first {
            pos = first - prenum;
            r.extend(&s[pos..pos + prenum]);
            pos += prenum;
        } else {
            left_fill(&mut r, prenum - first);
            r.extend(&s[..first]);
            pos = first;
        }
        if postnum == 0 {
            return r.into_iter().collect();
        }
    }
    if postnum > 0 {
        let rest = &s[pos..];
        let need = if prenum > 0 { ls - ls / 2 } else { ls };
        if postnum <= need {
            r.extend(rest.iter().take(postnum));
        } else {
            r.extend(rest);
            right_fill(&mut r, postnum - need);
        }
    }
    r.into_iter().collect()
}

fn split_items(items: Vec<String>, sep: &str, keep_empty: bool) -> Vec<String> {
    let mut out = Vec::new();
    for item in items {
        for part in item.split(sep) {
            if keep_empty || !part.is_empty() {
                out.push(part.to_string());
            }
        }
    }
    out
}

// zsh's numeric sort ((n), and (-) for signed numbers): runs of digits
// compare by value, everything else character by character.
fn numeric_cmp(a: &str, b: &str, signed: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (mut i, mut j) = (0, 0);
    let number = |s: &[char], k: usize| -> Option<(i128, usize)> {
        let neg =
            signed && s.get(k) == Some(&'-') && s.get(k + 1).is_some_and(|c| c.is_ascii_digit());
        let start = if neg { k + 1 } else { k };
        if !s.get(start).is_some_and(|c| c.is_ascii_digit()) {
            return None;
        }
        let mut end = start;
        while end < s.len() && s[end].is_ascii_digit() {
            end += 1;
        }
        let text: String = s[start..end].iter().collect();
        let v: i128 = text.parse().unwrap_or(i128::MAX);
        Some((if neg { -v } else { v }, end))
    };
    while i < a.len() && j < b.len() {
        if let (Some((x, ni)), Some((y, nj))) = (number(&a, i), number(&b, j)) {
            match x.cmp(&y) {
                Ordering::Equal => {
                    i = ni;
                    j = nj;
                    continue;
                }
                o => return o,
            }
        }
        match a[i].cmp(&b[j]) {
            Ordering::Equal => {
                i += 1;
                j += 1;
            }
            o => return o,
        }
    }
    (a.len() - i).cmp(&(b.len() - j))
}

// The flags in zsh's order (the "Rules" in zshexpn): forced joining,
// splitting, case, escapes, quoting, directory names, visibility,
// lexical splitting, uniqueness, ordering, re-evaluation, padding.
fn apply_flags(val: Val, f: &PFlags, quoted: bool) -> ExpResult<Val> {
    let mut val = plain(val);
    let list_in = is_list(&val);
    let wrap = |items: Vec<String>, was_list: bool| {
        if was_list {
            Val::List(items, false)
        } else {
            Val::Str(items.into_iter().next().unwrap_or_default())
        }
    };
    if f.indirect {
        val = match val {
            Val::Str(name) => lookup(&name),
            v => v,
        };
    }
    let splitting = f.split.is_some() || f.ifs_split;
    let mut list = is_list(&val) || list_in;
    // Rule 10: (j), or splitting, first joins the words.
    if (f.join.is_some() || splitting) && is_list(&val) {
        let sep = f.join.clone().unwrap_or_else(ifs_first);
        val = Val::Str(val_items(&val).join(&sep));
        list = false;
    }
    if let Some(sep) = &f.split {
        let items = val_items(&val);
        val = Val::List(
            split_items(items, sep, (f.at && quoted) || f.words_empty),
            false,
        );
    } else if f.ifs_split {
        let ifs = ifs();
        let words: Vec<String> = val_items(&val)
            .iter()
            .flat_map(|s| {
                s.split(|c| ifs.contains(c))
                    .filter(|w| !w.is_empty())
                    .map(|w| w.to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
        val = Val::List(words, false);
    } else if f.zsplit {
        let items = val_items(&val);
        val = Val::List(items.iter().flat_map(|s| shell_words(s)).collect(), false);
    }
    list = is_list(&val) || list;
    let mut items = val_items(&val);
    if matches!(val, Val::Unset) && !list {
        return Ok(Val::Unset);
    }
    for s in items.iter_mut() {
        match f.case {
            Some('U') => *s = s.to_uppercase(),
            Some('L') => *s = s.to_lowercase(),
            Some('C') => {
                let mut out = String::new();
                let mut start = true;
                for c in s.chars() {
                    if start {
                        out.extend(c.to_uppercase());
                    } else {
                        out.extend(c.to_lowercase());
                    }
                    start = !c.is_alphanumeric();
                }
                *s = out;
            }
            _ => {}
        }
        if f.prompt == 1 {
            *s = crate::shell::expand_prompt(s);
        } else if f.prompt > 1 {
            *s = crate::shell::render_prompt(s);
        }
        if let Some(g) = &f.gflags {
            *s = g_escapes(s, g);
        }
        if f.unquote {
            *s = unquote(s);
        }
        if f.quote_min {
            *s = crate::builtins::shell_quote(s);
        } else {
            match f.quote {
                0 => {}
                1 => *s = backslash_quote(s),
                2 => *s = format!("'{}'", s.replace('\'', "'\\''")),
                4.. => *s = dollar_quote(s),
                _ => {
                    let mut o = String::from('"');
                    for c in s.chars() {
                        if matches!(c, '\\' | '"' | '$' | '`') {
                            o.push('\\');
                        }
                        o.push(c);
                    }
                    o.push('"');
                    *s = o;
                }
            }
        }
        if f.backslash {
            let mut out = String::new();
            for c in s.chars() {
                if "*?[]()|<>~#^\\".contains(c) {
                    out.push('\\');
                }
                out.push(c);
            }
            *s = out;
        }
        if f.tilde_dirs {
            let home = var_get("HOME");
            if !home.is_empty() && (s == &home || s.starts_with(&format!("{}/", home))) {
                *s = format!("~{}", &s[home.len()..]);
            }
        }
        if f.visible {
            *s = visible(s);
        }
    }
    if f.unique {
        let mut seen = std::collections::HashSet::new();
        items.retain(|s| seen.insert(s.clone()));
    }
    if f.sort.is_some() || f.numeric || f.signed {
        if f.numeric || f.signed {
            items.sort_by(|a, b| numeric_cmp(a, b, f.signed));
        } else if f.icase {
            items.sort_by_key(|s| s.to_lowercase());
        } else {
            items.sort();
        }
        if f.sort == Some('O') {
            items.reverse();
        }
    }
    if f.eval {
        // (e): expand each value again, as if it appeared here; the
        // results split and glob like any expansion.
        let mut fields: Vec<Vec<(char, u8)>> = Vec::new();
        for it in &items {
            let chars: Vec<char> = it.chars().collect();
            let mut tmp = Ctx::new(true, false);
            walk(
                &chars,
                if quoted { Mode::Dq } else { Mode::Unquoted },
                &mut tmp,
            )?;
            fields.extend(tmp.fields);
        }
        return Ok(Val::Tagged(fields));
    }
    if f.pad_left.is_some() || f.pad_right.is_some() {
        let (pre, premul, preone) = f.pad_left.clone().unwrap_or((0, None, None));
        let (post, postmul, postone) = f.pad_right.clone().unwrap_or((0, None, None));
        for s in items.iter_mut() {
            *s = dopadding(
                s,
                pre,
                post,
                preone.as_deref(),
                postone.as_deref(),
                premul.as_deref(),
                postmul.as_deref(),
            );
        }
    }
    let mut out = wrap(items, list);
    if f.at {
        if let Val::List(items, _) = out {
            out = Val::List(items, false);
        }
    }
    Ok(out)
}

fn braced(body: &[char], mode: Mode) -> ExpResult<Val> {
    let text: String = body.iter().collect();
    let bad = || fail::<Val>(&format!("${{{}}}: bad substitution", text));
    if body.is_empty() {
        return bad();
    }
    let quoted = mode != Mode::Unquoted;
    let mut i = 0;
    let mut flags = PFlags::default();
    if body[0] == '(' {
        let Some((f, end)) = parse_flags(body) else {
            return fail(&format!("error in flags in '${{{}}}'", text));
        };
        flags = f;
        i = end;
    }
    // Prefix operators: # length, + set test, ^ rc-expansion, = split,
    // ~ glob, ! bash indirection.
    let (mut length, mut set_test, mut rc, mut ifs_split, mut indirect) =
        (false, false, false, false, false);
    while let Some(&c) = body.get(i) {
        let next = body.get(i + 1).copied();
        let starts_param = |n: Option<char>| {
            n.is_some_and(|n| {
                n == '_' || n == '$' || n == '(' || n.is_alphanumeric() || is_special_param(n)
            })
        };
        match c {
            // `${##}` is the length of $#, but `${##pat}` removes pat
            // from $#.
            '#' if starts_param(next) && !(next == Some('#') && i + 2 < body.len()) => {
                length = true
            }
            '+' if starts_param(next) => set_test = true,
            '^' if starts_param(next) || matches!(next, Some('^' | ':')) => rc = !rc,
            // `=` splits words, `==` turns SH_WORD_SPLIT off; `~` and
            // `~~` do the same for GLOB_SUBST (see prefix_toggles).
            '=' if starts_param(next) || matches!(next, Some('=' | ':')) => {
                ifs_split = next != Some('=');
                if !ifs_split {
                    i += 1;
                }
            }
            // `~` is never a parameter name: ${~:-*} has no name.
            '~' => {}
            '!' if starts_param(next) && i == 0 => indirect = true,
            _ => break,
        }
        i += 1;
    }

    // The value source: a nested ${...} or $(...), or a parameter name.
    let mut name = String::new();
    let mut sub = None;
    let mut val;
    let nested = body.get(i) == Some(&'$');
    if body.get(i) == Some(&'$') && body.get(i + 1) == Some(&'{') {
        let Some(end) = scan_brace(body, i + 1, false) else {
            return bad();
        };
        val = braced(&body[i + 2..end - 1], Mode::Unquoted)?;
        i = end;
    } else if body.get(i) == Some(&'$') && body.get(i + 1) == Some(&'(') {
        let arith_end = scan_arith(body, i + 1);
        let Some(end) = arith_end.or_else(|| scan_paren(body, i + 1)) else {
            return bad();
        };
        val = if arith_end.is_some() {
            Val::Str(arith_num(&body[i + 3..end - 2])?.to_string())
        } else {
            let src: String = body[i + 2..end - 1].iter().collect();
            Val::Str(command_subst(&src)?)
        };
        i = end;
    } else if body.get(i) == Some(&'"') {
        // ${(f)"$(cmd)"}: a quoted word as the value source.
        let Some(end) = scan_dquote(body, i) else {
            return bad();
        };
        val = Val::Str(word_string(&body[i + 1..end - 1], Mode::Dq)?);
        i = end;
    } else if let Some((n, s, end)) = parse_param(&body[i..]) {
        name = n;
        sub = s;
        i += end;
        // ${!ref} on a nameref: the name it refers to.
        let is_ref = with_shell(|s| {
            s.param(&name)
                .is_some_and(|p| p.attrs & crate::params::NAMEREF != 0)
        });
        if indirect && is_ref {
            return Ok(Val::Str(with_shell(|s| {
                s.scalar(&name).unwrap_or("").to_string()
            })));
        }
        if indirect {
            let target = var_get(&name);
            let tchars: Vec<char> = target.chars().collect();
            match parse_param(&tchars) {
                Some((n2, s2, e2)) if e2 == tchars.len() => {
                    name = n2;
                    sub = s2;
                }
                _ if target.is_empty() => return Ok(Val::Str(String::new())),
                _ => return fail(&format!("{}: bad substitution", target)),
            }
        }
        if set_test && i == body.len() {
            let set = if name == "commands" && sub.is_some() {
                !matches!(param_value(&name, sub.as_deref())?, Val::Unset)
            } else if let Some(s) = &sub {
                // ${+name[key]}: is the element set?
                !matches!(param_value(&name, Some(s))?, Val::Unset)
            } else {
                crate::shell::var_exists(&name)
            };
            return Ok(Val::Str(if set { "1" } else { "0" }.into()));
        }
        val = if name == "commands" && sub.is_none() && flags.keys {
            Val::List(crate::shell::all_commands(), false)
        } else if flags.keys && sub.is_none() {
            match assoc_keys(&name) {
                Some(keys) if flags.values => Val::List(
                    keys.iter()
                        .flat_map(|k| [k.clone(), assoc_get(&name, k).unwrap_or_default()])
                        .collect(),
                    false,
                ),
                Some(keys) => Val::List(keys, true),
                None => param_value(&name, None)?,
            }
        } else if flags.keys && sub.as_deref().is_some_and(|s| search_flags(s).is_some()) {
            // ${(k)assoc[(r)pat]}: the key whose value matches.
            let s = sub.clone().unwrap();
            let (sf, word) = search_flags(&s).unwrap();
            let hit = matcher(word, sf.contains('e'))?;
            let keys = assoc_keys(&name).unwrap_or_default();
            let mut found = keys
                .into_iter()
                .filter(|k| hit(&assoc_get(&name, k).unwrap_or_default()));
            let r = if sf.contains('R') || sf.contains('I') {
                found.next_back()
            } else {
                found.next()
            };
            r.map(Val::Str).unwrap_or(Val::Unset)
        } else if sub.is_none() {
            ksh_first(&name, param_value(&name, None)?)
        } else {
            param_value(&name, sub.as_deref())?
        };
    } else if matches!(body.get(i), Some(':' | '-' | '+' | '=' | '?')) || i == body.len() {
        // ${:-word}, ${(%):-%x}: no parameter.
        val = Val::Unset;
    } else {
        return bad();
    }
    if flags.type_of {
        return Ok(Val::Str(type_of(&name)));
    }
    // Rule 5: in double quotes, without (@), an array joins into one word
    // before nested subscripts, operators, modifiers and sorting apply.
    // A nested (@) result stays an array for the subscripts.
    let dq_join = quoted
        && !flags.at
        && !length
        && flags.join.is_none()
        && flags.split.is_none()
        && !ifs_split;
    if dq_join && body.get(i) == Some(&'[') {
        if let Val::List(items, true) = &val {
            val = Val::Str(items.join(&ifs_first()));
        }
    }
    // Further subscripts apply to the value: ${${x}[2]}, ${a[2][1]}.
    while body.get(i) == Some(&'[') {
        let mut depth = 0;
        let mut k = i;
        while k < body.len() {
            match body[k] {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            k += 1;
        }
        if k >= body.len() {
            return bad();
        }
        let s: String = body[i + 1..k].iter().collect();
        val = apply_subscript(val, &s)?;
        i = k + 1;
    }

    let rest = &body[i..];
    let sorts = (flags.sort.is_some() || flags.unique || flags.numeric || flags.signed)
        && !flags.keys
        && !flags.values;
    if dq_join && (!rest.is_empty() || sorts) {
        if let Val::List(items, star) = &val {
            if nested || *star {
                val = Val::Str(items.join(&ifs_first()));
            }
        }
    }
    let mut val = if rest.is_empty() {
        if !name.is_empty() {
            check_set(&name, &val)?;
        }
        val
    } else if rest[0] == ':' && rest.get(1) == Some(&'^') {
        // ${a:^b} zips two arrays; ${a:^^b} repeats the shorter one.
        let long = rest.get(2) == Some(&'^');
        let other: String = rest[if long { 3 } else { 2 }..].iter().collect();
        let a = val_items(&plain(val));
        let b = val_items(&lookup(&other));
        let n = if long {
            if a.is_empty() || b.is_empty() {
                0
            } else {
                a.len().max(b.len())
            }
        } else {
            a.len().min(b.len())
        };
        let mut items = Vec::new();
        for k in 0..n {
            items.push(a[k % a.len()].clone());
            items.push(b[k % b.len()].clone());
        }
        Val::List(items, false)
    } else if rest[0] == ':' && rest.get(1) == Some(&'#') {
        // ${arr:#pattern}: drop elements that match.
        let pat = pattern_of(&rest[2..])?;
        let was_list = is_list(&val);
        let keep = flags.matching;
        let items: Vec<String> = val_items(&val)
            .into_iter()
            .filter(|s| crate::pattern::matches(&pat, s) == keep)
            .collect();
        if was_list {
            Val::List(items, false)
        } else {
            items.into_iter().next().map(Val::Str).unwrap_or(Val::Unset)
        }
    } else if let Some(v) = (rest[0] == ':')
        .then(|| apply_modifiers(plain_clone(&val), rest))
        .transpose()?
        .flatten()
    {
        v
    } else {
        apply_operator(val, &name, sub.as_deref(), rest, mode, &text, &flags)?
    };

    if ifs_split {
        flags.ifs_split = true;
        flags.any = true;
    }
    if flags.any {
        val = apply_flags(val, &flags, quoted)?;
    }
    if length {
        let v = plain(val);
        let n = if flags.count_chars {
            val_items(&v).join(" ").chars().count()
        } else if (flags.words || flags.words_empty) && flags.split.is_none() {
            let ifs = ifs();
            val_items(&v)
                .iter()
                .map(|s| {
                    let parts = s.split(|c| ifs.contains(c));
                    if flags.words_empty {
                        parts.count()
                    } else {
                        parts.filter(|w| !w.is_empty()).count()
                    }
                })
                .sum()
        } else {
            match &v {
                Val::Unset => 0,
                Val::Str(s) => s.chars().count(),
                other => val_items(other).len(),
            }
        };
        return Ok(Val::Str(n.to_string()));
    }
    if rc {
        val = Val::Rc(val_items(&plain(val)));
    }
    Ok(val)
}

fn plain_clone(v: &Val) -> Val {
    match v {
        Val::Unset => Val::Unset,
        Val::Str(s) => Val::Str(s.clone()),
        Val::List(items, star) => Val::List(items.clone(), *star),
        Val::Rc(items) => Val::Rc(items.clone()),
        Val::Tagged(f) => Val::Tagged(f.clone()),
    }
}

// The assignment of `${name=word}`; with (A) the word's fields become an
// array, with (AA) key/value pairs.
// The result is the parameter read back, so attributes such as -Z3
// show; `name[sub]` assigns an element or range, digits a positional
// parameter.
fn assign_word(
    name: &str,
    sub: Option<&str>,
    word: &[char],
    mode: Mode,
    pf: &PFlags,
    text: &str,
) -> ExpResult<Val> {
    let positional = name.parse::<usize>().ok().filter(|&n| n > 0);
    if !crate::lexer::is_valid_name(name) && positional.is_none() {
        return fail(&format!("${{{}}}: cannot assign in this way", text));
    }
    if let Some(n) = positional {
        let v = word_string(word, mode)?;
        with_shell(|s| {
            if s.pos_params.len() < n {
                s.pos_params.resize(n, String::new());
            }
            s.pos_params[n - 1] = v.clone();
        });
        return Ok(Val::Str(v));
    }
    if let Some(sub) = sub {
        let ok = if pf.assign_array > 0 {
            let items = val_items(&plain(word_val(word, mode)?));
            let range = expand_string(sub)?;
            let range = if range.contains(',') {
                range
            } else {
                format!("{},{}", range, range)
            };
            crate::shell::array_splice(name, &range, items)
        } else {
            let v = word_string(word, mode)?;
            crate::exec::set_subscripted(name, sub, &v, false)
                .map_err(|status| ExpError { status })?
        };
        if !ok {
            return Err(ExpError { status: 1 });
        }
        return param_value(name, Some(sub));
    }
    assign_whole(name, word, mode, pf)?;
    param_value(name, None)
}

fn assign_whole(name: &str, word: &[char], mode: Mode, pf: &PFlags) -> ExpResult<Val> {
    if pf.assign_array > 0 {
        let items = match word_val(word, mode)? {
            Val::Tagged(fields) => {
                let mut out = Vec::new();
                for f in fields {
                    for piece in split_field(&f, &ifs()) {
                        out.push(tagged_text(&piece));
                    }
                }
                out
            }
            other => val_items(&other),
        };
        if pf.assign_array > 1 {
            crate::shell::assoc_declare(name);
        }
        if !crate::shell::array_set(name, items.clone()) {
            return Err(ExpError { status: 1 });
        }
        return Ok(Val::List(items, false));
    }
    let v = word_string(word, mode)?;
    if !var_set(name, &v, false) {
        return Err(ExpError { status: 1 });
    }
    Ok(Val::Str(v))
}

// The POSIX/bash operators after the name: - = ? + # % / : ^ ,
fn apply_operator(
    val: Val,
    name: &str,
    sub: Option<&str>,
    rest: &[char],
    mode: Mode,
    text: &str,
    pf: &PFlags,
) -> ExpResult<Val> {
    let bad = || fail::<Val>(&format!("${{{}}}: bad substitution", text));
    let is_set = !matches!(val, Val::Unset);
    let is_null = match &val {
        Val::Unset => true,
        Val::Str(s) => s.is_empty(),
        Val::List(items, _) | Val::Rc(items) => items.iter().all(|s| s.is_empty()),
        Val::Tagged(f) => f.iter().all(|x| tagged_text(x).is_empty()),
    };
    // ${name::=word}: assign unconditionally.
    if rest.starts_with(&[':', ':', '=']) {
        return assign_word(name, sub, &rest[3..], mode, pf, text);
    }
    // ${a:|b} removes b's elements from a; ${a:*b} keeps only those.
    if rest.len() > 1 && rest[0] == ':' && matches!(rest[1], '|' | '*') {
        let other: String = rest[2..].iter().collect();
        let others = val_items(&lookup(&other));
        let keep = rest[1] == '*';
        let was_list = is_list(&val);
        let items: Vec<String> = val_items(&plain(val))
            .into_iter()
            .filter(|x| others.contains(x) == keep)
            .collect();
        return Ok(if was_list {
            Val::List(items, false)
        } else {
            items
                .into_iter()
                .next()
                .map(Val::Str)
                .unwrap_or(Val::Str(String::new()))
        });
    }
    let (colon, op, word) =
        if rest[0] == ':' && rest.len() > 1 && matches!(rest[1], '-' | '=' | '?' | '+' | '/') {
            (true, rest[1], &rest[2..])
        } else {
            (false, rest[0], &rest[1..])
        };
    let missing = if colon { !is_set || is_null } else { !is_set };
    match op {
        '-' => {
            if missing {
                word_val(word, mode)
            } else {
                Ok(val)
            }
        }
        '+' => {
            if missing {
                Ok(Val::Str(String::new()))
            } else {
                word_val(word, mode)
            }
        }
        '=' => {
            if !missing {
                return Ok(val);
            }
            assign_word(name, sub, word, mode, pf, text)
        }
        '?' => {
            if !missing {
                return Ok(val);
            }
            let msg = word_string(word, mode)?;
            let msg = if msg.is_empty() {
                "parameter null or not set".to_string()
            } else {
                msg
            };
            fail(&format!("{}: {}", name, msg))
        }
        '#' | '%' | '/' => {
            check_set(name, &val)?;
            let mut fl = crate::pattern::SubFlags {
                substr: pf.substr,
                matched: pf.matching,
                rest: pf.rest_part,
                bind: pf.bind,
                eind: pf.eind,
                len: pf.mlen,
                ..Default::default()
            };
            let mut w = word;
            let mut repl = None;
            if op == '/' {
                fl.long = !pf.substr;
                fl.substr = false;
                if w.first() == Some(&'/') {
                    fl.global = true;
                    w = &w[1..];
                }
                if w.first() == Some(&'#') {
                    fl.start = true;
                    w = &w[1..];
                }
                if w.first() == Some(&'%') {
                    fl.end = true;
                    w = &w[1..];
                }
                if !(fl.start || fl.end) {
                    fl.substr = true;
                }
                fl.all = colon;
                let (pat_part, repl_part) = match find_unquoted(w, '/') {
                    Some(k) => (&w[..k], &w[k + 1..]),
                    None => (w, &w[w.len()..]),
                };
                w = pat_part;
                repl = Some(repl_part);
            } else {
                fl.end = op == '%';
                if w.first() == Some(&op) {
                    fl.long = true;
                    w = &w[1..];
                }
            }
            let n = match &pf.nth {
                Some(e) => eval_arith_str(e)?.max(1) as usize,
                None => 1,
            };
            let pat = crate::pattern::Pattern::new(&pattern_of(w)?);
            // Expanded per match, after $MATCH and $match are set.
            let err = std::cell::Cell::new(None);
            let expand_repl = |r: &[char]| match word_string(r, mode) {
                Ok(v) => v,
                Err(e) => {
                    err.set(Some(e));
                    String::new()
                }
            };
            let f = repl.map(|r| move || expand_repl(r));
            let f = f.as_ref().map(|f| f as &dyn Fn() -> String);
            let out = map_items(val, |s| pat.getmatch(s, fl, n, f));
            match err.take() {
                Some(e) => Err(e),
                None => Ok(out),
            }
        }
        ':' => {
            check_set(name, &val)?;
            let (off_part, len_part) = match find_unquoted(word, ':') {
                Some(k) => (&word[..k], Some(&word[k + 1..])),
                None => (word, None),
            };
            let off = eval_arith_str(&word_string(off_part, Mode::Unquoted)?)?;
            let len = match len_part {
                Some(l) => Some(eval_arith_str(&word_string(l, Mode::Unquoted)?)?),
                None => None,
            };
            Ok(match plain(val) {
                Val::List(items, star) => {
                    let items = if name == "@" || name == "*" {
                        let mut all = vec![var_get("0")];
                        all.extend(items);
                        all
                    } else {
                        items
                    };
                    Val::List(slice(&items, off, len), star)
                }
                other => {
                    let chars: Vec<String> = val_items(&other)
                        .concat()
                        .chars()
                        .map(|c| c.to_string())
                        .collect();
                    Val::Str(slice(&chars, off, len).concat())
                }
            })
        }
        '^' | ',' => {
            check_set(name, &val)?;
            let all = word.first() == Some(&op);
            let upper = op == '^';
            Ok(map_items(val, |s| {
                if all {
                    if upper {
                        s.to_uppercase()
                    } else {
                        s.to_lowercase()
                    }
                } else {
                    let mut cs = s.chars();
                    match cs.next() {
                        Some(c) => {
                            let first: String = if upper {
                                c.to_uppercase().collect()
                            } else {
                                c.to_lowercase().collect()
                            };
                            first + cs.as_str()
                        }
                        None => String::new(),
                    }
                }
            }))
        }
        _ => bad(),
    }
}

// The `${=...}`/`${==...}` and `${~...}`/`${~~...}` prefixes: whether
// to force word splitting and pattern characters on or off.
fn prefix_toggles(body: &[char]) -> (Option<bool>, Option<bool>) {
    let mut i = 0;
    if body.first() == Some(&'(') {
        match parse_flags(body) {
            Some((_, end)) => i = end,
            None => return (None, None),
        }
    }
    let (mut split, mut glob) = (None, None);
    while let Some(&c) = body.get(i) {
        let doubled = body.get(i + 1) == Some(&c);
        match c {
            '=' => split = Some(!doubled),
            '~' => glob = Some(!doubled),
            '#' | '+' | '^' | '!' => {}
            _ => break,
        }
        i += if doubled { 2 } else { 1 };
    }
    (split, glob)
}

fn expand_braced(body: &[char], mode: Mode, ctx: &mut Ctx) -> ExpResult<()> {
    let val = braced(body, mode)?;
    let quoted = mode != Mode::Unquoted;
    let (split, glob) = prefix_toggles(body);
    if quoted || (split.is_none() && glob.is_none()) {
        emit_val(ctx, &val, quoted);
        return Ok(());
    }
    let saved = ctx.param_tag;
    if split == Some(false) {
        ctx.param_tag &= !S;
    }
    match glob {
        Some(true) => ctx.param_tag &= !NG,
        Some(false) => ctx.param_tag |= NG,
        None => {}
    }
    let val = match val {
        // ${~x-"word"}: even quoted parts of the word become patterns.
        Val::Tagged(fields) if glob == Some(true) => Val::Tagged(
            fields
                .into_iter()
                .map(|f| {
                    f.into_iter()
                        .map(|(c, t)| (c, if t == Q { 0 } else { t & !NG }))
                        .collect()
                })
                .collect(),
        ),
        v => v,
    };
    emit_val(ctx, &val, quoted);
    ctx.param_tag = saved;
    Ok(())
}

fn slice<T: Clone>(items: &[T], off: i64, len: Option<i64>) -> Vec<T> {
    let n = items.len() as i64;
    let start = if off < 0 {
        (n + off).max(0)
    } else {
        off.min(n)
    };
    let end = match len {
        Some(l) if l < 0 => (n + l).max(start),
        Some(l) => (start + l).min(n),
        None => n,
    };
    items[start as usize..end as usize].to_vec()
}

// Index of the first `target` not inside quotes or escaped.
fn find_unquoted(chars: &[char], target: char) -> Option<usize> {
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '\'' => i = scan_squote(chars, i)?,
            '"' => i = scan_dquote(chars, i)?,
            '$' if chars.get(i + 1) == Some(&'{') => i = scan_brace(chars, i + 1, false)?,
            '$' if chars.get(i + 1) == Some(&'(') => i = scan_paren(chars, i + 1)?,
            c if c == target => return Some(i),
            _ => i += 1,
        }
    }
    None
}

// Expands a word to a single string (no splitting, no globbing).
fn word_string(word: &[char], mode: Mode) -> ExpResult<String> {
    let mut ctx = Ctx::new(false, false);
    let m = if mode == Mode::Heredoc {
        Mode::Dq
    } else {
        mode
    };
    walk(word, m, &mut ctx)?;
    Ok(ctx.joined())
}

// Expands a word into a pattern for `crate::pattern`: quoted characters
// are backslash-escaped so they match literally.
fn pattern_of(word: &[char]) -> ExpResult<String> {
    let mut ctx = Ctx::new(false, false);
    walk(word, Mode::Unquoted, &mut ctx)?;
    Ok(pattern_text(&ctx))
}

fn pattern_text(ctx: &Ctx) -> String {
    let mut out = String::new();
    for &(c, f) in ctx.fields.iter().flatten() {
        if f & MARK != 0 {
            continue;
        }
        if f & (Q | NG) != 0 && "*?[]()|<>^#~\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn ansi_c(chars: &[char]) -> String {
    let mut out = String::new();
    let mut i = 0;
    let n = chars.len();
    let take_digits = |i: &mut usize, radix: u32, max: usize| -> Option<u32> {
        let start = *i;
        while *i < n && *i - start < max && chars[*i].is_digit(radix) {
            *i += 1;
        }
        if *i == start {
            return None;
        }
        let s: String = chars[start..*i].iter().collect();
        u32::from_str_radix(&s, radix).ok()
    };
    while i < n {
        if chars[i] != '\\' || i + 1 >= n {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        i += 1;
        let c = chars[i];
        i += 1;
        match c {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'a' => out.push('\x07'),
            'b' => out.push('\x08'),
            'e' | 'E' => out.push('\x1b'),
            'f' => out.push('\x0c'),
            'v' => out.push('\x0b'),
            '\\' | '\'' | '"' | '?' => out.push(c),
            '0'..='7' => {
                i -= 1;
                if let Some(v) = take_digits(&mut i, 8, 3) {
                    out.push(char::from_u32(v & 0xff).unwrap_or('?'));
                }
            }
            'x' => match take_digits(&mut i, 16, 2) {
                Some(v) => out.push(char::from_u32(v).unwrap_or('?')),
                None => out.push_str("\\x"),
            },
            'u' | 'U' => {
                let max = if c == 'u' { 4 } else { 8 };
                match take_digits(&mut i, 16, max) {
                    Some(v) => out.push(char::from_u32(v).unwrap_or('\u{fffd}')),
                    None => {
                        out.push('\\');
                        out.push(c);
                    }
                }
            }
            'c' if i < n => {
                out.push(char::from_u32((chars[i] as u32) & 0x1f).unwrap_or('?'));
                i += 1;
            }
            _ => {
                out.push('\\');
                out.push(c);
            }
        }
    }
    out
}

/// Decodes backslash escapes the way `$'...'` does (used by `echo -e`).
pub fn decode_escapes(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    ansi_c(&chars)
}

// ---- brace expansion ------------------------------------------------------

// zsh brace-expands after parameter expansion, so `IF{${^arr},x}` works;
// only braces and commas from unquoted literal text count.
fn brace_expand(field: &[(char, u8)]) -> Vec<Vec<(char, u8)>> {
    let active = |i: usize| field[i].1 & B != 0;
    for i in 0..field.len() {
        if field[i].0 != '{' || !active(i) {
            continue;
        }
        let Some((close, commas)) = brace_close(field, i) else {
            continue;
        };
        let mut alts: Vec<Vec<(char, u8)>> = Vec::new();
        if !commas.is_empty() {
            let mut last = i + 1;
            for &c in commas.iter().chain(std::iter::once(&close)) {
                alts.push(field[last..c].to_vec());
                last = c + 1;
            }
        } else {
            let inner: Vec<char> = field[i + 1..close].iter().map(|&(c, _)| c).collect();
            if let Some(seq) = sequence(&inner) {
                alts = seq
                    .into_iter()
                    .map(|w| w.chars().map(|c| (c, field[i].1)).collect())
                    .collect();
            }
        }
        if alts.is_empty() {
            continue;
        }
        let mut out = Vec::new();
        for alt in alts {
            let mut w = field[..i].to_vec();
            w.extend(alt);
            w.extend_from_slice(&field[close + 1..]);
            out.extend(brace_expand(&w));
        }
        return out;
    }
    vec![field.to_vec()]
}

// Finds the `}` matching the `{` at `open` and the top-level commas.
fn brace_close(field: &[(char, u8)], open: usize) -> Option<(usize, Vec<usize>)> {
    let mut depth = 0;
    let mut commas = Vec::new();
    for (i, &(c, t)) in field.iter().enumerate().skip(open) {
        if t & B == 0 {
            continue;
        }
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((i, commas));
                }
            }
            ',' if depth == 1 => commas.push(i),
            _ => {}
        }
    }
    None
}

// `{a..b}` and `{a..b..step}` over integers or single characters.
fn sequence(inner: &[char]) -> Option<Vec<String>> {
    let s: String = inner.iter().collect();
    let parts: Vec<&str> = s.split("..").collect();
    if parts.len() != 2 && parts.len() != 3 {
        return None;
    }
    let step: i64 = match parts.get(2) {
        Some(p) => p.parse().ok()?,
        None => 1,
    };
    let step = step.unsigned_abs().max(1) as i64;
    if let (Ok(a), Ok(b)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) {
        let width = if (parts[0].starts_with('0') && parts[0].len() > 1)
            || (parts[1].starts_with('0') && parts[1].len() > 1)
        {
            parts[0].len().max(parts[1].len())
        } else {
            0
        };
        if (a - b).unsigned_abs() / step as u64 > 100_000 {
            return None;
        }
        let mut out = Vec::new();
        let mut x = a;
        loop {
            out.push(format!("{:0width$}", x, width = width));
            if x == b {
                break;
            }
            if a < b {
                x += step;
                if x > b {
                    break;
                }
            } else {
                x -= step;
                if x < b {
                    break;
                }
            }
        }
        return Some(out);
    }
    let (ca, cb): (Vec<char>, Vec<char>) = (parts[0].chars().collect(), parts[1].chars().collect());
    if ca.len() == 1 && cb.len() == 1 && ca[0].is_ascii_alphabetic() && cb[0].is_ascii_alphabetic()
    {
        let (a, b) = (ca[0] as i64, cb[0] as i64);
        let mut out = Vec::new();
        let mut x = a;
        loop {
            out.push(char::from_u32(x as u32)?.to_string());
            if x == b {
                break;
            }
            if a < b {
                x += step;
                if x > b {
                    break;
                }
            } else {
                x -= step;
                if x < b {
                    break;
                }
            }
        }
        return Some(out);
    }
    None
}

// ---- field splitting and pathname expansion -------------------------------

fn split_field(field: &[(char, u8)], ifs: &str) -> Vec<Vec<(char, u8)>> {
    let is_delim = |&(c, f): &(char, u8)| f & S != 0 && ifs.contains(c);
    let is_ws = |c: char| matches!(c, ' ' | '\t' | '\n');
    let n = field.len();
    let mut out = Vec::new();
    let mut cur: Vec<(char, u8)> = Vec::new();
    let mut i = 0;
    while i < n && is_delim(&field[i]) && is_ws(field[i].0) {
        i += 1;
    }
    while i < n {
        if !is_delim(&field[i]) {
            cur.push(field[i]);
            i += 1;
            continue;
        }
        if is_ws(field[i].0) {
            let mut j = i;
            while j < n && is_delim(&field[j]) && is_ws(field[j].0) {
                j += 1;
            }
            if j < n && is_delim(&field[j]) {
                // Whitespace before a non-whitespace delimiter.
                i = j;
                continue;
            }
            if j == n {
                break;
            }
            out.push(std::mem::take(&mut cur));
            i = j;
        } else {
            out.push(std::mem::take(&mut cur));
            i += 1;
            while i < n && is_delim(&field[i]) && is_ws(field[i].0) {
                i += 1;
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

enum Globbed {
    // Not a pattern, or globbing is off.
    Word(String),
    Matches(Vec<String>),
    // A pattern that matched nothing, with its literal text.
    NoMatch(String),
}

fn glob_piece(piece: &[(char, u8)], do_glob: bool) -> Globbed {
    let literal: String = piece
        .iter()
        .filter(|(_, f)| f & MARK == 0)
        .map(|(c, _)| *c)
        .collect();
    if !do_glob {
        return Globbed::Word(literal);
    }
    // Quoted characters are escaped so they match literally.
    let mut pat = String::new();
    for &(c, f) in piece {
        if f & MARK != 0 {
            continue;
        }
        if (f & (Q | NG) != 0 && "*?[]()|<>^#~\\".contains(c)) || (c == '\\' && f & (Q | NG) == 0) {
            pat.push('\\');
        }
        pat.push(c);
    }
    let (base, quals) = crate::globber::split_qualifiers(&pat);
    if quals.is_none() && !crate::pattern::has_meta(base) {
        return Globbed::Word(literal);
    }
    let mut quals = quals.unwrap_or_default();
    let explicit_null = quals.null;
    if crate::options::is_set("globdots") {
        quals.dots = true;
    }
    let mut matches = crate::globber::glob(base, Some(&quals));
    if matches.is_empty() {
        return if explicit_null {
            Globbed::Matches(Vec::new())
        } else {
            Globbed::NoMatch(literal)
        };
    }
    if crate::options::is_set("markdirs") {
        for m in matches.iter_mut() {
            if !m.ends_with('/') && std::path::Path::new(m).is_dir() {
                m.push('/');
            }
        }
    }
    Globbed::Matches(matches)
}

// Globs every word of a command and applies NULL_GLOB, CSH_NULL_GLOB and
// NOMATCH to patterns that matched nothing.
fn finish_all(ctxs: Vec<Ctx>, do_glob: bool) -> ExpResult<Vec<String>> {
    let ifs = ifs();
    let (null, csh_null, nomatch) = with_shell(|s| {
        let on = |o| crate::options::is_set_in(s, o);
        (on("nullglob"), on("cshnullglob"), on("nomatch"))
    });
    let mut out = Vec::new();
    let (mut any_match, mut failed) = (false, None::<String>);
    for ctx in &ctxs {
        for field in &ctx.fields {
            for piece in split_field(field, &ifs) {
                match glob_piece(&piece, do_glob) {
                    Globbed::Word(w) => out.push(w),
                    Globbed::Matches(m) => {
                        any_match = true;
                        out.extend(m);
                    }
                    Globbed::NoMatch(lit) => {
                        if null {
                        } else if csh_null {
                            failed.get_or_insert(lit);
                        } else if nomatch {
                            return fail(&format!("no matches found: {}", lit));
                        } else {
                            out.push(lit);
                        }
                    }
                }
            }
        }
    }
    if csh_null && !any_match && failed.is_some() {
        return fail("no match");
    }
    Ok(out)
}

// ---- public entry points --------------------------------------------------

/// Fully expands command words into fields.
pub fn expand_words(words: &[String]) -> ExpResult<Vec<String>> {
    let do_glob = !opt_set(OPT_NOGLOB);
    let mut ctxs = Vec::new();
    let braces = !crate::options::is_set("ignorebraces");
    for word in words {
        let chars: Vec<char> = word.chars().collect();
        let mut ctx = Ctx::new(true, false);
        walk(&chars, Mode::Unquoted, &mut ctx)?;
        if braces {
            ctx.fields = ctx.fields.iter().flat_map(|f| brace_expand(f)).collect();
        }
        ctxs.push(ctx);
    }
    finish_all(ctxs, do_glob)
}

/// Expands a word to one string without field splitting or globbing
/// (redirection targets, `case` words, `[[ ]]` operands).
pub fn expand_string(word: &str) -> ExpResult<String> {
    let chars: Vec<char> = word.chars().collect();
    let mut ctx = Ctx::new(false, false);
    walk(&chars, Mode::Unquoted, &mut ctx)?;
    Ok(ctx.joined())
}

/// Expands the value of an assignment (`name=value`).
pub fn expand_assign_value(word: &str) -> ExpResult<String> {
    let chars: Vec<char> = word.chars().collect();
    let mut ctx = Ctx::new(false, true);
    walk(&chars, Mode::Unquoted, &mut ctx)?;
    Ok(ctx.joined())
}

/// Expands a word used as a pattern; quoted characters match literally.
pub fn expand_pattern(word: &str) -> ExpResult<String> {
    let chars: Vec<char> = word.chars().collect();
    pattern_of(&chars)
}

/// Expands the body of an unquoted here-document.
pub fn expand_heredoc(body: &str) -> ExpResult<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut ctx = Ctx::new(false, false);
    walk(&chars, Mode::Heredoc, &mut ctx)?;
    Ok(ctx.joined())
}

/// Expands parameters, command substitutions and arithmetic in a prompt.
pub fn expand_prompt_string(s: &str) -> ExpResult<String> {
    expand_heredoc(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{shell_init, var_set};
    use crate::TEST_LOCK;

    // These tests check POSIX expansion, so they run in sh emulation.
    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shell_init();
        with_shell(|s| crate::options::init(s, crate::options::Emulation::Sh));
        lock
    }

    #[test]
    fn zsh_defaults_neither_split_nor_glob_parameters() {
        let _g = setup();
        with_shell(|s| crate::options::init(s, crate::options::Emulation::Zsh));
        var_set("x", "a b *", false);
        assert_eq!(words(&["$x"]), vec!["a b *"]);
        assert_eq!(words(&["$(echo c d)"]), vec!["c", "d"]);
        assert_eq!(words(&["${=x}"]), vec!["a", "b", "*"]);
        assert_eq!(words(&["${unset_v:-a b}"]), vec!["a b"]);
        assert!(expand_words(&["/nonexistent-dir/*".to_string()]).is_err());
        crate::options::set("nullglob", true);
        assert!(words(&["/nonexistent-dir/*"]).is_empty());
    }

    fn words(s: &[&str]) -> Vec<String> {
        expand_words(&s.iter().map(|w| w.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn field_splitting_uses_ifs() {
        let _l = setup();
        var_set("x", " a  b ", false);
        assert_eq!(words(&["$x"]), vec!["a", "b"]);
        assert_eq!(words(&["\"$x\""]), vec![" a  b "]);
        var_set("IFS", ":", false);
        var_set("y", "a::b:", false);
        assert_eq!(words(&["$y"]), vec!["a", "", "b"]);
        var_set("IFS", " \t\n", false);
    }

    #[test]
    fn quoted_empty_and_unset() {
        let _l = setup();
        var_set("e", "", false);
        assert_eq!(words(&["$e"]), Vec::<String>::new());
        assert_eq!(words(&["\"$e\""]), vec![""]);
        assert_eq!(words(&["''"]), vec![""]);
    }

    #[test]
    fn positional_at_and_star() {
        let _l = setup();
        with_shell(|s| s.pos_params = vec!["a b".into(), "".into(), "c".into()]);
        assert_eq!(words(&["\"$@\""]), vec!["a b", "", "c"]);
        assert_eq!(words(&["\"$*\""]), vec!["a b  c"]);
        assert_eq!(words(&["$@"]), vec!["a", "b", "c"]);
        assert_eq!(words(&["x\"$@\"y"]), vec!["xa b", "", "cy"]);
        with_shell(|s| s.pos_params.clear());
        assert_eq!(words(&["\"$@\""]), Vec::<String>::new());
    }

    #[test]
    fn quotes_suppress_glob_and_tilde() {
        let _l = setup();
        var_set("HOME", "/home/u", false);
        assert_eq!(words(&["'*'"]), vec!["*"]);
        assert_eq!(words(&["\"~\""]), vec!["~"]);
        assert_eq!(words(&["~/x"]), vec!["/home/u/x"]);
        assert_eq!(expand_assign_value("a:~/b").unwrap(), "a:/home/u/b");
    }

    #[test]
    fn parameter_operators() {
        let _l = setup();
        var_set("p", "/usr/local/bin", false);
        assert_eq!(expand_string("${p##*/}").unwrap(), "bin");
        assert_eq!(expand_string("${p%/*}").unwrap(), "/usr/local");
        assert_eq!(expand_string("${p#\"/usr\"}").unwrap(), "/local/bin");
        assert_eq!(expand_string("${unset_v-def}").unwrap(), "def");
        assert_eq!(expand_string("${p:+alt}").unwrap(), "alt");
        assert_eq!(expand_string("${#p}").unwrap(), "14");
        assert_eq!(expand_string("${p:1:3}").unwrap(), "usr");
        assert_eq!(words(&["${unset_v:-\"a b\"}"]), vec!["a b"]);
        assert_eq!(words(&["${unset_v:-a b}"]), vec!["a", "b"]);
    }

    #[test]
    fn arithmetic_and_brace() {
        let _l = setup();
        assert_eq!(expand_string("$((1 + 2 * 3))").unwrap(), "7");
        assert_eq!(words(&["{a,b}{1..2}"]), vec!["a1", "a2", "b1", "b2"]);
        assert_eq!(words(&["'{a,b}'"]), vec!["{a,b}"]);
        assert_eq!(words(&["{a}"]), vec!["{a}"]);
    }

    #[test]
    fn non_ascii_words() {
        let _l = setup();
        var_set("v", "héllo wörld", false);
        assert_eq!(words(&["$v"]), vec!["héllo", "wörld"]);
        assert_eq!(expand_string("${v%wörld}").unwrap(), "héllo ");
    }

    #[test]
    fn heredoc_keeps_quotes() {
        let _l = setup();
        var_set("x", "v", false);
        assert_eq!(expand_heredoc("'$x' \"\\$x\"\n").unwrap(), "'v' \"$x\"\n");
    }
}
