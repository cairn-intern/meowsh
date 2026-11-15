//! `test`, `[`, and `[[ ]]` evaluation.

use crate::expand::{expand_pattern, expand_string};
use crate::shell::builtin_err;
use crate::sys::err_line;
use std::ffi::CString;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

type TResult = Result<bool, String>;

fn is_unary(op: &str) -> bool {
    matches!(
        op,
        "-b" | "-c"
            | "-d"
            | "-e"
            | "-f"
            | "-g"
            | "-h"
            | "-k"
            | "-L"
            | "-n"
            | "-p"
            | "-r"
            | "-s"
            | "-S"
            | "-t"
            | "-u"
            | "-w"
            | "-x"
            | "-z"
            | "-O"
            | "-G"
            | "-N"
            | "-a"
            | "-v"
            | "-o"
    )
}

fn is_binary(op: &str) -> bool {
    matches!(
        op,
        "=" | "=="
            | "!="
            | "<"
            | ">"
            | "-eq"
            | "-ne"
            | "-lt"
            | "-le"
            | "-gt"
            | "-ge"
            | "-nt"
            | "-ot"
            | "-ef"
            | "=~"
    )
}

fn access(path: &str, mode: i32) -> bool {
    match CString::new(path) {
        Ok(c) => unsafe { libc::access(c.as_ptr(), mode) == 0 },
        Err(_) => false,
    }
}

fn unary(op: &str, arg: &str) -> TResult {
    let meta = || std::fs::metadata(arg);
    Ok(match op {
        "-n" => !arg.is_empty(),
        "-z" => arg.is_empty(),
        "-e" | "-a" => meta().is_ok(),
        "-f" => meta().map(|m| m.is_file()).unwrap_or(false),
        "-d" => meta().map(|m| m.is_dir()).unwrap_or(false),
        "-b" => meta()
            .map(|m| m.file_type().is_block_device())
            .unwrap_or(false),
        "-c" => meta()
            .map(|m| m.file_type().is_char_device())
            .unwrap_or(false),
        "-p" => meta().map(|m| m.file_type().is_fifo()).unwrap_or(false),
        "-S" => meta().map(|m| m.file_type().is_socket()).unwrap_or(false),
        "-h" | "-L" => std::fs::symlink_metadata(arg)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        "-s" => meta().map(|m| m.len() > 0).unwrap_or(false),
        "-g" => meta()
            .map(|m| m.permissions().mode() & 0o2000 != 0)
            .unwrap_or(false),
        "-u" => meta()
            .map(|m| m.permissions().mode() & 0o4000 != 0)
            .unwrap_or(false),
        "-k" => meta()
            .map(|m| m.permissions().mode() & 0o1000 != 0)
            .unwrap_or(false),
        "-r" => access(arg, libc::R_OK),
        "-w" => access(arg, libc::W_OK),
        "-x" => access(arg, libc::X_OK),
        "-O" => meta()
            .map(|m| m.uid() == unsafe { libc::geteuid() })
            .unwrap_or(false),
        "-G" => meta()
            .map(|m| m.gid() == unsafe { libc::getegid() })
            .unwrap_or(false),
        "-N" => meta().map(|m| m.mtime() > m.atime()).unwrap_or(false),
        "-t" => {
            let fd: i32 = arg
                .trim()
                .parse()
                .map_err(|_| format!("{}: integer expression expected", arg))?;
            unsafe { libc::isatty(fd) == 1 }
        }
        "-v" => var_is_set(arg),
        // -o option: whether the shell option is on.
        "-o" => crate::options::resolve(arg)
            .is_some_and(|(name, on)| crate::options::is_set(name) == on),
        _ => return Err(format!("{}: unary operator expected", op)),
    })
}

