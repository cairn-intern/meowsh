//! Arithmetic expansion: `$(( ))`. Signed 64-bit integers wrapping on
//! overflow, zsh-style floats and math functions, C operator precedence, assignment operators, and short-circuiting
//! `&&`, `||`, and `?:` that skip side effects in unevaluated operands.

use std::cell::Cell;

/// An arithmetic value. Integers are signed 64-bit and wrap on overflow;
/// any float operand makes the result a float, as in zsh.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Num {
    I(i64),
    F(f64),
}

impl Num {
    pub fn as_int(self) -> i64 {
        match self {
            Num::I(n) => n,
            Num::F(f) => f as i64,
        }
    }

    pub fn as_float_pub(self) -> f64 {
        self.as_float()
    }

    fn as_float(self) -> f64 {
        match self {
            Num::I(n) => n as f64,
            Num::F(f) => f,
        }
    }

    pub fn truthy(self) -> bool {
        match self {
            Num::I(n) => n != 0,
            Num::F(f) => f != 0.0,
        }
    }
}

impl std::fmt::Display for Num {
    // Floats always show a decimal point (`1.`), like zsh.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Num::I(n) => write!(f, "{}", n),
            Num::F(x) => {
                let s = format!("{}", x);
                if x.is_finite() && !s.contains('.') && !s.contains('e') {
                    write!(f, "{}.", s)
                } else {
                    write!(f, "{}", s)
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum T {
    Num(Num),
    Ident(String),
    Op(&'static str),
}

const OPS: &[&str] = &[
    "<<=", ">>=", "**", "<<", ">>", "<=", ">=", "==", "!=", "&&", "||", "++", "--", "+=", "-=",
    "*=", "/=", "%=", "&=", "^=", "|=", "+", "-", "*", "/", "%", "<", ">", "&", "^", "|", "!", "~",
    "?", ":", "=", "(", ")", ",",
];

thread_local! {
    static DEPTH: Cell<usize> = const { Cell::new(0) };
}

pub fn parse_number(s: &str) -> Result<i64, String> {
    let s = s.trim();
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let v = if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).map(|v| v as i64)
    } else if let Some((base, digits)) = body.split_once('#') {
        let base: u32 = base.parse().map_err(|_| format!("{}: invalid number", s))?;
        if !(2..=36).contains(&base) {
            return Err(format!(
                "invalid base (must be 2 to 36 inclusive): {}",
                base
            ));
        }
        u64::from_str_radix(digits, base).map(|v| v as i64)
    } else if body.len() > 1 && body.starts_with('0') && crate::options::quick("octalzeroes") {
        // A leading zero means octal only with OCTAL_ZEROES (sh, ksh).
        u64::from_str_radix(&body[1..], 8).map(|v| v as i64)
    } else {
        body.parse::<u64>().map(|v| v as i64)
    };
    let v = v.map_err(|_| format!("{}: invalid number", s))?;
    Ok(if neg { v.wrapping_neg() } else { v })
}

// A decimal float literal: digits with a `.` and/or an exponent.
fn parse_float(s: &str) -> Option<f64> {
    let t = s.trim();
    let body = t.strip_prefix(['-', '+']).unwrap_or(t);
    let decimal = body
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '-' | '+'));
    if !decimal || body.starts_with("0x") || !(body.contains('.') || body.contains(['e', 'E'])) {
        return None;
    }
    t.parse::<f64>().ok()
}

fn parse_value(s: &str) -> Result<Num, String> {
    if let Some(f) = parse_float(s) {
        return Ok(Num::F(f));
    }
    parse_number(s).map(Num::I)
}

