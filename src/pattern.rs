//! Shell pattern matching (`*`, `?`, `[...]`, backslash escapes) used by
//! `case`, `[[ == ]]`, and the `${x#pat}` family. Patterns come from the
//! expander with quoted characters already backslash-escaped.

#[derive(Debug, Clone)]
enum Item {
    Char(char),
    Any,
    Star,
    Class { negated: bool, set: Vec<ClassItem> },
}

#[derive(Debug, Clone)]
enum ClassItem {
    Char(char),
    Range(char, char),
    Named(String),
}

fn compile(pattern: &str) -> Vec<Item> {
    let p: Vec<char> = pattern.chars().collect();
    let mut items = Vec::new();
    let mut i = 0;
    while i < p.len() {
        match p[i] {
            '\\' if i + 1 < p.len() => {
                items.push(Item::Char(p[i + 1]));
                i += 2;
            }
            '*' => {
                if !matches!(items.last(), Some(Item::Star)) {
                    items.push(Item::Star);
                }
                i += 1;
            }
            '?' => {
                items.push(Item::Any);
                i += 1;
            }
            '[' => match parse_class(&p, i) {
                Some((item, next)) => {
                    items.push(item);
                    i = next;
                }
                None => {
                    items.push(Item::Char('['));
                    i += 1;
                }
            },
            c => {
                items.push(Item::Char(c));
                i += 1;
            }
        }
    }
    items
}

// Parses a bracket expression starting at `p[start] == '['`.
fn parse_class(p: &[char], start: usize) -> Option<(Item, usize)> {
    let mut i = start + 1;
    let mut negated = false;
    if i < p.len() && (p[i] == '!' || p[i] == '^') {
        negated = true;
        i += 1;
    }
    let mut set = Vec::new();
    let mut first = true;
    while i < p.len() {
        let c = p[i];
        if c == ']' && !first {
            return Some((Item::Class { negated, set }, i + 1));
        }
        first = false;
        if c == '[' && i + 1 < p.len() && p[i + 1] == ':' {
            if let Some(end) =
                (i + 2..p.len().saturating_sub(1)).find(|&k| p[k] == ':' && p[k + 1] == ']')
            {
                let name: String = p[i + 2..end].iter().collect();
                set.push(ClassItem::Named(name));
                i = end + 2;
                continue;
            }
        }
        let lo = if c == '\\' && i + 1 < p.len() {
            i += 1;
            p[i]
        } else {
            c
        };
        if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
            let mut hi = p[i + 2];
            let mut adv = 3;
            if hi == '\\' && i + 3 < p.len() {
                hi = p[i + 3];
                adv = 4;
            }
            set.push(ClassItem::Range(lo, hi));
            i += adv;
        } else {
            set.push(ClassItem::Char(lo));
            i += 1;
        }
    }
    None
}

fn class_matches(set: &[ClassItem], c: char) -> bool {
    set.iter().any(|item| match item {
        ClassItem::Char(x) => *x == c,
        ClassItem::Range(lo, hi) => *lo <= c && c <= *hi,
        ClassItem::Named(name) => match name.as_str() {
            "alpha" => c.is_alphabetic(),
            "digit" => c.is_ascii_digit(),
            "alnum" => c.is_alphanumeric(),
            "upper" => c.is_uppercase(),
            "lower" => c.is_lowercase(),
            "space" => c.is_whitespace(),
            "blank" => c == ' ' || c == '\t',
            "punct" => c.is_ascii_punctuation(),
            "print" => !c.is_control(),
            "graph" => !c.is_control() && !c.is_whitespace(),
            "cntrl" => c.is_control(),
            "xdigit" => c.is_ascii_hexdigit(),
            _ => false,
        },
    })
}

fn item_matches(item: &Item, c: char) -> bool {
    match item {
        Item::Char(x) => *x == c,
        Item::Any => true,
        Item::Class { negated, set } => class_matches(set, c) != *negated,
        Item::Star => true,
    }
}

fn match_items(items: &[Item], s: &[char]) -> bool {
    // Iterative matcher with single-star backtracking.
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < items.len() {
            if let Item::Star = items[pi] {
                star = Some((pi, si));
                pi += 1;
                continue;
            }
            if item_matches(&items[pi], s[si]) {
                pi += 1;
                si += 1;
                continue;
            }
        }
        match star {
            Some((sp, ss)) => {
                pi = sp + 1;
                si = ss + 1;
                star = Some((sp, ss + 1));
            }
            None => return false,
        }
    }
    while pi < items.len() && matches!(items[pi], Item::Star) {
        pi += 1;
    }
    pi == items.len()
}

pub struct Pattern {
    items: Vec<Item>,
}