// `-v name` or `-v name[subscript]`.
fn var_is_set(arg: &str) -> bool {
    let Some((name, sub)) = arg.strip_suffix(']').and_then(|a| a.split_once('[')) else {
        return crate::shell::var_exists(arg);
    };
    if let Some(v) = crate::shell::assoc_get(name, sub) {
        return !v.is_empty()
            || crate::shell::assoc_keys(name).is_some_and(|k| k.iter().any(|k| k == sub));
    }
    let Some(items) = crate::shell::with_shell(|s| s.array(name).cloned()) else {
        return false;
    };
    match crate::arith::eval(sub) {
        Ok(i) if i > 0 => (i as usize) <= items.len(),
        Ok(i) if i < 0 => i.unsigned_abs() as usize <= items.len(),
        _ => false,
    }
}

fn int(s: &str) -> Result<i64, String> {
    s.trim()
        .parse::<i64>()
        .map_err(|_| format!("{}: integer expression expected", s))
}

fn regex_match(s: &str, re: &str) -> TResult {
    let (Ok(cs), Ok(cre)) = (CString::new(s), CString::new(re)) else {
        return Ok(false);
    };
    unsafe {
        let mut preg: libc::regex_t = std::mem::zeroed();
        if libc::regcomp(
            &mut preg,
            cre.as_ptr(),
            libc::REG_EXTENDED | libc::REG_NOSUB,
        ) != 0
        {
            return Err(format!("{}: invalid regular expression", re));
        }
        let r = libc::regexec(&preg, cs.as_ptr(), 0, std::ptr::null_mut(), 0);
        libc::regfree(&mut preg);
        Ok(r == 0)
    }
}

fn binary(a: &str, op: &str, b: &str) -> TResult {
    Ok(match op {
        "=" | "==" => a == b,
        "!=" => a != b,
        "<" => a < b,
        ">" => a > b,
        "-eq" => int(a)? == int(b)?,
        "-ne" => int(a)? != int(b)?,
        "-lt" => int(a)? < int(b)?,
        "-le" => int(a)? <= int(b)?,
        "-gt" => int(a)? > int(b)?,
        "-ge" => int(a)? >= int(b)?,
        "-nt" | "-ot" => {
            let ma = std::fs::metadata(a)
                .ok()
                .map(|m| (m.mtime(), m.mtime_nsec()));
            let mb = std::fs::metadata(b)
                .ok()
                .map(|m| (m.mtime(), m.mtime_nsec()));
            match (ma, mb) {
                (Some(x), Some(y)) => {
                    if op == "-nt" {
                        x > y
                    } else {
                        x < y
                    }
                }
                (Some(_), None) => op == "-nt",
                (None, Some(_)) => op == "-ot",
                _ => false,
            }
        }
        "-ef" => match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        },
        "=~" => regex_match(a, b)?,
        _ => return Err(format!("{}: binary operator expected", op)),
    })
}

// ---- test / [ --------------------------------------------------------------

struct TestParser<'a> {
    args: &'a [String],
    pos: usize,
}

impl TestParser<'_> {
    fn peek(&self) -> Option<&str> {
        self.args.get(self.pos).map(|s| s.as_str())
    }

    fn or(&mut self) -> TResult {
        let mut v = self.and()?;
        while self.peek() == Some("-o") {
            self.pos += 1;
            let r = self.and()?;
            v = v || r;
        }
        Ok(v)
    }

    fn and(&mut self) -> TResult {
        let mut v = self.not()?;
        while self.peek() == Some("-a") {
            self.pos += 1;
            let r = self.not()?;
            v = v && r;
        }
        Ok(v)
    }

    fn not(&mut self) -> TResult {
        if self.peek() == Some("!") && self.pos + 1 < self.args.len() {
            self.pos += 1;
            return Ok(!self.not()?);
        }
        self.primary()
    }

    fn primary(&mut self) -> TResult {
        let Some(a) = self.peek().map(|s| s.to_string()) else {
            return Err("argument expected".to_string());
        };
        let rest = self.args.len() - self.pos;
        if a == "(" {
            self.pos += 1;
            let v = self.or()?;
            if self.peek() != Some(")") {
                return Err("`)' expected".to_string());
            }
            self.pos += 1;
            return Ok(v);
        }
        if rest >= 3 && is_binary(&self.args[self.pos + 1]) {
            let (x, op, y) = (
                self.args[self.pos].clone(),
                self.args[self.pos + 1].clone(),
                self.args[self.pos + 2].clone(),
            );
            self.pos += 3;
            return binary(&x, &op, &y);
        }
        if rest >= 2 && is_unary(&a) {
            let arg = self.args[self.pos + 1].clone();
            self.pos += 2;
            return unary(&a, &arg);
        }
        self.pos += 1;
        Ok(!a.is_empty())
    }
}