// Tokens and where each starts in the text, for error messages.
fn tokenize(s: &str) -> Result<(Vec<T>, Vec<usize>), String> {
    let c: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut starts = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        starts.truncate(out.len());
        starts.push(i);
        if ch.is_ascii_digit() || (ch == '.' && c.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let start = i;
            // OCTAL_ZEROES: `09` is the number 0 followed by 9.
            if ch == '0' && crate::options::quick("octalzeroes") {
                let mut j = i + 1;
                while j < c.len() && c[j].is_ascii_digit() {
                    j += 1;
                }
                let plain = !matches!(c.get(j), Some('.' | '#' | 'e' | 'E' | 'x' | 'X'));
                if plain && j > i + 1 {
                    let mut k = i + 1;
                    while k < j && ('0'..='7').contains(&c[k]) {
                        k += 1;
                    }
                    if k < j {
                        let text: String = c[i..k].iter().collect();
                        out.push(T::Num(parse_value(&text)?));
                        i = k;
                        continue;
                    }
                }
            }
            while i < c.len() {
                let d = c[i];
                let exp_sign = matches!(d, '+' | '-')
                    && matches!(c[i - 1], 'e' | 'E')
                    && !c[start..i].iter().any(|x| matches!(x, 'x' | 'X' | '#'));
                if d.is_ascii_alphanumeric() || d == '#' || d == '_' || d == '.' || exp_sign {
                    i += 1;
                } else {
                    break;
                }
            }
            let text: String = c[start..i].iter().collect();
            out.push(T::Num(parse_value(&text)?));
            continue;
        }
        // `##c` (or `#\c`): the character code of c; `#name`: of the
        // first character of $name.
        if ch == '#' {
            let code = |x: Option<&char>| x.map(|&x| x as i64);
            if matches!(c.get(i + 1), Some('#' | '\\')) {
                let mut k = i + 2;
                if c[i + 1] == '#' && c.get(k) == Some(&'\\') && k + 1 < c.len() {
                    k += 1;
                }
                let v = code(c.get(k)).ok_or("character missing after ##")?;
                out.push(T::Num(Num::I(v)));
                i = k + 1;
                continue;
            }
            if c.get(i + 1)
                .is_some_and(|x| x.is_ascii_alphabetic() || *x == '_')
            {
                let start = i + 1;
                i = start;
                while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '_') {
                    i += 1;
                }
                let name: String = c[start..i].iter().collect();
                let v = crate::shell::var_get(&name)
                    .chars()
                    .next()
                    .map_or(0, |x| x as i64);
                out.push(T::Num(Num::I(v)));
                continue;
            }
        }
        if crate::lexer::is_name_start(ch) {
            let start = i;
            while i < c.len() && crate::lexer::is_name_char(c[i]) {
                i += 1;
            }
            out.push(T::Ident(c[start..i].iter().collect()));
            continue;
        }
        let mut matched = false;
        for op in OPS {
            let len = op.len();
            if i + len <= c.len() && c[i..i + len].iter().copied().eq(op.chars()) {
                out.push(T::Op(op));
                i += len;
                matched = true;
                break;
            }
        }
        if !matched {
            return Err(format!("bad math expression: illegal character: {}", c[i]));
        }
    }
    starts.truncate(out.len());
    Ok((out, starts))
}

struct P {
    toks: Vec<T>,
    pos: usize,
    src: Vec<char>,
    starts: Vec<usize>,
}

type R = Result<Num, String>;

fn var_value(name: &str) -> R {
    let Some(val) = crate::shell::var_get_opt(name) else {
        if crate::shell::opt_set(crate::types::OPT_NOUNSET) {
            return Err(format!("{}: parameter not set", name));
        }
        return Ok(Num::I(0));
    };
    if val.trim().is_empty() {
        return Ok(Num::I(0));
    }
    if let Ok(n) = parse_value(&val) {
        return Ok(n);
    }
    // A variable holding an expression is evaluated recursively.
    let depth = DEPTH.with(|d| d.get());
    if depth > 64 {
        return Err(format!("{}: expression recursion level exceeded", name));
    }
    DEPTH.with(|d| d.set(depth + 1));
    let r = eval_num(&val);
    DEPTH.with(|d| d.set(depth));
    r
}

