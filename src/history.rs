//! zsh-style history expansion (BANG_HIST): `!!`, `!n`, `!-n`, `!str`,
//! `!?str?`, word designators (`:0`, `:$`, `:2-4`, `!$`, `!*`, ...) and
//! modifiers (`:h`, `:s/a/b/`, ...), plus `^old^new` at the start.

/// Expands history references in an input line. `history` holds the
/// earlier lines, oldest first; event numbers count from 1. Returns the
/// new line, or an error message.
pub fn expand(line: &str, history: &[String]) -> Result<String, String> {
    let c: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    // `^old^new^` is `!!:s/old/new/`.
    if c.first() == Some(&'^') {
        let rest: String = c[1..].iter().collect();
        let mut parts = rest.splitn(3, '^');
        let old = parts.next().unwrap_or("").to_string();
        let new = parts
            .next()
            .unwrap_or("")
            .trim_end_matches('\n')
            .to_string();
        let tail = parts.next().unwrap_or("").to_string();
        let last = history.last().ok_or("no previous command")?;
        let replaced = last.replacen(&old, &new, 1);
        return Ok(format!(
            "{}{}",
            replaced.trim_end_matches('\n'),
            if tail.is_empty() {
                "\n".to_string()
            } else {
                tail
            }
        ));
    }
    let mut in_squote = false;
    while i < c.len() {
        let ch = c[i];
        if ch == '\\' && c.get(i + 1) == Some(&'!') {
            out.push('!');
            i += 2;
            continue;
        }
        if ch == '\'' {
            in_squote = !in_squote;
        }
        if ch != '!' || in_squote {
            out.push(ch);
            i += 1;
            continue;
        }
        // `!` before a blank, `=` or `(` is literal, and so is one with
        // neither event nor word designator (`&!;`, `!}`, `!'`).
        let Some(&n) = c.get(i + 1) else {
            out.push('!');
            i += 1;
            continue;
        };
        if matches!(
            n,
            ' ' | '\t' | '\n' | '=' | '(' | ';' | '}' | '\'' | '"' | '`'
        ) {
            out.push('!');
            i += 1;
            continue;
        }
        i += 1;
        // The event.
        let braced = c.get(i) == Some(&'{');
        if braced {
            i += 1;
        }
        let Some(&first) = c.get(i).filter(|&&ch| ch != '\n') else {
            return Err("'}' expected".to_string());
        };
        let event: String = match first {
            '!' => {
                i += 1;
                last(history)?
            }
            '#' => {
                i += 1;
                out.clone()
            }
            '-' | '0'..='9' => {
                let start = i;
                i += 1;
                while c.get(i).is_some_and(char::is_ascii_digit) {
                    i += 1;
                }
                let n: i64 = c[start..i]
                    .iter()
                    .collect::<String>()
                    .parse()
                    .map_err(|_| "bad history event".to_string())?;
                let idx = if n < 0 {
                    history.len() as i64 + n
                } else {
                    n - 1
                };
                history
                    .get(usize::try_from(idx).map_err(|_| no_event(n))?)
                    .cloned()
                    .ok_or_else(|| no_event(n))?
            }
            '?' => {
                i += 1;
                let start = i;
                while i < c.len() && c[i] != '?' && c[i] != '\n' {
                    i += 1;
                }
                let pat: String = c[start..i].iter().collect();
                if c.get(i) == Some(&'?') {
                    i += 1;
                }
                history
                    .iter()
                    .rev()
                    .find(|h| h.contains(&pat))
                    .cloned()
                    .ok_or_else(|| format!("no such event: {}", pat))?
            }
            // `!$`, `!^`, `!*`, `!:...`: words of the last command.
            '$' | '^' | '*' | ':' | '%' => last(history)?,
            _ => {
                let start = i;
                while i < c.len()
                    && !matches!(
                        c[i],
                        ' ' | '\t'
                            | '\n'
                            | ':'
                            | '^'
                            | '$'
                            | '*'
                            | '%'
                            | '}'
                            | ';'
                            | '&'
                            | '|'
                            | '"'
                            | '\''
                    )
                {
                    i += 1;
                }
                let pfx: String = c[start..i].iter().collect();
                history
                    .iter()
                    .rev()
                    .find(|h| h.starts_with(&pfx))
                    .cloned()
                    .ok_or_else(|| format!("event not found: {}", pfx))?
            }
        };
        let event = event.trim_end_matches('\n').to_string();
        // Word designators.
        let words = crate::expand::split_shell_words(&event);
        let mut text = event.clone();
        let designator = match c.get(i) {
            Some(':')
                if c.get(i + 1)
                    .is_some_and(|d| d.is_ascii_digit() || matches!(d, '$' | '^' | '*' | '-')) =>
            {
                i += 1;
                true
            }
            Some('$' | '^' | '*') => true,
            _ => false,
        };
        if designator {
            let pick = |k: &str, i: &mut usize| -> Option<usize> {
                let _ = k;
                match c.get(*i) {
                    Some('$') => {
                        *i += 1;
                        Some(words.len().saturating_sub(1))
                    }
                    Some('^') => {
                        *i += 1;
                        Some(1)
                    }
                    Some(d) if d.is_ascii_digit() => {
                        let start = *i;
                        while c.get(*i).is_some_and(char::is_ascii_digit) {
                            *i += 1;
                        }
                        c[start..*i].iter().collect::<String>().parse().ok()
                    }
                    _ => None,
                }
            };
            let (from, to);
            if c.get(i) == Some(&'*') {
                i += 1;
                from = 1;
                to = words.len().saturating_sub(1);
            } else {
                from = if c.get(i) == Some(&'-') {
                    0
                } else {
                    pick("from", &mut i).unwrap_or(0)
                };
                if c.get(i) == Some(&'-') {
                    i += 1;
                    to = match pick("to", &mut i) {
                        Some(t) => t,
                        None if c.get(i) == Some(&'*') || c.get(i) == Some(&'$') => {
                            i += 1;
                            words.len().saturating_sub(1)
                        }
                        // `x-`: up to but not including the last word.
                        None => words.len().saturating_sub(2),
                    };
                } else if c.get(i) == Some(&'*') {
                    i += 1;
                    to = words.len().saturating_sub(1);
                } else {
                    to = from;
                }
            }
            if from >= words.len() || to >= words.len() {
                return Err("no such word in event".to_string());
            }
            text = words[from..=to.max(from)].join(" ");
        }
        // Modifiers apply to each word chosen.
        if c.get(i) == Some(&':') {
            // As zsh reads history modifiers, `\\` is one backslash.
            let raw: String = c[i..].iter().collect();
            let mods: Vec<char> = raw.replace("\\\\", "\\").chars().collect();
            let (res, used_m) = crate::expand::history_modifiers(&text, &mods);
            // Map the length used back onto the original text.
            let consumed: String = mods[..used_m].iter().collect();
            let used = if used_m == 0 {
                0
            } else {
                let mut k = 0;
                let mut seen = String::new();
                while k < c.len() - i && seen.replace("\\\\", "\\") != consumed {
                    seen.push(c[i + k]);
                    k += 1;
                }
                k
            };
            if used > 0 {
                text = res;
                i += used;
            }
        }
        if braced {
            // zsh wants the brace closed: `!{!` alone is an error.
            if c.get(i) != Some(&'}') {
                return Err("'}' expected".to_string());
            }
            i += 1;
        }
        out.push_str(&text);
    }
    Ok(out)
}

fn last(history: &[String]) -> Result<String, String> {
    history
        .last()
        .cloned()
        .ok_or_else(|| "no previous command".to_string())
}

fn no_event(n: i64) -> String {
    format!("no such event: {}", n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_and_words() {
        let _g = crate::zsh_test_guard();
        let h = vec!["print one two three".to_string()];
        assert_eq!(
            expand("echo !!\n", &h).unwrap(),
            "echo print one two three\n"
        );
        assert_eq!(expand("echo !:$ !:1\n", &h).unwrap(), "echo three one\n");
        assert_eq!(expand("echo !1:2-3\n", &h).unwrap(), "echo two three\n");
        assert_eq!(expand("echo '!!' a\\!b\n", &h).unwrap(), "echo '!!' a!b\n");
    }

    #[test]
    fn an_unclosed_brace_is_an_error_not_a_panic() {
        let _g = crate::zsh_test_guard();
        let h = vec!["echo hi".to_string()];
        assert_eq!(expand("echo !{", &h), Err("'}' expected".to_string()));
        assert_eq!(expand("echo !{\n", &h), Err("'}' expected".to_string()));
    }
}