fn eval_test(args: &[String]) -> TResult {
    let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    // POSIX: the meaning depends on the number of arguments.
    match a.len() {
        0 => Ok(false),
        1 => Ok(!a[0].is_empty()),
        2 => {
            if a[0] == "!" {
                Ok(a[1].is_empty())
            } else if is_unary(a[0]) {
                unary(a[0], a[1])
            } else {
                Err(format!("{}: unary operator expected", a[0]))
            }
        }
        3 => {
            if is_binary(a[1]) && a[1] != "=~" {
                binary(a[0], a[1], a[2])
            } else if a[1] == "-a" || a[1] == "-o" {
                let (x, y) = (!a[0].is_empty(), !a[2].is_empty());
                Ok(if a[1] == "-a" { x && y } else { x || y })
            } else if a[0] == "!" {
                Ok(!eval_test(&args[1..])?)
            } else if a[0] == "(" && a[2] == ")" {
                Ok(!a[1].is_empty())
            } else {
                Err(format!("{}: binary operator expected", a[1]))
            }
        }
        4 if a[0] == "!" => Ok(!eval_test(&args[1..])?),
        4 if a[0] == "(" && a[3] == ")" => eval_test(&args[1..3]),
        _ => {
            let mut p = TestParser { args, pos: 0 };
            let v = p.or()?;
            if p.pos != args.len() {
                return Err(format!("{}: too many arguments", args[p.pos]));
            }
            Ok(v)
        }
    }
}

/// `test` and `[`. Returns 0 (true), 1 (false), or 2 (error).
pub fn builtin_test(words: &[String]) -> i32 {
    let name = words[0].as_str();
    let mut args = &words[1..];
    if name == "[" {
        if args.last().map(|s| s.as_str()) != Some("]") {
            err_line("meowsh: [: missing `]'");
            return 2;
        }
        args = &args[..args.len() - 1];
    }
    match eval_test(args) {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(e) => {
            builtin_err(name, &e);
            2
        }
    }
}

// ---- [[ ]] -----------------------------------------------------------------

struct DbParser<'a> {
    toks: &'a [String],
    pos: usize,
}

fn exp(word: &str) -> Result<String, String> {
    expand_string(word).map_err(|_| String::new())
}