fn int_binop(op: &str, l: i64, r: i64) -> Result<i64, String> {
    Ok(match op {
        "+" => l.wrapping_add(r),
        "-" => l.wrapping_sub(r),
        "*" => l.wrapping_mul(r),
        "/" => {
            if r == 0 {
                return Err("division by zero".to_string());
            }
            l.wrapping_div(r)
        }
        "%" => {
            if r == 0 {
                return Err("division by zero".to_string());
            }
            l.wrapping_rem(r)
        }
        "<<" => l.wrapping_shl((r & 63) as u32),
        ">>" => l.wrapping_shr((r & 63) as u32),
        "&" => l & r,
        "^" => l ^ r,
        "|" => l | r,
        "**" => {
            if r < 0 {
                return Err("exponent less than 0".to_string());
            }
            let mut acc: i64 = 1;
            for _ in 0..r.min(64) {
                acc = acc.wrapping_mul(l);
            }
            if r > 64 && l.abs() > 1 {
                acc = 0;
            }
            acc
        }
        _ => return Err(format!("unknown operator {}", op)),
    })
}

fn binop(op: &str, l: Num, r: Num) -> R {
    let either_float = matches!(l, Num::F(_)) || matches!(r, Num::F(_));
    // Bitwise operators always work on integers.
    if !either_float || matches!(op, "<<" | ">>" | "&" | "^" | "|") {
        return int_binop(op, l.as_int(), r.as_int()).map(Num::I);
    }
    let (a, b) = (l.as_float(), r.as_float());
    Ok(Num::F(match op {
        "+" => a + b,
        "-" => a - b,
        "*" => a * b,
        "/" => a / b,
        "%" => a % b,
        "**" => a.powf(b),
        _ => return Err(format!("unknown operator {}", op)),
    }))
}

fn compare(op: &str, l: Num, r: Num) -> bool {
    let ord = match (l, r) {
        (Num::I(a), Num::I(b)) => a.partial_cmp(&b),
        _ => l.as_float().partial_cmp(&r.as_float()),
    };
    use std::cmp::Ordering::*;
    match op {
        "==" => ord == Some(Equal),
        "!=" => ord != Some(Equal),
        "<" => ord == Some(Less),
        "<=" => matches!(ord, Some(Less | Equal)),
        ">" => ord == Some(Greater),
        _ => matches!(ord, Some(Greater | Equal)),
    }
}

// zsh/mathfunc functions.
/// A math function defined with `functions -M`: its name in
/// expressions, argument count limits, the shell function implementing
/// it, and whether it takes its argument as a string (`-s`).
#[derive(Clone)]
pub struct MathFunc {
    pub name: String,
    pub min: i64,
    pub max: i64,
    pub shellfn: String,
    pub string: bool,
}

pub static MATH_FUNCS: std::sync::RwLock<Vec<MathFunc>> = std::sync::RwLock::new(Vec::new());

// The value of the last arithmetic evaluation: a math function's result.
static LAST_VALUE: std::sync::Mutex<Option<Num>> = std::sync::Mutex::new(None);

fn math_func(name: &str) -> Option<MathFunc> {
    MATH_FUNCS
        .read()
        .ok()?
        .iter()
        .find(|m| m.name == name)
        .cloned()
}

// Calls the shell function behind a `functions -M` math function.
fn call_user(m: &MathFunc, args: Vec<String>) -> R {
    let n = args.len() as i64;
    if n < m.min || (m.max >= 0 && n > m.max) {
        return Err(format!(
            "wrong number of arguments: {}({})",
            m.name,
            args.join(",")
        ));
    }
    let body = crate::shell::with_shell(|s| {
        s.functions
            .get(&m.shellfn)
            .map(|f| (f.body.clone(), f.line))
    });
    let Some((body, line)) = body else {
        return Err(format!("{}: function not defined", m.shellfn));
    };
    if let Ok(mut l) = LAST_VALUE.lock() {
        *l = None;
    }
    let mut words = vec![m.name.clone()];
    words.extend(args);
    crate::exec::call_function(&body, &words, line);
    Ok(LAST_VALUE.lock().ok().and_then(|l| *l).unwrap_or(Num::I(0)))
}

