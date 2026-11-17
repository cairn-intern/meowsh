//! zsh/zutil's zformat, after Src/Modules/zutil.c.

use crate::shell::builtin_err;

struct Specs {
    text: std::collections::HashMap<char, Vec<char>>,
    // How many `%` quoting doubled in each spec.
    quoted: std::collections::HashMap<char, usize>,
}

// One chunk of a -f/-F format, up to `endchar` (or the end); zsh's
// zformat_substring. Returns the index of the terminator, None for a
// malformed format.
#[allow(clippy::too_many_arguments)]
fn substring(
    s: &[char],
    mut i: usize,
    specs: &Specs,
    out: &mut String,
    endchar: Option<char>,
    presence: bool,
    quote: bool,
    skip: bool,
) -> Option<usize> {
    while i < s.len() && Some(s[i]) != endchar {
        if s[i] != '%' {
            if !skip {
                out.push(s[i]);
            }
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        let mut right = s.get(i) == Some(&'-');
        if right {
            i += 1;
        }
        let (mut min, mut max) = (-1i64, -1i64);
        if s.get(i).is_some_and(|c| c.is_ascii_digit()) {
            min = 0;
            while let Some(d) = s.get(i).and_then(|c| c.to_digit(10)) {
                min = min * 10 + d as i64;
                i += 1;
            }
        }
        let testit = s.get(i) == Some(&'(');
        if testit && s.get(i + 1) == Some(&'-') {
            right = true;
            i += 1;
        }
        if (s.get(i) == Some(&'.') || testit) && s.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
            max = 0;
            i += 1;
            while let Some(d) = s.get(i).and_then(|c| c.to_digit(10)) {
                max = max * 10 + d as i64;
                i += 1;
            }
        } else if s.get(i) == Some(&'.') || testit {
            i += 1;
        }
        let c = s.get(i).copied();
        // Not a spec character: the sequence is literal text.
        if !testit && matches!(c, None | Some('%' | ')' | '-' | '.')) {
            let mut st = start;
            if !quote && i - start == 1 && matches!(c, Some('%' | ')')) {
                st += 1;
            }
            i = st;
            if !skip {
                out.push(s[i]);
            }
            i += 1;
            continue;
        }
        if let (true, Some(c)) = (testit, c) {
            let mut testval = if min >= 0 {
                min
            } else if max >= 0 {
                max
            } else {
                0
            };
            let actval: bool = match specs.text.get(&c).filter(|v| !v.is_empty()) {
                Some(v) => {
                    if presence {
                        let mut a = if testval != 0 {
                            crate::display_width(&v.iter().collect::<String>()) as i64
                        } else {
                            1
                        };
                        a -= *specs.quoted.get(&c).unwrap_or(&0) as i64;
                        if right {
                            testval < a
                        } else {
                            testval >= a
                        }
                    } else {
                        if right {
                            testval = -testval;
                        }
                        let n = crate::arith::eval(&v.iter().collect::<String>()).unwrap_or(0);
                        n - testval != 0
                    }
                }
                None => {
                    if presence {
                        !right
                    } else {
                        testval != 0
                    }
                }
            };
            i += 1;
            let endcharl = *s.get(i)?;
            let e = substring(
                s,
                i + 1,
                specs,
                out,
                Some(endcharl),
                presence,
                quote,
                skip || actval,
            )?;
            if e >= s.len() {
                return None;
            }
            let e2 = substring(
                s,
                e + 1,
                specs,
                out,
                Some(')'),
                presence,
                quote,
                skip || !actval,
            )?;
            if e2 >= s.len() {
                return None;
            }
            i = e2 + 1;
            continue;
        }
        let Some(c) = c else {
            // `%(` at the very end: kept as text.
            if !skip {
                out.extend(s[start..i].iter());
            }
            return Some(s.len());
        };
        if skip {
            i += 1;
            continue;
        }
        match specs.text.get(&c) {
            Some(spec) => {
                let (mut smin, mut smax) = (min, max);
                let q = *specs.quoted.get(&c).unwrap_or(&0);
                if (smin > 0 || smax > 0) && q > 0 {
                    let mut k = 0;
                    while k < spec.len() {
                        if spec[k] == '%' {
                            if smin > 0 && (k as i64) < smin {
                                smin += 1;
                            }
                            if smax > 0 && (k as i64) < smax {
                                smax += 1;
                            }
                            k += 1;
                        }
                        k += 1;
                    }
                }
                let mut len = spec.len() as i64;
                if smax >= 0 && len > smax {
                    len = smax;
                }
                let outl = if smin >= 0 { smin.max(len) } else { len };
                let text: String = spec[..len as usize].iter().collect();
                let pad = " ".repeat((outl - len) as usize);
                if len >= outl {
                    out.extend(spec[..outl as usize].iter());
                } else if right {
                    out.push_str(&pad);
                    out.push_str(&text);
                } else {
                    out.push_str(&text);
                    out.push_str(&pad);
                }
            }
            None => out.extend(s[start..=i].iter()),
        }
        i += 1;
    }
    Some(i)
}

