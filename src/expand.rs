//! Word expansion: brace, tilde, parameter, command substitution,
//! arithmetic, field splitting, pathname expansion and quote removal.
//!
//! Words arrive raw from the parser, quotes included. Expansion walks a
//! word once and produces characters tagged with how they were produced,
//! so later stages know which characters may be split on or globbed.

use crate::lexer::{
    scan_bquote, scan_brace, scan_dollar_squote, scan_dquote, scan_paren, scan_squote,
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
    eprintln!("meowsh: {}", msg);
    Err(ExpError { status: 1 })
}

// Character tags.
const Q: u8 = 1; // quoted: never split or globbed
const S: u8 = 2; // result of an unquoted expansion: subject to field splitting
const MARK: u8 = 4; // zero-width marker: the field exists even if empty

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
        }
    }

    fn push(&mut self, c: char, flag: u8) {
        self.fields.last_mut().unwrap().push((c, flag));
        self.content += 1;
    }

    fn mark(&mut self) {
        self.push('\0', MARK);
    }

    fn new_field(&mut self) {
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
}

impl Val {
    fn joined(&self) -> String {
        match self {
            Val::Unset => String::new(),
            Val::Str(s) => s.clone(),
            Val::List(items, star) => {
                if *star {
                    items.join(&ifs_first())
                } else {
                    items.join(" ")
                }
            }
        }
    }
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
    if let Some(items) = with_shell(|s| s.arrays.get(name).cloned()) {
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

fn param_value(name: &str, sub: Option<&str>) -> ExpResult<Val> {
    let Some(sub) = sub else {
        return Ok(lookup(name));
    };
    let is_assoc = with_shell(|s| s.assoc_arrays.contains_key(name));
    if sub == "@" || sub == "*" {
        let star = sub == "*";
        let items = if is_assoc {
            assoc_values(name).unwrap_or_default()
        } else if let Some(items) = with_shell(|s| s.arrays.get(name).cloned()) {
            items
        } else {
            match var_get_opt(name) {
                Some(v) => vec![v],
                None => Vec::new(),
            }
        };
        return Ok(Val::List(items, star));
    }
    if let Some(v) = subscript_search(name, sub, is_assoc)? {
        return Ok(v);
    }
    if is_assoc {
        let key = expand_string(sub)?;
        return Ok(match assoc_get(name, &key) {
            Some(v) => Val::Str(v),
            None => Val::Unset,
        });
    }
    let idx = eval_arith_str(&expand_string(sub)?)?;
    if let Some(items) = with_shell(|s| s.arrays.get(name).cloned()) {
        let len = items.len() as i64;
        let pos = if idx > 0 {
            idx - 1
        } else if idx < 0 {
            len + idx
        } else {
            return Ok(Val::Unset);
        };
        if pos < 0 || pos >= len {
            return Ok(Val::Unset);
        }
        return Ok(Val::Str(items[pos as usize].clone()));
    }
    match var_get_opt(name) {
        Some(v) if idx == 0 || idx == 1 => Ok(Val::Str(v)),
        _ => Ok(Val::Unset),
    }
}

/// zsh subscript flags: `(i)`/`(I)` give the first/last index (keys for
/// associative arrays) whose element matches the pattern, `(r)`/`(R)` the
/// first/last matching value. `(e)` matches the text literally. Returns
/// None when `sub` has no such flags.
fn subscript_search(name: &str, sub: &str, is_assoc: bool) -> ExpResult<Option<Val>> {
    let Some(rest) = sub.strip_prefix('(') else {
        return Ok(None);
    };
    let Some(close) = rest.find(')') else {
        return Ok(None);
    };
    let flags = &rest[..close];
    if flags.is_empty() || !flags.chars().all(|c| "iIrRe".contains(c)) {
        return Ok(None);
    }
    let word = &rest[close + 1..];
    let exact = flags.contains('e');
    let pat = if exact {
        expand_string(word)?
    } else {
        expand_pattern(word)?
    };
    let hit = |v: &str| {
        if exact {
            v == pat
        } else {
            crate::pattern::matches(&pat, v)
        }
    };
    let last = flags.contains('I') || flags.contains('R');
    let want_value = flags.contains('r') || flags.contains('R');
    if is_assoc {
        let keys = assoc_keys(name).unwrap_or_default();
        let mut found = keys.iter().filter_map(|k| {
            let v = assoc_get(name, k).unwrap_or_default();
            if want_value {
                hit(&v).then_some(v)
            } else {
                hit(k).then(|| k.clone())
            }
        });
        let r = if last {
            found.next_back()
        } else {
            found.next()
        };
        return Ok(Some(r.map(Val::Str).unwrap_or(Val::Unset)));
    }
    let items = with_shell(|s| s.arrays.get(name).cloned())
        .or_else(|| var_get_opt(name).map(|v| vec![v]))
        .unwrap_or_default();
    let mut idx = items
        .iter()
        .enumerate()
        .filter(|(_, v)| hit(v))
        .map(|(k, _)| k);
    let found = if last { idx.next_back() } else { idx.next() };
    Ok(Some(match (found, want_value) {
        (Some(k), true) => Val::Str(items[k].clone()),
        (None, true) => Val::Unset,
        (Some(k), false) => Val::Str((k + 1).to_string()),
        (None, false) => Val::Str(if last { 0 } else { items.len() + 1 }.to_string()),
    }))
}

fn check_set(name: &str, val: &Val) -> ExpResult<()> {
    if matches!(val, Val::Unset) && name != "@" && name != "*" && opt_set(OPT_NOUNSET) {
        return fail(&format!("{}: unbound variable", name));
    }
    Ok(())
}

fn emit_str(ctx: &mut Ctx, s: &str, quoted: bool) {
    let flag = if quoted { Q } else { S };
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
            for c in item.chars() {
                ctx.push(c, S);
            }
        }
    }
}