fn call(name: &str, args: &[Num]) -> R {
    if let Some(m) = math_func(name) {
        return call_user(&m, args.iter().map(|a| a.to_string()).collect());
    }
    let one = |f: fn(f64) -> f64| -> R {
        match args {
            [x] => Ok(Num::F(f(x.as_float()))),
            _ => Err(format!("{}: wrong number of arguments", name)),
        }
    };
    match name {
        "int" => match args {
            [x] => Ok(Num::I(x.as_int())),
            _ => Err(format!("{}: wrong number of arguments", name)),
        },
        "float" => one(|x| x),
        "abs" => match args {
            [Num::I(n)] => Ok(Num::I(n.wrapping_abs())),
            _ => one(f64::abs),
        },
        "rint" => one(f64::round_ties_even),
        "ceil" => one(f64::ceil),
        "floor" => one(f64::floor),
        "trunc" => one(f64::trunc),
        "sqrt" => one(f64::sqrt),
        "exp" => one(f64::exp),
        "log" => one(f64::ln),
        "log2" => one(f64::log2),
        "log10" => one(f64::log10),
        "sin" => one(f64::sin),
        "cos" => one(f64::cos),
        "tan" => one(f64::tan),
        _ => Err(format!("{}: unknown function", name)),
    }
}

impl P {
    // zsh's "<what> expected at `rest'" or "... at end of string".
    fn expected(&self, what: &str) -> String {
        match self.starts.get(self.pos) {
            Some(&k) => format!(
                "bad math expression: {} expected at `{}'",
                what,
                self.src[k..].iter().collect::<String>()
            ),
            None => format!("bad math expression: {} expected at end of string", what),
        }
    }

    fn peek(&self) -> Option<&T> {
        self.toks.get(self.pos)
    }

    fn peek_op(&self, op: &str) -> bool {
        matches!(self.peek(), Some(T::Op(o)) if *o == op)
    }