impl Pattern {
    pub fn new(pattern: &str) -> Self {
        Pattern {
            items: compile(pattern),
        }
    }

    pub fn matches_chars(&self, s: &[char]) -> bool {
        match_items(&self.items, s)
    }

    pub fn matches(&self, s: &str) -> bool {
        let chars: Vec<char> = s.chars().collect();
        self.matches_chars(&chars)
    }
}

pub fn matches(pattern: &str, s: &str) -> bool {
    Pattern::new(pattern).matches(s)
}

/// True if the pattern contains an unescaped glob metacharacter.
pub fn has_meta(pattern: &str) -> bool {
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '*' | '?' | '[' => return true,
            _ => {}
        }
    }
    false
}

/// Removes backslash escapes from a pattern, yielding its literal text.
pub fn unescape(pattern: &str) -> String {
    let mut out = String::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// `${v#p}` / `${v##p}`: removes the shortest/longest matching prefix.
pub fn strip_prefix(value: &str, pattern: &str, longest: bool) -> String {
    let pat = Pattern::new(pattern);
    let chars: Vec<char> = value.chars().collect();
    let range: Box<dyn Iterator<Item = usize>> = if longest {
        Box::new((0..=chars.len()).rev())
    } else {
        Box::new(0..=chars.len())
    };
    for i in range {
        if pat.matches_chars(&chars[..i]) {
            return chars[i..].iter().collect();
        }
    }
    value.to_string()
}

/// `${v%p}` / `${v%%p}`: removes the shortest/longest matching suffix.
pub fn strip_suffix(value: &str, pattern: &str, longest: bool) -> String {
    let pat = Pattern::new(pattern);
    let chars: Vec<char> = value.chars().collect();
    let range: Box<dyn Iterator<Item = usize>> = if longest {
        Box::new(0..=chars.len())
    } else {
        Box::new((0..=chars.len()).rev())
    };
    for i in range {
        if pat.matches_chars(&chars[i..]) {
            return chars[..i].iter().collect();
        }
    }
    value.to_string()
}

/// `${v/p/r}` and friends: replaces the longest match at the leftmost
/// matching position (or every match when `global`).
pub fn substitute(
    value: &str,
    pattern: &str,
    repl: &str,
    global: bool,
    anchor_start: bool,
    anchor_end: bool,
) -> String {
    let pat = Pattern::new(pattern);
    let chars: Vec<char> = value.chars().collect();
    let n = chars.len();
    if anchor_start {
        for end in (0..=n).rev() {
            if pat.matches_chars(&chars[..end]) {
                return format!("{}{}", repl, chars[end..].iter().collect::<String>());
            }
        }
        return value.to_string();
    }
    if anchor_end {
        for start in 0..=n {
            if pat.matches_chars(&chars[start..]) {
                return format!("{}{}", chars[..start].iter().collect::<String>(), repl);
            }
        }
        return value.to_string();
    }
    if pattern.is_empty() {
        return value.to_string();
    }
    let mut out = String::new();
    let mut i = 0;
    let mut replaced = false;
    while i < n {
        if !(replaced && !global) {
            if let Some(end) = (i + 1..=n).rev().find(|&e| pat.matches_chars(&chars[i..e])) {
                out.push_str(repl);
                i = end;
                replaced = true;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_matching() {
        assert!(matches("f*", "foo"));
        assert!(!matches("f*", "bar"));
        assert!(matches("?a?", "bar"));
        assert!(matches("[a-c]x", "bx"));
        assert!(matches("[!a-c]x", "dx"));
        assert!(matches("[[:digit:]]*", "1abc"));
        assert!(matches("\\*", "*"));
        assert!(!matches("\\*", "a"));
        assert!(matches("*.rs", "main.rs"));
        assert!(matches("é*", "école"));
    }

    #[test]
    fn prefix_suffix() {
        assert_eq!(strip_prefix("a/b/c", "*/", false), "b/c");
        assert_eq!(strip_prefix("a/b/c", "*/", true), "c");
        assert_eq!(strip_suffix("a.tar.gz", ".*", false), "a.tar");
        assert_eq!(strip_suffix("a.tar.gz", ".*", true), "a");
        assert_eq!(strip_suffix("héllo", "l*", true), "hé");
    }

    #[test]
    fn substitution() {
        assert_eq!(substitute("a-b-c", "-", "+", false, false, false), "a+b-c");
        assert_eq!(substitute("a-b-c", "-", "+", true, false, false), "a+b+c");
        assert_eq!(substitute("abc", "a", "X", false, true, false), "Xbc");
        assert_eq!(substitute("abc", "c", "X", false, false, true), "abX");
        assert_eq!(substitute("aXbXc", "X*", "_", false, false, false), "a_");
    }
}