impl DbParser<'_> {
    fn peek(&self) -> Option<&str> {
        self.toks.get(self.pos).map(|s| s.as_str())
    }

    fn or(&mut self, run: bool) -> TResult {
        let mut v = self.and(run)?;
        while self.peek() == Some("||") {
            self.pos += 1;
            let r = self.and(run && !v)?;
            v = v || r;
        }
        Ok(v)
    }

    fn and(&mut self, run: bool) -> TResult {
        let mut v = self.not(run)?;
        while self.peek() == Some("&&") {
            self.pos += 1;
            let r = self.not(run && v)?;
            v = v && r;
        }
        Ok(v)
    }

    fn not(&mut self, run: bool) -> TResult {
        if self.peek() == Some("!") {
            self.pos += 1;
            return Ok(!self.not(run)?);
        }
        self.primary(run)
    }

    fn primary(&mut self, run: bool) -> TResult {
        let Some(t) = self.peek().map(|s| s.to_string()) else {
            return Err("unexpected end of expression".to_string());
        };
        if t == "(" {
            self.pos += 1;
            let v = self.or(run)?;
            if self.peek() != Some(")") {
                return Err("expected `)'".to_string());
            }
            self.pos += 1;
            return Ok(v);
        }
        let next = self.toks.get(self.pos + 1).map(|s| s.as_str());
        if let Some(op) = next {
            if is_binary(op) && self.pos + 2 < self.toks.len() {
                let op = op.to_string();
                let lhs = self.toks[self.pos].clone();
                let mut rhs = self.toks[self.pos + 2].clone();
                self.pos += 3;
                // zsh: a pattern with groups, `== *.(a|b)`, arrives split
                // at the parentheses; rejoin the pieces.
                if matches!(op.as_str(), "=" | "==" | "!=") {
                    let mut depth = (rhs == "(") as i32;
                    while let Some(t) = self.toks.get(self.pos) {
                        if depth == 0 && t != "(" {
                            break;
                        }
                        depth += (t == "(") as i32 - (t == ")") as i32;
                        rhs.push_str(t);
                        self.pos += 1;
                        if depth == 0 {
                            // Text glued after the group, as in `(a|b)*`.
                            if let Some(t) = self.toks.get(self.pos) {
                                if !matches!(t.as_str(), "&&" | "||" | "(" | ")" | "]]") {
                                    rhs.push_str(t);
                                    self.pos += 1;
                                }
                            }
                        }
                    }
                    if depth != 0 {
                        return Err("expected `)'".to_string());
                    }
                }
                if !run {
                    return Ok(false);
                }
                let l = exp(&lhs)?;
                return match op.as_str() {
                    "=" | "==" | "!=" => {
                        let pat = expand_pattern(&rhs).map_err(|_| String::new())?;
                        let m = crate::pattern::matches(&pat, &l);
                        Ok(if op == "!=" { !m } else { m })
                    }
                    _ => binary(&l, &op, &exp(&rhs)?),
                };
            }
        }
        if is_unary(&t) && next.is_some() && !matches!(next, Some("&&") | Some("||") | Some(")")) {
            let arg = self.toks[self.pos + 1].clone();
            self.pos += 2;
            if !run {
                return Ok(false);
            }
            return unary(&t, &exp(&arg)?);
        }
        self.pos += 1;
        if !run {
            return Ok(false);
        }
        Ok(!exp(&t)?.is_empty())
    }
}

/// Evaluates `[[ ... ]]` from its raw words.
pub fn eval_dbracket(words: &[String]) -> i32 {
    let mut p = DbParser {
        toks: words,
        pos: 0,
    };
    let result = p.or(true).and_then(|v| {
        if p.pos != words.len() {
            Err(format!("syntax error near `{}'", words[p.pos]))
        } else {
            Ok(v)
        }
    });
    match result {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(e) => {
            if !e.is_empty() {
                err_line(&format!("meowsh: [[: {}", e));
            }
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(args: &[&str]) -> i32 {
        let mut w = vec!["test".to_string()];
        w.extend(args.iter().map(|s| s.to_string()));
        builtin_test(&w)
    }

    #[test]
    fn posix_argument_counts() {
        assert_eq!(t(&[]), 1);
        assert_eq!(t(&["x"]), 0);
        assert_eq!(t(&[""]), 1);
        assert_eq!(t(&["-n"]), 0);
        assert_eq!(t(&["!", ""]), 0);
        assert_eq!(t(&["a", "=", "a"]), 0);
        assert_eq!(t(&["!", "a", "=", "b"]), 0);
        assert_eq!(t(&["1", "-lt", "2", "-a", "x", "!=", "y"]), 0);
        assert_eq!(t(&["abc", "-eq", "1"]), 2);
    }
}
