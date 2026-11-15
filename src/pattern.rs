//! Shell pattern matching (`*`, `?`, `[...]`, zsh `(a|b)` groups,
//! backslash escapes) used by
//! `case`, `[[ == ]]`, and the `${x#pat}` family. Patterns come from the
//! expander with quoted characters already backslash-escaped.

#[derive(Debug, Clone)]
enum Item {
    Char(char),
    Any,
    Star,
    Class { negated: bool, set: Vec<ClassItem> },
    // `(alt1|alt2)`: matches any alternative; with (#b), the slot that
    // records what it matched.
    Group(Vec<Vec<Item>>, Option<usize>),
    // zsh `<lo-hi>`: a run of digits whose value is in range.
    Range(Option<u64>, Option<u64>),
    // EXTENDED_GLOB `x#` (min 0), `x##` (min 1), `x(#cN,M)`.
    Repeat(Box<Item>, usize, Option<usize>),
    // EXTENDED_GLOB `^pat`: anything the rest does not match.
    Not(Vec<Item>),
    // `(#s)` / `(#e)`: zero-width anchors. Patterns are matched against
    // whole strings, so they only mark where a branch may end.
    Anchor,
}

// Compile-time state: EXTENDED_GLOB, (#i) case folding, (#b) capturing
// groups and (#m) whole-match parameters.
struct Ctx {
    ext: bool,
    icase: bool,
    backref: bool,
    groups: usize,
    matchref: bool,
}

#[derive(Debug, Clone)]
enum ClassItem {
    Char(char),
    Range(char, char),
    Named(String),
}

/// With EXTENDED_GLOB, a glob `a~b`: (a, b), b being matched against
/// whole paths.
pub fn glob_exclusion(pattern: &str) -> Option<(String, String)> {
    if !crate::options::quick("extendedglob") {
        return None;
    }
    let p: Vec<char> = pattern.chars().collect();
    let k = split_exclusion(&p)?;
    Some((p[..k].iter().collect(), p[k + 1..].iter().collect()))
}