/// The zformat builtin.
pub fn builtin_zformat(words: &[String]) -> i32 {
    let name = "zformat";
    let mut opts = String::new();
    let mut i = 1;
    while let Some(w) = words.get(i) {
        if w == "--" {
            i += 1;
            break;
        }
        if !w.starts_with('-') || w.len() < 2 {
            break;
        }
        for c in w[1..].chars() {
            if !"afFqQ".contains(c) {
                builtin_err(name, &format!("bad option: -{}", c));
                return 1;
            }
            opts.push(c);
        }
        i += 1;
    }
    let args = &words[i..];
    let o = |c: char| opts.contains(c);
    let qopt = if o('q') {
        Some('q')
    } else if o('Q') {
        Some('Q')
    } else {
        None
    };
    if o('q') && o('Q') {
        builtin_err(name, "only one of -qQ allowed");
        return 1;
    }
    let n = ['a', 'f', 'F'].iter().filter(|c| o(**c)).count();
    if n < 1 {
        builtin_err(name, "one of -afF expected");
        return 1;
    }
    if n > 1 {
        builtin_err(name, "only one of -afF allowed");
        return 1;
    }
    if o('a') && qopt.is_some() {
        builtin_err(name, "-qQ not allowed with -a");
        return 1;
    }
    if args.len() < 2 {
        builtin_err(name, "not enough arguments");
        return 1;
    }
    if o('a') {
        return zformat_align(args);
    }
    let presence = o('F');
    let mut specs = Specs {
        text: Default::default(),
        quoted: Default::default(),
    };
    for a in &args[2..] {
        let mut arg: Vec<char> = a.chars().collect();
        let mut quote = qopt.is_some();
        if arg.len() >= 2 && arg[0] == '%' && arg[1] == '%' {
            quote = false;
            arg.drain(..2);
        } else if arg.first() == Some(&'%') {
            quote = true;
            arg.remove(0);
        }
        let bad = match arg.first() {
            None => true,
            Some(c) => {
                matches!(c, '-' | '.' | '%' | ')') || c.is_ascii_digit() || arg.get(1) != Some(&':')
            }
        };
        if bad {
            builtin_err(name, &format!("invalid spec: {}", a));
            return 1;
        }
        let key = arg[0];
        let body: Vec<char> = arg[2..].to_vec();
        if quote {
            let pct = body.iter().filter(|c| **c == '%').count();
            let mut q = Vec::new();
            for c in body {
                q.push(c);
                if c == '%' {
                    q.push('%');
                }
            }
            specs.text.insert(key, q);
            specs.quoted.insert(key, pct);
        } else {
            specs.text.insert(key, body);
            specs.quoted.insert(key, 0);
        }
    }
    let fmt: Vec<char> = args[1].chars().collect();
    let mut out = String::new();
    if substring(
        &fmt,
        0,
        &specs,
        &mut out,
        None,
        presence,
        qopt == Some('q'),
        false,
    )
    .is_none()
    {
        builtin_err(name, &format!("malformed format string: {}", args[1]));
        return 1;
    }
    i32::from(!crate::shell::var_set(&args[0], &out, false))
}

// zformat -a array sep spec...: aligns the `name:description` specs on
// the separator.
fn zformat_align(args: &[String]) -> i32 {
    let mut pre = 0usize;
    for a in &args[2..] {
        let c: Vec<char> = a.chars().collect();
        let mut k = 0;
        let mut name = String::new();
        while k < c.len() && c[k] != ':' {
            if c[k] == '\\' && k + 1 < c.len() {
                k += 1;
            }
            name.push(c[k]);
            k += 1;
        }
        if k < c.len() && c[k] == ':' && k + 1 < c.len() {
            pre = pre.max(crate::display_width(&name));
        }
    }
    let sep = &args[1];
    let mut out = Vec::new();
    for a in &args[2..] {
        let c: Vec<char> = a.chars().collect();
        let mut k = 0;
        let mut name = String::new();
        while k < c.len() && c[k] != ':' {
            if c[k] == '\\' && k + 1 < c.len() {
                k += 1;
            }
            name.push(c[k]);
            k += 1;
        }
        if k < c.len() && c[k] == ':' && k + 1 < c.len() {
            let rest: String = c[k + 1..].iter().collect();
            let pad = pre - crate::display_width(&name);
            out.push(format!("{}{}{}{}", name, " ".repeat(pad), sep, rest));
        } else {
            out.push(name);
        }
    }
    if args[0] == "argv" {
        crate::shell::with_shell(|s| s.pos_params = out);
        return 0;
    }
    i32::from(!crate::shell::array_set(&args[0], out))
}