fn emit_val(ctx: &mut Ctx, val: &Val, quoted: bool) {
    match val {
        Val::Unset => emit_str(ctx, "", quoted),
        Val::Str(s) => emit_str(ctx, s, quoted),
        Val::List(items, star) => emit_list(ctx, items, *star, quoted),
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
                emit_str(ctx, &out, mode != Mode::Unquoted);
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
                ctx.push(c, lit);
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
        Err(e) => fail(&format!("{}: {}", expr.trim(), e)),
    }
}

fn arith_num(expr: &[char]) -> ExpResult<crate::arith::Num> {
    let mut ctx = Ctx::new(false, false);
    walk(expr, Mode::Dq, &mut ctx)?;
    let text = ctx.joined();
    crate::arith::eval_num(&text).or_else(|e| fail(&format!("{}: {}", text.trim(), e)))
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
            let Some(end) = scan_paren(chars, i + 1) else {
                return fail("unterminated command substitution");
            };
            let is_arith = chars.get(i + 2) == Some(&'(')
                && end >= i + 5
                && chars[end - 2] == ')'
                && scan_paren(chars, i + 2) == Some(end - 1);
            if is_arith {
                let v = arith_num(&chars[i + 3..end - 2])?;
                emit_str(ctx, &v.to_string(), quoted);
            } else {
                let src: String = chars[i + 2..end - 1].iter().collect();
                let out = command_subst(&src)?;
                emit_str(ctx, &out, quoted);
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
        c if c == '_' || c.is_ascii_alphabetic() => {
            let mut j = i + 1;
            while j < n && (chars[j] == '_' || chars[j].is_ascii_alphanumeric()) {
                j += 1;
            }
            let name: String = chars[i + 1..j].iter().collect();
            let val = lookup(&name);
            check_set(&name, &val)?;
            emit_val(ctx, &val, quoted);
            Ok(j)
        }
        c if is_special_param(c) => {
            let name = c.to_string();
            let val = lookup(&name);
            check_set(&name, &val)?;
            emit_val(ctx, &val, quoted);
            Ok(i + 2)
        }
        _ => {
            ctx.push('$', lit);
            Ok(i + 1)
        }
    }
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

fn expand_braced(body: &[char], mode: Mode, ctx: &mut Ctx) -> ExpResult<()> {
    let quoted = mode != Mode::Unquoted;
    let text: String = body.iter().collect();
    let bad = || fail::<()>(&format!("${{{}}}: bad substitution", text));
    if body.is_empty() {
        return bad();
    }

    // ${#name}: length.
    if body[0] == '#' && body.len() > 1 {
        if let Some((name, sub, end)) = parse_param(&body[1..]) {
            if end + 1 == body.len() {
                let val = param_value(&name, sub.as_deref())?;
                check_set(&name, &val)?;
                let len = match &val {
                    Val::Unset => 0,
                    Val::Str(s) => s.chars().count(),
                    Val::List(items, _) => items.len(),
                };
                emit_str(ctx, &len.to_string(), quoted);
                return Ok(());
            }
        }
    }

    // ${+name}: 1 if set (zsh).
    if body[0] == '+' {
        let Some((name, sub, end)) = parse_param(&body[1..]) else {
            return bad();
        };
        if end + 1 != body.len() {
            return bad();
        }
        let set = !matches!(param_value(&name, sub.as_deref())?, Val::Unset);
        emit_str(ctx, if set { "1" } else { "0" }, quoted);
        return Ok(());
    }

    // ${(flags)name} (zsh): k = keys, v = values, @ = keep elements apart.
    let mut start = 0;
    let mut flags = String::new();
    if body[0] == '(' {
        let Some(close) = body.iter().position(|&c| c == ')') else {
            return bad();
        };
        flags = body[1..close].iter().collect();
        start = close + 1;
    }

    // ${!name}: indirection.
    let mut indirect = false;
    if body.get(start) == Some(&'!') && body.len() > start + 1 {
        indirect = true;
        start += 1;
    }

    let Some((mut name, sub, end)) = parse_param(&body[start..]) else {
        return bad();
    };
    let rest = &body[start + end..];

    if indirect {
        let target = var_get(&name);
        if target.is_empty() {
            emit_str(ctx, "", quoted);
            return Ok(());
        }
        name = target;
    }

    let mut val = if indirect {
        let target_chars: Vec<char> = name.chars().collect();
        match parse_param(&target_chars) {
            Some((n2, s2, e2)) if e2 == target_chars.len() => {
                name = n2;
                param_value(&name, s2.as_deref())?
            }
            _ => return fail(&format!("{}: bad substitution", name)),
        }
    } else {
        param_value(&name, sub.as_deref())?
    };

    if flags.contains('k') {
        if let Some(keys) = assoc_keys(&name) {
            val = Val::List(keys, true);
        }
    }
    if flags.contains('@') {
        if let Val::List(items, _) = val {
            val = Val::List(items, false);
        }
    }

    if rest.is_empty() {
        check_set(&name, &val)?;
        emit_val(ctx, &val, quoted);
        return Ok(());
    }

    let is_set = !matches!(val, Val::Unset);
    let is_null = val.joined().is_empty();

    let (colon, op, word) =
        if rest[0] == ':' && rest.len() > 1 && matches!(rest[1], '-' | '=' | '?' | '+') {
            (true, rest[1], &rest[2..])
        } else {
            (false, rest[0], &rest[1..])
        };

    match op {
        '-' | '=' | '?' | '+' => {
            let missing = if colon { !is_set || is_null } else { !is_set };
            match op {
                '-' => {
                    if missing {
                        walk_word(word, mode, ctx)?;
                        if quoted {
                            ctx.content += 1;
                        }
                    } else {
                        emit_val(ctx, &val, quoted);
                    }
                }
                '+' => {
                    if !missing {
                        walk_word(word, mode, ctx)?;
                    }
                    if quoted {
                        ctx.content += 1;
                    }
                }
                '=' => {
                    if missing {
                        if !crate::lexer::is_valid_name(&name) || sub.is_some() {
                            return fail(&format!("${{{}}}: cannot assign in this way", text));
                        }
                        let v = word_string(word, mode)?;
                        if !var_set(&name, &v, false) {
                            return Err(ExpError { status: 1 });
                        }
                        emit_str(ctx, &v, quoted);
                    } else {
                        emit_val(ctx, &val, quoted);
                    }
                }
                _ => {
                    if missing {
                        let msg = word_string(word, mode)?;
                        let msg = if msg.is_empty() {
                            "parameter null or not set".to_string()
                        } else {
                            msg
                        };
                        return fail(&format!("{}: {}", name, msg));
                    }
                    emit_val(ctx, &val, quoted);
                }
            }
            Ok(())
        }
        '#' | '%' => {
            check_set(&name, &val)?;
            let longest = word.first() == Some(&op);
            let pat_chars = if longest { &word[1..] } else { word };
            let pat = pattern_of(pat_chars)?;
            let f = |s: &str| {
                if op == '#' {
                    crate::pattern::strip_prefix(s, &pat, longest)
                } else {
                    crate::pattern::strip_suffix(s, &pat, longest)
                }
            };
            emit_val(ctx, &map_val(val, f), quoted);
            Ok(())
        }
        '/' => {
            check_set(&name, &val)?;
            let (mut global, mut anchor_start, mut anchor_end) = (false, false, false);
            let mut w = word;
            match w.first() {
                Some('/') => {
                    global = true;
                    w = &w[1..];
                }
                Some('#') => {
                    anchor_start = true;
                    w = &w[1..];
                }
                Some('%') => {
                    anchor_end = true;
                    w = &w[1..];
                }
                _ => {}
            }
            let sep = find_unquoted(w, '/');
            let (pat_part, repl_part) = match sep {
                Some(k) => (&w[..k], &w[k + 1..]),
                None => (w, &w[w.len()..]),
            };
            let pat = pattern_of(pat_part)?;
            let repl = word_string(repl_part, mode)?;
            let f = |s: &str| {
                crate::pattern::substitute(s, &pat, &repl, global, anchor_start, anchor_end)
            };
            emit_val(ctx, &map_val(val, f), quoted);
            Ok(())
        }
        ':' => {
            check_set(&name, &val)?;
            let spec = word;
            let (off_part, len_part) = match find_unquoted(spec, ':') {
                Some(k) => (&spec[..k], Some(&spec[k + 1..])),
                None => (spec, None),
            };
            let off = eval_arith_str(&word_string(off_part, Mode::Unquoted)?)?;
            let len = match len_part {
                Some(l) => Some(eval_arith_str(&word_string(l, Mode::Unquoted)?)?),
                None => None,
            };
            match val {
                Val::List(items, star) => {
                    let items = if name == "@" || name == "*" {
                        let mut all = vec![var_get("0")];
                        all.extend(items);
                        all
                    } else {
                        items
                    };
                    let sliced = slice(&items, off, len);
                    emit_list(ctx, &sliced, star, quoted);
                }
                other => {
                    let chars: Vec<String> =
                        other.joined().chars().map(|c| c.to_string()).collect();
                    emit_str(ctx, &slice(&chars, off, len).concat(), quoted);
                }
            }
            Ok(())
        }
        '^' | ',' => {
            check_set(&name, &val)?;
            let all = word.first() == Some(&op);
            let upper = op == '^';
            let f = |s: &str| {
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
            };
            emit_val(ctx, &map_val(val, f), quoted);
            Ok(())
        }
        _ => bad(),
    }
}

// Expands the word of `${x-word}` / `${x+word}` in place.
fn walk_word(word: &[char], mode: Mode, ctx: &mut Ctx) -> ExpResult<()> {
    let saved = ctx.unquoted_lit;
    if mode == Mode::Unquoted {
        ctx.unquoted_lit = S;
    }
    let r = walk(word, mode, ctx);
    ctx.unquoted_lit = saved;
    r
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

fn map_val(val: Val, f: impl Fn(&str) -> String) -> Val {
    match val {
        Val::Unset => Val::Str(f("")),
        Val::Str(s) => Val::Str(f(&s)),
        Val::List(items, star) => Val::List(items.iter().map(|s| f(s)).collect(), star),
    }
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
        if f & Q != 0 && matches!(c, '*' | '?' | '[' | ']' | '\\') {
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

fn brace_expand(word: &str) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        match chars[i] {
            '\\' => i += 2,
            '\'' => i = scan_squote(&chars, i).unwrap_or(n),
            '"' => i = scan_dquote(&chars, i).unwrap_or(n),
            '`' => i = scan_bquote(&chars, i).unwrap_or(n),
            '$' if chars.get(i + 1) == Some(&'{') => {
                i = scan_brace(&chars, i + 1, false).unwrap_or(n)
            }
            '$' if chars.get(i + 1) == Some(&'(') => i = scan_paren(&chars, i + 1).unwrap_or(n),
            '{' => {
                if let Some((close, commas)) = brace_close(&chars, i) {
                    let pre: String = chars[..i].iter().collect();
                    let post: String = chars[close + 1..].iter().collect();
                    let mut alts: Vec<String> = Vec::new();
                    if !commas.is_empty() {
                        let mut last = i + 1;
                        for &c in commas.iter().chain(std::iter::once(&close)) {
                            alts.push(chars[last..c].iter().collect());
                            last = c + 1;
                        }
                    } else if let Some(seq) = sequence(&chars[i + 1..close]) {
                        alts = seq;
                    }
                    if !alts.is_empty() {
                        let mut out = Vec::new();
                        for alt in alts {
                            out.extend(brace_expand(&format!("{}{}{}", pre, alt, post)));
                        }
                        return out;
                    }
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    vec![word.to_string()]
}

// Finds the `}` matching the `{` at `open` and the top-level commas.
fn brace_close(chars: &[char], open: usize) -> Option<(usize, Vec<usize>)> {
    let n = chars.len();
    let mut depth = 0;
    let mut commas = Vec::new();
    let mut i = open;
    while i < n {
        match chars[i] {
            '\\' => {
                i += 2;
                continue;
            }
            '\'' => {
                i = scan_squote(chars, i)?;
                continue;
            }
            '"' => {
                i = scan_dquote(chars, i)?;
                continue;
            }
            '$' if chars.get(i + 1) == Some(&'{') => {
                i = scan_brace(chars, i + 1, false)?;
                continue;
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                i = scan_paren(chars, i + 1)?;
                continue;
            }
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
        i += 1;
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

fn glob_piece(piece: &[(char, u8)], do_glob: bool) -> Vec<String> {
    let literal: String = piece
        .iter()
        .filter(|(_, f)| f & MARK == 0)
        .map(|(c, _)| *c)
        .collect();
    let has_meta = piece
        .iter()
        .any(|&(c, f)| f & (Q | MARK) == 0 && matches!(c, '*' | '?' | '['));
    if !do_glob || !has_meta {
        return vec![literal];
    }
    let mut pat = String::new();
    for &(c, f) in piece {
        if f & MARK != 0 {
            continue;
        }
        if (f & Q != 0 && matches!(c, '*' | '?' | '[' | ']')) || c == '\\' {
            pat.push('[');
            pat.push(c);
            pat.push(']');
        } else {
            pat.push(c);
        }
    }
    let opts = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: true,
    };
    let mut matches: Vec<String> = match glob::glob_with(&pat, opts) {
        Ok(paths) => paths
            .filter_map(Result::ok)
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    if matches.is_empty() {
        return vec![literal];
    }
    matches.sort();
    matches
}

fn finish(ctx: Ctx, do_glob: bool) -> Vec<String> {
    let ifs = ifs();
    let mut out = Vec::new();
    for field in &ctx.fields {
        for piece in split_field(field, &ifs) {
            out.extend(glob_piece(&piece, do_glob));
        }
    }
    out
}

// ---- public entry points --------------------------------------------------

/// Fully expands command words into fields.
pub fn expand_words(words: &[String]) -> ExpResult<Vec<String>> {
    let do_glob = !opt_set(OPT_NOGLOB);
    let mut out = Vec::new();
    for word in words {
        for w in brace_expand(word) {
            let chars: Vec<char> = w.chars().collect();
            let mut ctx = Ctx::new(true, false);
            walk(&chars, Mode::Unquoted, &mut ctx)?;
            out.extend(finish(ctx, do_glob));
        }
    }
    Ok(out)
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

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shell_init();
        lock
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