// Splits an EXTENDED_GLOB `a~b` at the top-level `~`.
fn split_exclusion(p: &[char]) -> Option<usize> {
    let mut depth = 0;
    let mut i = 0;
    while i < p.len() {
        match p[i] {
            '\\' => i += 1,
            '(' => depth += 1,
            ')' => depth -= 1,
            '[' => {
                if let Some((_, next)) = parse_class(p, i) {
                    i = next;
                    continue;
                }
            }
            '~' if depth == 0 && i > 0 => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn compile(p: &[char], cx: &mut Ctx) -> Vec<Item> {
    let mut i = 0;
    let mut alts = compile_seq(p, &mut i, false, cx);
    alts.pop().unwrap_or_default()
}

// Index just past the `)` closing the group opened at `p[open]`.
fn group_end(p: &[char], open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut i = open;
    while i < p.len() {
        match p[i] {
            '\\' => i += 1,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// Compiles items up to the end, or inside a group up to its `)`, split
// into alternatives at `|`. A `(` without a matching `)` is literal.
fn compile_seq(p: &[char], i: &mut usize, nested: bool, cx: &mut Ctx) -> Vec<Vec<Item>> {
    let mut alts = Vec::new();
    let mut items = Vec::new();
    let mut negate = false;
    let finish = |items: Vec<Item>, negate: bool| {
        if negate {
            vec![Item::Not(items)]
        } else {
            items
        }
    };
    while *i < p.len() {
        let i0 = *i;
        match p[i0] {
            ')' if nested => {
                *i += 1;
                break;
            }
            '|' if nested => {
                alts.push(finish(std::mem::take(&mut items), negate));
                negate = false;
                *i += 1;
            }
            '^' if cx.ext && items.is_empty() && !negate => {
                negate = true;
                *i += 1;
            }
            '#' if cx.ext && !items.is_empty() => {
                let last = items.pop().unwrap();
                let min = if p.get(i0 + 1) == Some(&'#') {
                    *i += 1;
                    1
                } else {
                    0
                };
                *i += 1;
                items.push(Item::Repeat(Box::new(last), min, None));
            }
            // (#i), (#b), (#m), (#cN,M) and other globbing flags.
            '(' if cx.ext && p.get(i0 + 1) == Some(&'#') => {
                let mut j = i0 + 2;
                if matches!(p.get(j), Some('s' | 'e')) && p.get(j + 1) == Some(&')') {
                    items.push(Item::Anchor);
                }
                while j < p.len() && p[j] != ')' {
                    match p[j] {
                        'i' | 'l' => cx.icase = true,
                        'I' => cx.icase = false,
                        'b' => cx.backref = true,
                        'B' => cx.backref = false,
                        'm' => cx.matchref = true,
                        'M' => cx.matchref = false,
                        'c' => {
                            let end = (j + 1..p.len()).find(|&k| p[k] == ')').unwrap_or(p.len());
                            let spec: String = p[j + 1..end].iter().collect();
                            let (lo, hi) = match spec.split_once(',') {
                                Some((a, b)) => (a.parse().unwrap_or(0), b.parse().ok()),
                                None => {
                                    let n = spec.parse().unwrap_or(0);
                                    (n, Some(n))
                                }
                            };
                            if let Some(last) = items.pop() {
                                items.push(Item::Repeat(Box::new(last), lo, hi));
                            }
                            j = end;
                            continue;
                        }
                        _ => {}
                    }
                    j += 1;
                }
                *i = (j + 1).min(p.len());
            }
            '<' if crate::lexer::numeric_range_len(p, i0).is_some() => {
                let len = crate::lexer::numeric_range_len(p, i0).unwrap();
                let inner: String = p[i0 + 1..i0 + len - 1].iter().collect();
                let (lo, hi) = inner.split_once('-').unwrap_or(("", ""));
                items.push(Item::Range(lo.parse().ok(), hi.parse().ok()));
                *i += len;
            }
            '(' if group_end(p, i0).is_some() => {
                *i += 1;
                let slot = cx.backref.then(|| {
                    cx.groups += 1;
                    cx.groups - 1
                });
                items.push(Item::Group(compile_seq(p, i, true, cx), slot));
            }
            _ => {
                let one = compile_one(p, i);
                if !(matches!(one, Item::Star) && matches!(items.last(), Some(Item::Star))) {
                    items.push(one);
                }
            }
        }
    }
    alts.push(finish(items, negate));
    alts
}

fn compile_one(p: &[char], i: &mut usize) -> Item {
    let start = *i;
    match p[start] {
        '\\' if start + 1 < p.len() => {
            *i += 2;
            Item::Char(p[start + 1])
        }
        '*' => {
            *i += 1;
            Item::Star
        }
        '?' => {
            *i += 1;
            Item::Any
        }
        '[' => match parse_class(p, start) {
            Some((item, next)) => {
                *i = next;
                item
            }
            None => {
                *i += 1;
                Item::Char('[')
            }
        },
        c => {
            *i += 1;
            Item::Char(c)
        }
    }
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
            // zsh's own classes.
            "IDENT" => crate::lexer::is_name_char(c),
            "IFS" => " \t\n\0".contains(c),
            "IFSSPACE" => " \t\n".contains(c),
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
        Item::Group(..) | Item::Range(..) | Item::Repeat(..) | Item::Not(_) | Item::Anchor => false,
    }
}

fn has_group(items: &[Item]) -> bool {
    items.iter().any(|i| {
        matches!(
            i,
            Item::Group(..) | Item::Range(..) | Item::Repeat(..) | Item::Not(_) | Item::Anchor
        )
    })
}

// Whether `item` matches exactly `chunk`.
fn match_one(item: &Item, chunk: &[char]) -> bool {
    match item {
        Item::Star => true,
        Item::Group(alts, _) => alts.iter().any(|alt| match_items(alt, chunk)),
        Item::Range(..) | Item::Repeat(..) | Item::Not(_) | Item::Anchor => {
            match_rec(std::slice::from_ref(item), chunk)
        }
        it => chunk.len() == 1 && item_matches(it, chunk[0]),
    }
}

fn match_repeat(item: &Item, min: usize, max: Option<usize>, s: &[char], rest: &[Item]) -> bool {
    if min == 0 && match_rec(rest, s) {
        return true;
    }
    if max == Some(0) {
        return false;
    }
    (1..=s.len()).any(|k| {
        match_one(item, &s[..k])
            && match_repeat(
                item,
                min.saturating_sub(1),
                max.map(|m| m - 1),
                &s[k..],
                rest,
            )
    })
}

// Backtracking matcher for patterns containing groups.
fn match_rec(items: &[Item], s: &[char]) -> bool {
    let Some((first, rest)) = items.split_first() else {
        return s.is_empty();
    };
    match first {
        Item::Star => (0..=s.len()).any(|k| match_rec(rest, &s[k..])),
        Item::Group(alts, _) => (0..=s.len())
            .any(|k| alts.iter().any(|alt| match_items(alt, &s[..k])) && match_rec(rest, &s[k..])),
        Item::Range(lo, hi) => {
            let digits = s.iter().take_while(|c| c.is_ascii_digit()).count();
            (1..=digits).any(|k| {
                let text: String = s[..k].iter().collect();
                let in_range = text
                    .parse::<u64>()
                    .is_ok_and(|v| lo.is_none_or(|lo| v >= lo) && hi.is_none_or(|hi| v <= hi));
                in_range && match_rec(rest, &s[k..])
            })
        }
        Item::Repeat(item, min, max) => match_repeat(item, *min, *max, s, rest),
        Item::Anchor => match_rec(rest, s),
        Item::Not(inner) => {
            (0..=s.len()).any(|k| !match_items(inner, &s[..k]) && match_rec(rest, &s[k..]))
        }
        item => !s.is_empty() && item_matches(item, s[0]) && match_rec(rest, &s[1..]),
    }
}

fn match_items(items: &[Item], s: &[char]) -> bool {
    if has_group(items) {
        return match_rec(items, s);
    }
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
    // EXTENDED_GLOB `a~b`: what must not match.
    exclude: Option<Vec<Item>>,
    icase: bool,
    // (#b) capturing groups; (#m) sets MATCH, MBEGIN and MEND.
    groups: usize,
    matchref: bool,
}

impl Pattern {
    pub fn new(pattern: &str) -> Self {
        let p: Vec<char> = pattern.chars().collect();
        let mut cx = Ctx {
            ext: crate::options::quick("extendedglob"),
            icase: false,
            backref: false,
            groups: 0,
            matchref: false,
        };
        let (main, excl) = match split_exclusion(&p).filter(|_| cx.ext) {
            Some(k) => (&p[..k], Some(&p[k + 1..])),
            None => (&p[..], None),
        };
        let mut items = compile(main, &mut cx);
        let mut exclude = excl.map(|e| compile(e, &mut cx));
        if cx.icase {
            fold_items(&mut items);
            if let Some(e) = exclude.as_mut() {
                fold_items(e);
            }
        }
        Pattern {
            items,
            exclude,
            icase: cx.icase,
            groups: cx.groups,
            matchref: cx.matchref,
        }
    }

    /// After a match of `s[b..e]`, sets zsh's `$match`, `$mbegin` and
    /// `$mend` for (#b) groups and `$MATCH`, `$MBEGIN`, `$MEND` for (#m).
    /// Positions count from 1 in the whole of `s`.
    pub fn record(&self, s: &[char], b: usize, e: usize) {
        if self.groups > 0 {
            let folded: Vec<char> = if self.icase {
                s.iter().flat_map(|c| c.to_lowercase()).collect()
            } else {
                s.to_vec()
            };
            let mut caps = vec![None; self.groups];
            cap_rec(&self.items, &folded[b..e], b, &mut caps);
            let (mut m, mut mb, mut me) = (Vec::new(), Vec::new(), Vec::new());
            for c in caps {
                match c {
                    Some((x, y)) => {
                        m.push(s[x..y].iter().collect());
                        mb.push((x + 1).to_string());
                        me.push(y.to_string());
                    }
                    None => {
                        m.push(String::new());
                        mb.push("-1".to_string());
                        me.push("-1".to_string());
                    }
                }
            }
            crate::shell::array_set("match", m);
            crate::shell::array_set("mbegin", mb);
            crate::shell::array_set("mend", me);
        }
        if self.matchref {
            let text: String = s[b..e].iter().collect();
            crate::shell::var_set("MATCH", &text, false);
            crate::shell::var_set("MBEGIN", &(b + 1).to_string(), false);
            crate::shell::var_set("MEND", &e.to_string(), false);
        }
    }

    fn records(&self) -> bool {
        self.groups > 0 || self.matchref
    }

    pub fn matches_chars(&self, s: &[char]) -> bool {
        let folded: Vec<char>;
        let s = if self.icase {
            folded = s.iter().flat_map(|c| c.to_lowercase()).collect();
            &folded[..]
        } else {
            s
        };
        match_items(&self.items, s) && !self.exclude.as_ref().is_some_and(|e| match_items(e, s))
    }

    pub fn matches(&self, s: &str) -> bool {
        let chars: Vec<char> = s.chars().collect();
        self.matches_chars(&chars)
    }
}

fn fold_items(items: &mut [Item]) {
    for it in items.iter_mut() {
        match it {
            Item::Char(c) => *c = c.to_lowercase().next().unwrap_or(*c),
            Item::Group(alts, _) => alts.iter_mut().for_each(|a| fold_items(a)),
            Item::Repeat(inner, ..) => fold_items(std::slice::from_mut(inner.as_mut())),
            Item::Not(inner) => fold_items(inner),
            Item::Class { set, .. } => {
                for c in set.iter_mut() {
                    match c {
                        ClassItem::Char(x) => *x = x.to_lowercase().next().unwrap_or(*x),
                        ClassItem::Range(a, b) => {
                            *a = a.to_lowercase().next().unwrap_or(*a);
                            *b = b.to_lowercase().next().unwrap_or(*b);
                        }
                        ClassItem::Named(_) => {}
                    }
                }
            }
            _ => {}
        }
    }
}

pub fn matches(pattern: &str, s: &str) -> bool {
    let p = Pattern::new(pattern);
    let chars: Vec<char> = s.chars().collect();
    let hit = p.matches_chars(&chars);
    if hit && p.records() {
        p.record(&chars, 0, chars.len());
    }
    hit
}

// The matcher again, recording (#b) group spans (offsets from `off`):
// greedy like zsh, so a group takes as much as the rest allows.
type Caps = Vec<Option<(usize, usize)>>;

fn cap_rec(items: &[Item], s: &[char], off: usize, caps: &mut Caps) -> bool {
    let Some((first, rest)) = items.split_first() else {
        return s.is_empty();
    };
    match first {
        Item::Star => (0..=s.len())
            .rev()
            .any(|k| cap_rec(rest, &s[k..], off + k, caps)),
        Item::Group(alts, slot) => {
            for k in (0..=s.len()).rev() {
                for alt in alts {
                    let saved = caps.clone();
                    if cap_rec(alt, &s[..k], off, caps) && cap_rec(rest, &s[k..], off + k, caps) {
                        if let Some(n) = slot {
                            caps[*n] = Some((off, off + k));
                        }
                        return true;
                    }
                    *caps = saved;
                }
            }
            false
        }
        Item::Repeat(item, min, max) => cap_repeat(item, *min, *max, s, off, rest, caps),
        Item::Anchor => cap_rec(rest, s, off, caps),
        Item::Range(..) | Item::Not(_) => (0..=s.len()).rev().any(|k| {
            match_rec(std::slice::from_ref(first), &s[..k]) && cap_rec(rest, &s[k..], off + k, caps)
        }),
        item => !s.is_empty() && item_matches(item, s[0]) && cap_rec(rest, &s[1..], off + 1, caps),
    }
}

fn cap_repeat(
    item: &Item,
    min: usize,
    max: Option<usize>,
    s: &[char],
    off: usize,
    rest: &[Item],
    caps: &mut Caps,
) -> bool {
    if max != Some(0) {
        for k in (1..=s.len()).rev() {
            let saved = caps.clone();
            if cap_rec(std::slice::from_ref(item), &s[..k], off, caps)
                && cap_repeat(
                    item,
                    min.saturating_sub(1),
                    max.map(|m| m - 1),
                    &s[k..],
                    off + k,
                    rest,
                    caps,
                )
            {
                return true;
            }
            *caps = saved;
        }
    }
    min == 0 && cap_rec(rest, s, off, caps)
}

/// True if the pattern contains an unescaped glob metacharacter: `*`,
/// `?`, `[`, a `(...)` group, or a `<n-m>` range.
pub fn has_meta(pattern: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < p.len() {
        match p[i] {
            '\\' => i += 1,
            '*' | '?' => return true,
            '[' if parse_class(&p, i).is_some() => return true,
            '(' if group_end(&p, i).is_some() => return true,
            '<' if crate::lexer::numeric_range_len(&p, i).is_some() => return true,
            '#' | '^' | '~' if crate::options::quick("extendedglob") => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// Backslash-escapes pattern characters so `s` matches only itself.
pub fn escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "*?[]()|<>^#~\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
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

/// Flags for `Pattern::getmatch`, as zsh's SUB_* (Src/glob.c).
#[derive(Default, Clone, Copy, Debug)]
pub struct SubFlags {
    pub end: bool,     // match at the end (`%`)
    pub long: bool,    // longest match (`##`, `%%`, `/`)
    pub substr: bool,  // match substrings, not just at an end ((S), `/`)
    pub start: bool,   // anchored at the start (`/#`)
    pub global: bool,  // all matches (`//`)
    pub all: bool,     // the whole string must match (`:/`)
    pub matched: bool, // (M): the matched part
    pub rest: bool,    // (R): the rest (the default)
    pub bind: bool,    // (B): start index
    pub eind: bool,    // (E): end index
    pub len: bool,     // (N): match length
}

impl Pattern {
    fn longest_at(&self, s: &[char], t: usize) -> Option<usize> {
        (t..=s.len()).rev().find(|&e| self.matches_chars(&s[t..e]))
    }

    fn shortest_at(&self, s: &[char], t: usize) -> Option<usize> {
        (t..=s.len()).find(|&e| self.matches_chars(&s[t..e]))
    }

    // Builds the result for a match of s[b..e] (zsh's get_match_ret).
    fn match_ret(s: &[char], b: usize, e: usize, fl: &SubFlags, repl: Option<&str>) -> String {
        let mut out = String::new();
        if fl.matched {
            out.extend(&s[b..e]);
        }
        if fl.rest {
            if fl.matched {
                out.push(' ');
            }
            out.extend(&s[..b]);
            if let Some(r) = repl {
                out.push_str(r);
            }
            out.extend(&s[e..]);
        }
        let mut nums = Vec::new();
        if fl.bind {
            nums.push((b + 1).to_string());
        }
        if fl.eind {
            nums.push((e + 1).to_string());
        }
        if fl.len {
            nums.push((e - b).to_string());
        }
        if !nums.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&nums.join(" "));
        }
        out
    }

    /// zsh's igetmatch: finds the `n`th match (1-based) of the pattern in
    /// `value` as `fl` describes and returns the requested parts, with
    /// `repl` substituted for the match in the rest. No match gives the
    /// whole value (or the empty match's parts).
    /// `repl` is called for each replacement after the match parameters
    /// are set, so it may refer to `$MATCH` or `$match`.
    pub fn getmatch(
        &self,
        value: &str,
        mut fl: SubFlags,
        n: usize,
        repl: Option<&dyn Fn() -> String>,
    ) -> String {
        let s: Vec<char> = value.chars().collect();
        let len = s.len();
        let mut n = n.max(1) as isize;
        if !(fl.matched || fl.rest || fl.bind || fl.eind || fl.len) {
            fl.rest = true;
        }
        if fl.start && fl.end {
            fl.long = true;
        }
        let ret = |b, e| {
            if self.records() {
                self.record(&s, b, e);
            }
            let r = repl.map(|f| f());
            Self::match_ret(&s, b, e, &fl, r.as_deref())
        };
        if fl.all {
            return if self.matches_chars(&s) {
                ret(0, len)
            } else {
                Self::match_ret(&s, 0, 0, &fl, None)
            };
        }
        let empty_ok = || self.matches_chars(&[]);
        match (fl.end, fl.long, fl.substr) {
            (false, _, false) => {
                if let Some(mut e) = self.longest_at(&s, 0) {
                    if !fl.long {
                        e = self.shortest_at(&s, 0).unwrap_or(e);
                    }
                    return ret(0, e);
                }
            }
            (true, false, false) => {
                for t in (0..=len).rev() {
                    if self.matches_chars(&s[t..]) {
                        return ret(t, len);
                    }
                }
            }
            (true, true, false) => {
                for t in 0..=len {
                    if self.matches_chars(&s[t..]) {
                        return ret(t, len);
                    }
                    if fl.start {
                        break;
                    }
                }
            }
            (false, long, true) => {
                if !long && !fl.global && empty_ok() {
                    n -= 1;
                    if n == 0 {
                        return ret(0, 0);
                    }
                }
                let mut chunks: Vec<(usize, usize)> = Vec::new();
                let mut t = 0;
                while t <= len {
                    if let Some(mut mpos) = self.longest_at(&s, t) {
                        if !long {
                            mpos = self.shortest_at(&s, t).unwrap_or(mpos);
                        }
                        n -= 1;
                        if fl.global {
                            if mpos > t || chunks.last().is_none_or(|c| c.1 != t) {
                                chunks.push((t, mpos));
                            }
                            t = if mpos > t { mpos } else { t + 1 };
                            continue;
                        }
                        if n == 0 {
                            return ret(t, mpos);
                        }
                    }
                    t += 1;
                }
                if fl.global && !chunks.is_empty() {
                    let mut out = String::new();
                    let mut i = 0;
                    for (b, e) in chunks {
                        out.extend(&s[i..b]);
                        if self.records() {
                            self.record(&s, b, e);
                        }
                        out.push_str(&repl.map(|f| f()).unwrap_or_default());
                        i = e;
                    }
                    out.extend(&s[i..]);
                    return out;
                }
                if long && !fl.global && empty_ok() {
                    n -= 1;
                    if n == 0 {
                        return ret(0, 0);
                    }
                }
            }
            (true, long, true) => {
                if empty_ok() {
                    n -= 1;
                    if n == 0 {
                        return ret(len, len);
                    }
                }
                for t in (0..len).rev() {
                    if let Some(mut mpos) = self.longest_at(&s, t) {
                        n -= 1;
                        if n == 0 {
                            if !long {
                                mpos = self.shortest_at(&s, t).unwrap_or(mpos);
                            }
                            return ret(t, mpos);
                        }
                    }
                }
                if long && empty_ok() {
                    n -= 1;
                    if n == 0 {
                        return ret(len, len);
                    }
                }
            }
        }
        Self::match_ret(&s, 0, 0, &fl, None)
    }
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
    fn groups_and_alternation() {
        assert!(matches("([1-4]*)", "4.0"));
        assert!(!matches("([1-4]*)", "5.9"));
        assert!(!matches("([1-4]*)", ""));
        assert!(matches("*.(rs|toml)", "Cargo.toml"));
        assert!(matches("(a|b)(c|d)", "bd"));
        assert!(!matches("(a|b)(c|d)", "ab"));
        assert!(matches("a(", "a("));
        assert!(matches("<->", "123"));
        assert!(!matches("<->", "12a"));
        assert!(matches("file<1-3>.txt", "file2.txt"));
        assert!(!matches("file<1-3>.txt", "file4.txt"));
        assert!(matches("\\(x\\)", "(x)"));
    }

    fn gm(v: &str, p: &str, fl: SubFlags, repl: Option<&str>) -> String {
        let f = repl.map(|r| move || r.to_string());
        Pattern::new(p).getmatch(v, fl, 1, f.as_ref().map(|f| f as &dyn Fn() -> String))
    }

    #[test]
    fn prefix_suffix() {
        let long = SubFlags {
            long: true,
            ..Default::default()
        };
        let end = SubFlags {
            end: true,
            ..Default::default()
        };
        let end_long = SubFlags {
            end: true,
            long: true,
            ..Default::default()
        };
        assert_eq!(gm("a/b/c", "*/", SubFlags::default(), None), "b/c");
        assert_eq!(gm("a/b/c", "*/", long, None), "c");
        assert_eq!(gm("a.tar.gz", ".*", end, None), "a.tar");
        assert_eq!(gm("a.tar.gz", ".*", end_long, None), "a");
        assert_eq!(gm("héllo", "l*", end_long, None), "hé");
    }

    #[test]
    fn substitution() {
        let sub = SubFlags {
            long: true,
            substr: true,
            ..Default::default()
        };
        let all = SubFlags {
            global: true,
            ..sub
        };
        let start = SubFlags {
            long: true,
            start: true,
            ..Default::default()
        };
        let end = SubFlags {
            long: true,
            end: true,
            ..Default::default()
        };
        assert_eq!(gm("a-b-c", "-", sub, Some("+")), "a+b-c");
        assert_eq!(gm("a-b-c", "-", all, Some("+")), "a+b+c");
        assert_eq!(gm("abc", "a", start, Some("X")), "Xbc");
        assert_eq!(gm("abc", "c", end, Some("X")), "abX");
        assert_eq!(gm("aXbXc", "X*", sub, Some("_")), "a_");
    }
}