    fn eat_op(&mut self, ops: &[&'static str]) -> Option<&'static str> {
        if let Some(T::Op(o)) = self.peek() {
            if let Some(found) = ops.iter().find(|x| *x == o) {
                self.pos += 1;
                return Some(found);
            }
        }
        None
    }

    fn comma(&mut self, skip: bool) -> R {
        let mut v = self.assign(skip)?;
        while self.eat_op(&[","]).is_some() {
            v = self.assign(skip)?;
        }
        Ok(v)
    }

    fn assign(&mut self, skip: bool) -> R {
        if let (Some(T::Ident(name)), Some(T::Op(op))) = (
            self.toks.get(self.pos).cloned(),
            self.toks.get(self.pos + 1).cloned(),
        ) {
            if matches!(
                op,
                "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "<<=" | ">>=" | "&=" | "^=" | "|="
            ) {
                self.pos += 2;
                let rhs = self.assign(skip)?;
                if skip {
                    return Ok(Num::I(0));
                }
                let v = if op == "=" {
                    rhs
                } else {
                    binop(&op[..op.len() - 1], var_value(&name)?, rhs)?
                };
                set_var(&name, v)?;
                return Ok(v);
            }
        }
        self.ternary(skip)
    }

    fn ternary(&mut self, skip: bool) -> R {
        let v = self.lor(skip)?;
        if self.eat_op(&["?"]).is_some() {
            let c = v.truthy();
            let a = self.comma(skip || !c)?;
            if self.eat_op(&[":"]).is_none() {
                return Err("bad math expression: ':' expected".to_string());
            }
            let b = self.assign(skip || c)?;
            return Ok(if c { a } else { b });
        }
        Ok(v)
    }

    fn lor(&mut self, skip: bool) -> R {
        let mut l = self.land(skip)?;
        while self.eat_op(&["||"]).is_some() {
            let r = self.land(skip || l.truthy())?;
            l = Num::I((l.truthy() || r.truthy()) as i64);
        }
        Ok(l)
    }

    fn land(&mut self, skip: bool) -> R {
        let mut l = self.binary(0, skip)?;
        while self.eat_op(&["&&"]).is_some() {
            let r = self.binary(0, skip || !l.truthy())?;
            l = Num::I((l.truthy() && r.truthy()) as i64);
        }
        Ok(l)
    }

    // Binary levels from loosest to tightest.
    fn binary(&mut self, level: usize, skip: bool) -> R {
        const LEVELS: &[&[&str]] = &[
            &["|"],
            &["^"],
            &["&"],
            &["==", "!="],
            &["<", "<=", ">", ">="],
            &["<<", ">>"],
            &["+", "-"],
            &["*", "/", "%"],
        ];
        if level >= LEVELS.len() {
            return self.power(skip);
        }
        let mut l = self.binary(level + 1, skip)?;
        while let Some(op) = self.eat_op(LEVELS[level]) {
            let r = self.binary(level + 1, skip)?;
            l = match op {
                "==" | "!=" | "<" | "<=" | ">" | ">=" => Num::I(compare(op, l, r) as i64),
                _ if skip => Num::I(0),
                _ => binop(op, l, r)?,
            };
        }
        Ok(l)
    }

    fn power(&mut self, skip: bool) -> R {
        let base = self.unary(skip)?;
        if self.eat_op(&["**"]).is_some() {
            let exp = self.power(skip)?;
            if skip {
                return Ok(Num::I(0));
            }
            return binop("**", base, exp);
        }
        Ok(base)
    }

    fn unary(&mut self, skip: bool) -> R {
        if let Some(op) = self.eat_op(&["+", "-", "!", "~", "++", "--"]) {
            if op == "++" || op == "--" {
                let name = match self.peek() {
                    Some(T::Ident(n)) => n.clone(),
                    _ => return Err("bad math expression: lvalue required".to_string()),
                };
                self.pos += 1;
                if skip {
                    return Ok(Num::I(0));
                }
                let v = var_value(&name)?;
                let nv = binop(if op == "++" { "+" } else { "-" }, v, Num::I(1))?;
                set_var(&name, nv)?;
                return Ok(nv);
            }
            let v = self.unary(skip)?;
            return Ok(match (op, v) {
                ("+", v) => v,
                ("-", Num::I(n)) => Num::I(n.wrapping_neg()),
                ("-", Num::F(f)) => Num::F(-f),
                ("!", v) => Num::I(!v.truthy() as i64),
                (_, v) => Num::I(!v.as_int()),
            });
        }
        self.postfix(skip)
    }

    fn postfix(&mut self, skip: bool) -> R {
        match self.peek().cloned() {
            Some(T::Num(n)) => {
                self.pos += 1;
                Ok(n)
            }
            Some(T::Ident(name)) => {
                self.pos += 1;
                // `functions -Ms`: the text between the parentheses is
                // the one argument.
                if self.peek_op("(") && math_func(&name).is_some_and(|m| m.string) {
                    let start = self.starts[self.pos] + 1;
                    let mut depth = 0;
                    let mut end = start;
                    for (k, &c) in self.src.iter().enumerate().skip(start) {
                        match c {
                            '(' => depth += 1,
                            ')' if depth == 0 => {
                                end = k;
                                break;
                            }
                            ')' => depth -= 1,
                            _ => {}
                        }
                        end = k + 1;
                    }
                    let arg: String = self.src[start..end.min(self.src.len())].iter().collect();
                    while self.pos < self.toks.len() && self.starts[self.pos] <= end {
                        self.pos += 1;
                    }
                    if skip {
                        return Ok(Num::I(0));
                    }
                    let m = math_func(&name).unwrap();
                    return call_user(&m, vec![arg]);
                }
                if self.peek_op("(") {
                    self.pos += 1;
                    let mut args = Vec::new();
                    if !self.peek_op(")") {
                        args.push(self.assign(skip)?);
                        while self.eat_op(&[","]).is_some() {
                            args.push(self.assign(skip)?);
                        }
                    }
                    if !self.peek_op(")") {
                        return Err("missing `)'".to_string());
                    }
                    self.pos += 1;
                    if skip {
                        return Ok(Num::I(0));
                    }
                    return call(&name, &args);
                }
                if let Some(op) = self.eat_op(&["++", "--"]) {
                    if skip {
                        return Ok(Num::I(0));
                    }
                    let v = var_value(&name)?;
                    let nv = binop(if op == "++" { "+" } else { "-" }, v, Num::I(1))?;
                    set_var(&name, nv)?;
                    return Ok(v);
                }
                if skip {
                    return Ok(Num::I(0));
                }
                var_value(&name)
            }
            Some(T::Op("(")) => {
                self.pos += 1;
                let v = self.comma(skip)?;
                if !self.peek_op(")") {
                    return Err("missing `)'".to_string());
                }
                self.pos += 1;
                Ok(v)
            }
            Some(T::Op(_)) | None => Err(self.expected("operand")),
        }
    }
}

fn set_var(name: &str, v: Num) -> Result<(), String> {
    if crate::shell::var_set(name, &v.to_string(), false) {
        Ok(())
    } else {
        Err(format!("read-only variable: {}", name))
    }
}

/// Evaluates an arithmetic expression whose parameter expansions and
/// command substitutions have already been performed.
pub fn eval_num(expr: &str) -> R {
    let (toks, starts) = tokenize(expr)?;
    if toks.is_empty() {
        return Ok(Num::I(0));
    }
    let mut p = P {
        toks,
        pos: 0,
        src: expr.chars().collect(),
        starts,
    };
    let v = p.comma(false)?;
    if let Ok(mut l) = LAST_VALUE.lock() {
        *l = Some(v);
    }
    if p.pos < p.toks.len() {
        return Err(match &p.toks[p.pos] {
            T::Op(":") => "bad math expression: ':' without '?'".to_string(),
            T::Op("=") => "bad math expression: lvalue required".to_string(),
            _ => p.expected("operator"),
        });
    }
    Ok(v)
}

/// Like `eval_num`, truncating a float result to an integer.
pub fn eval(expr: &str) -> Result<i64, String> {
    eval_num(expr).map(Num::as_int)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_and_associativity() {
        assert_eq!(eval("10-2-3"), Ok(5));
        assert_eq!(eval("3*(2+1)"), Ok(9));
        assert_eq!(eval(" (1+2)*3 "), Ok(9));
        assert_eq!(eval("2<3"), Ok(1));
        assert_eq!(eval("5%3"), Ok(2));
        assert_eq!(eval("1<<2"), Ok(4));
        assert_eq!(eval("5>3?1:2"), Ok(1));
        assert_eq!(eval("2**10"), Ok(1024));
        assert_eq!(eval("-2+3"), Ok(1));
        assert_eq!(eval("0x10 + 8#10"), Ok(24));
        // Without OCTAL_ZEROES a leading zero is decimal.
        assert_eq!(eval("010"), Ok(10));
        assert_eq!(eval("1 || 1/0"), Ok(1));
    }

    #[test]
    fn overflow_wraps() {
        assert_eq!(eval("9223372036854775807+1"), Ok(i64::MIN));
    }

    #[test]
    fn division_by_zero_is_error() {
        assert!(eval("7/0").is_err());
        assert!(eval("7%0").is_err());
    }

    #[test]
    fn floats_and_math_functions() {
        assert_eq!(eval_num("1.5 * 2"), Ok(Num::F(3.0)));
        assert_eq!(eval_num("10 / 4.").map(|n| n.to_string()), Ok("2.5".into()));
        assert_eq!(eval_num("1.0").map(|n| n.to_string()), Ok("1.".into()));
        assert_eq!(eval_num("int(rint(2.5 * 1000))"), Ok(Num::I(2500)));
        assert_eq!(eval_num("int(1.9)"), Ok(Num::I(1)));
        assert_eq!(eval_num("floor(-1.5)"), Ok(Num::F(-2.0)));
        assert_eq!(eval_num("1e3"), Ok(Num::F(1000.0)));
        assert_eq!(eval_num("0.5 < 1"), Ok(Num::I(1)));
        assert_eq!(eval("7 / 2"), Ok(3));
        assert!(eval_num("nope(1)").is_err());
    }

    #[test]
    fn bad_syntax_is_error() {
        assert!(eval("1 +").is_err());
        assert!(eval("1 2").is_err());
        assert!(eval("1 @ 2").is_err());
    }
}
