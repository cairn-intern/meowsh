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
            return Err(format!("{}: invalid arithmetic base", s));
        }
        u64::from_str_radix(digits, base).map(|v| v as i64)
    } else if body.len() > 1 && body.starts_with('0') {
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

fn tokenize(s: &str) -> Result<Vec<T>, String> {
    let c: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        if ch.is_ascii_digit() || (ch == '.' && c.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let start = i;
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
        if ch.is_ascii_alphabetic() || ch == '_' {
            let start = i;
            while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '_') {
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
            return Err(format!(
                "syntax error: invalid arithmetic operator (error token is \"{}\")",
                c[i..].iter().collect::<String>()
            ));
        }
    }
    Ok(out)
}

struct P {
    toks: Vec<T>,
    pos: usize,
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
                return Err("division by 0".to_string());
            }
            l.wrapping_div(r)
        }
        "%" => {
            if r == 0 {
                return Err("division by 0".to_string());
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
fn call(name: &str, args: &[Num]) -> R {
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
                return Err("`:' expected for conditional expression".to_string());
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
                    _ => return Err(format!("syntax error: operand expected after {}", op)),
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
            Some(T::Op(op)) => Err(format!(
                "syntax error: operand expected (error token is \"{}\")",
                op
            )),
            None => Err("syntax error: operand expected".to_string()),
        }
    }
}

fn set_var(name: &str, v: Num) -> Result<(), String> {
    if crate::shell::var_set(name, &v.to_string(), false) {
        Ok(())
    } else {
        Err(format!("{}: readonly variable", name))
    }
}

/// Evaluates an arithmetic expression whose parameter expansions and
/// command substitutions have already been performed.
pub fn eval_num(expr: &str) -> R {
    let toks = tokenize(expr)?;
    if toks.is_empty() {
        return Ok(Num::I(0));
    }
    let mut p = P { toks, pos: 0 };
    let v = p.comma(false)?;
    if p.pos < p.toks.len() {
        return Err(format!(
            "syntax error in expression (error token is \"{:?}\")",
            p.toks[p.pos]
        ));
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
        assert_eq!(eval("0x10 + 010"), Ok(24));
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
