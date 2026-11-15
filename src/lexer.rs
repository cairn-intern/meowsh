use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Word(String),
    IoNumber(i32),
    // `{name}` right before a redirection operator.
    IoVar(String),
    Op(&'static str),
    // `(( expr ))`, holding the expression text.
    Arith(String),
    Newline,
    Eof,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LexError {
    // The input ended inside a construct; more input may complete it.
    Incomplete,
    Syntax(String),
}

const OPERATORS: &[&str] = &[
    // zsh's clobbering and both-streams forms, longest first.
    "&>>|", "&>>!", ">>&|", ">>&!", ">&|", ">&!", "&>|", "&>!", ">>|", ">>!", ">>&", ">!", "&>>",
    "<<<", "<<-", "&&", "||", "|&", ";;", ";&", ";|", "<<", ">>", "<&", ">&", "<>", ">|", "&>",
    "&|", "&!", "|", "&", ";", "<", ">", "(", ")",
];

fn is_operator_start(c: char) -> bool {
    matches!(c, '|' | '&' | ';' | '<' | '>' | '(' | ')')
}

pub fn is_name_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic() || (!c.is_ascii() && c.is_alphabetic() && wide_names())
}

pub fn is_name_char(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric() || (!c.is_ascii() && c.is_alphanumeric() && wide_names())
}

// zsh accepts non-ASCII letters in names unless POSIX_IDENTIFIERS.
fn wide_names() -> bool {
    !crate::options::quick("posixidentifiers")
}

pub fn is_valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if is_name_start(c) => chars.all(is_name_char),
        _ => false,
    }
}

/// True if `s` has the form `name=...`, `name+=...`, or `name[sub]=...`.
pub fn is_assignment(s: &str) -> bool {
    assignment_split(s).is_some()
}

/// Splits an assignment word into (name, optional subscript, append, value).
pub fn assignment_split(s: &str) -> Option<(String, Option<String>, bool, String)> {
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() || !is_name_start(chars[0]) {
        return None;
    }
    let mut i = 1;
    while i < chars.len() && is_name_char(chars[i]) {
        i += 1;
    }
    let name: String = chars[..i].iter().collect();
    let mut subscript = None;
    if i < chars.len() && chars[i] == '[' {
        let mut depth = 0;
        let start = i + 1;
        let mut j = i;
        while j < chars.len() {
            match chars[j] {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        if j >= chars.len() || j == start {
            return None;
        }
        subscript = Some(chars[start..j].iter().collect());
        i = j + 1;
    }
    let append = i < chars.len() && chars[i] == '+';
    if append {
        i += 1;
    }
    if i >= chars.len() || chars[i] != '=' {
        return None;
    }
    let value: String = chars[i + 1..].iter().collect();
    Some((name, subscript, append, value))
}

// ---- Scanners shared with the expander --------------------------------
//
// Each takes the index of the opening character and returns the index just
// past the closing one, or None if the input ends first.

pub fn scan_squote(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < chars.len() {
        if chars[j] == '\'' {
            return Some(j + 1);
        }
        j += 1;
    }
    None
}

// `$'...'`: `i` indexes the quote after `$`.
pub fn scan_dollar_squote(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < chars.len() {
        match chars[j] {
            '\\' => j += 2,
            '\'' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

pub fn scan_dquote(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < chars.len() {
        match chars[j] {
            '\\' => j += 2,
            '"' => return Some(j + 1),
            '`' => j = scan_bquote(chars, j)?,
            '$' if j + 1 < chars.len() && chars[j + 1] == '(' => j = scan_paren(chars, j + 1)?,
            '$' if j + 1 < chars.len() && chars[j + 1] == '{' => {
                j = scan_brace(chars, j + 1, true)?
            }
            _ => j += 1,
        }
    }
    None
}

pub fn scan_bquote(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < chars.len() {
        match chars[j] {
            '\\' => j += 2,
            '`' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

// `${...}`: `i` indexes the `{`.
pub fn scan_brace(chars: &[char], i: usize, in_dq: bool) -> Option<usize> {
    let mut j = i + 1;
    let mut depth = 1;
    while j < chars.len() {
        match chars[j] {
            '\\' => j += 2,
            '\'' if !in_dq => j = scan_squote(chars, j)?,
            '"' => j = scan_dquote(chars, j)?,
            '`' => j = scan_bquote(chars, j)?,
            '$' if j + 1 < chars.len() && chars[j + 1] == '(' => j = scan_paren(chars, j + 1)?,
            '$' if j + 1 < chars.len() && chars[j + 1] == '{' => {
                j = scan_brace(chars, j + 1, in_dq)?
            }
            '{' => {
                depth += 1;
                j += 1;
            }
            '}' => {
                depth -= 1;
                j += 1;
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => j += 1,
        }
    }
    None
}

// `$(...)` and `$((...))`: `i` indexes the first `(`. Unbalanced `)` that
// end case patterns are skipped while inside `case ... esac`.
pub fn scan_paren(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i + 1;
    let mut depth = 1;
    let mut case_depth = 0usize;
    while j < chars.len() {
        let c = chars[j];
        let at_word_start = j == i + 1
            || matches!(
                chars[j - 1],
                ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')'
            );
        match c {
            '\\' => j += 2,
            '\'' => j = scan_squote(chars, j)?,
            '"' => j = scan_dquote(chars, j)?,
            '`' => j = scan_bquote(chars, j)?,
            '$' if j + 1 < chars.len() && chars[j + 1] == '(' => j = scan_paren(chars, j + 1)?,
            '$' if j + 1 < chars.len() && chars[j + 1] == '{' => {
                j = scan_brace(chars, j + 1, false)?
            }
            '#' if at_word_start => {
                while j < chars.len() && chars[j] != '\n' {
                    j += 1;
                }
            }
            '(' => {
                depth += 1;
                j += 1;
            }
            ')' => {
                j += 1;
                if case_depth > 0 && depth == 1 {
                    continue;
                }
                depth -= 1;
                if depth == 0 {
                    return Some(j);
                }
            }
            c if at_word_start && is_name_start(c) => {
                let start = j;
                while j < chars.len() && is_name_char(chars[j]) {
                    j += 1;
                }
                let word: String = chars[start..j].iter().collect();
                let ends_word = j >= chars.len()
                    || matches!(chars[j], ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')');
                if ends_word {
                    if word == "case" {
                        case_depth += 1;
                    } else if word == "esac" {
                        case_depth = case_depth.saturating_sub(1);
                    }
                }
            }
            _ => j += 1,
        }
    }
    None
}

/// `$((...))` with `i` at the first `(`: returns the index after `))`, or
/// None if the parentheses do not close that way (then it is a command
/// substitution starting with a subshell). Unlike `scan_paren`, `#` is
/// not a comment here: `$(( #c ))` is a character code.
pub fn scan_arith(chars: &[char], i: usize) -> Option<usize> {
    if chars.get(i + 1) != Some(&'(') {
        return None;
    }
    let mut depth = 0usize;
    let mut j = i;
    while j < chars.len() {
        match chars[j] {
            '\\' => j += 1,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 1 {
                    return (chars.get(j + 1) == Some(&')')).then_some(j + 2);
                }
                if depth == 0 {
                    return None;
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// Length of a zsh numeric range glob `<->`, `<1-10>`, `<5->` at `i`.
pub fn numeric_range_len(chars: &[char], i: usize) -> Option<usize> {
    if chars.get(i) != Some(&'<') {
        return None;
    }
    let mut j = i + 1;
    while chars.get(j).is_some_and(|c| c.is_ascii_digit()) {
        j += 1;
    }
    if chars.get(j) != Some(&'-') {
        return None;
    }
    j += 1;
    while chars.get(j).is_some_and(|c| c.is_ascii_digit()) {
        j += 1;
    }
    (chars.get(j) == Some(&'>')).then_some(j + 1 - i)
}

// True if `word` is `name=`, `name+=` or `name[sub]=`: a following `(`
// starts an array value rather than continuing the word.
fn is_assign_prefix(word: &str) -> bool {
    let w = word.strip_suffix('=').unwrap_or("");
    let w = w.strip_suffix('+').unwrap_or(w);
    let name = match w.find('[') {
        Some(k) if w.ends_with(']') => &w[..k],
        _ => w,
    };
    word.ends_with('=') && is_valid_name(name)
}

/// Removes quoting from a here-document delimiter word. Returns the
/// delimiter and whether any part of it was quoted.
pub fn unquote_delimiter(raw: &str) -> (String, bool) {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::new();
    let mut quoted = false;
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                quoted = true;
                if i + 1 < chars.len() {
                    out.push(chars[i + 1]);
                }
                i += 2;
            }
            '\'' | '"' => {
                quoted = true;
                let q = chars[i];
                i += 1;
                while i < chars.len() && chars[i] != q {
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
    (out, quoted)
}

struct PendingHeredoc {
    id: usize,
    delim: String,
    strip_tabs: bool,
}

pub struct Lexer {
    chars: Vec<char>,
    pos: usize,
    // When false, reaching the end of the input inside a construct reports
    // Incomplete so the caller can read another line.
    final_input: bool,
    pending: Vec<PendingHeredoc>,
    pub heredoc_bodies: HashMap<usize, String>,
    next_heredoc_id: usize,
    // (position, line) of the last `line()` answer, to count on from.
    line_cache: std::cell::Cell<(usize, usize)>,
    // Lines of the same input read before this text.
    pub line_offset: usize,
    // Whether `#` at the start of a word starts a comment.
    pub comments: bool,
}

impl Lexer {
    pub fn new(input: &str, final_input: bool) -> Self {
        Lexer {
            chars: input.chars().collect(),
            pos: 0,
            final_input,
            pending: Vec::new(),
            heredoc_bodies: HashMap::new(),
            next_heredoc_id: 0,
            line_cache: std::cell::Cell::new((0, 1)),
            line_offset: 0,
            comments: true,
        }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn set_pos(&mut self, pos: usize) {
        self.pos = pos.min(self.chars.len());
    }

    /// 1-based line number of the current position.
    pub fn line(&self) -> usize {
        let pos = self.pos.min(self.chars.len());
        let (mut from, mut line) = self.line_cache.get();
        if pos < from {
            (from, line) = (0, 1);
        }
        line += self.chars[from..pos].iter().filter(|&&c| c == '\n').count();
        self.line_cache.set((pos, line));
        line + self.line_offset
    }

    pub fn rest(&self) -> String {
        self.chars[self.pos.min(self.chars.len())..]
            .iter()
            .collect()
    }

    /// Splices alias text into the input at the current position.
    pub fn insert_text(&mut self, text: &str) {
        let at = self.pos.min(self.chars.len());
        self.chars.splice(at..at, text.chars());
    }

    fn incomplete(&self, msg: &str) -> LexError {
        if self.final_input {
            LexError::Syntax(msg.to_string())
        } else {
            LexError::Incomplete
        }
    }

    /// Registers a here-document whose body starts after the next newline.
    pub fn add_heredoc(&mut self, raw_delim: &str, strip_tabs: bool) -> (usize, bool) {
        let (delim, quoted) = unquote_delimiter(raw_delim);
        let id = self.next_heredoc_id;
        self.next_heredoc_id += 1;
        self.pending.push(PendingHeredoc {
            id,
            delim,
            strip_tabs,
        });
        (id, !quoted)
    }

    pub fn has_pending_heredocs(&self) -> bool {
        !self.pending.is_empty()
    }

    fn read_heredocs(&mut self) -> Result<(), LexError> {
        let pending = std::mem::take(&mut self.pending);
        for hd in pending {
            let mut body = String::new();
            loop {
                if self.pos >= self.chars.len() {
                    if !self.final_input {
                        return Err(LexError::Incomplete);
                    }
                    eprintln!(
                        "meowsh: warning: here-document delimited by end-of-file (wanted `{}')",
                        hd.delim
                    );
                    break;
                }
                let start = self.pos;
                let mut end = start;
                while end < self.chars.len() && self.chars[end] != '\n' {
                    end += 1;
                }
                if end >= self.chars.len() && !self.final_input {
                    return Err(LexError::Incomplete);
                }
                let mut line: String = self.chars[start..end].iter().collect();
                if hd.strip_tabs {
                    line = line.trim_start_matches('\t').to_string();
                }
                self.pos = (end + 1).min(self.chars.len());
                if line == hd.delim {
                    break;
                }
                body.push_str(&line);
                body.push('\n');
            }
            self.heredoc_bodies.insert(hd.id, body);
        }
        Ok(())
    }

    pub fn next_token(&mut self) -> Result<Tok, LexError> {
        loop {
            while self.pos < self.chars.len() && matches!(self.chars[self.pos], ' ' | '\t') {
                self.pos += 1;
            }
            if self.pos + 1 < self.chars.len()
                && self.chars[self.pos] == '\\'
                && self.chars[self.pos + 1] == '\n'
            {
                self.pos += 2;
                if self.pos >= self.chars.len() && !self.final_input {
                    return Err(LexError::Incomplete);
                }
                continue;
            }
            // `#` starts a comment at the start of a word, but not right
            // after `(` or `)`, where zsh reads glob syntax such as `(#i)`
            // and `(ab)##`.
            if self.comments
                && self.pos < self.chars.len()
                && self.chars[self.pos] == '#'
                && !(self.pos > 0 && matches!(self.chars[self.pos - 1], '(' | ')'))
            {
                while self.pos < self.chars.len() && self.chars[self.pos] != '\n' {
                    self.pos += 1;
                }
                continue;
            }
            break;
        }

        if self.pos >= self.chars.len() {
            if !self.pending.is_empty() {
                // `cat <<EOF` with no newline after it.
                if !self.final_input {
                    return Err(LexError::Incomplete);
                }
                self.read_heredocs()?;
            }
            return Ok(Tok::Eof);
        }

        let c = self.chars[self.pos];
        if c == '\n' {
            self.pos += 1;
            if !self.pending.is_empty() {
                self.read_heredocs()?;
            }
            return Ok(Tok::Newline);
        }

        if c.is_ascii_digit() {
            let mut j = self.pos;
            while j < self.chars.len() && self.chars[j].is_ascii_digit() {
                j += 1;
            }
            if j < self.chars.len() && matches!(self.chars[j], '<' | '>') {
                let digits: String = self.chars[self.pos..j].iter().collect();
                if let Ok(n) = digits.parse::<i32>() {
                    self.pos = j;
                    return Ok(Tok::IoNumber(n));
                }
            }
        }

        if c == '{' {
            let mut j = self.pos + 1;
            while j < self.chars.len() && is_name_char(self.chars[j]) {
                j += 1;
            }
            if j > self.pos + 1
                && is_name_start(self.chars[self.pos + 1])
                && self.chars.get(j) == Some(&'}')
                && matches!(self.chars.get(j + 1), Some('<' | '>'))
                && self.chars.get(j + 2) != Some(&'(')
            {
                let name: String = self.chars[self.pos + 1..j].iter().collect();
                self.pos = j + 1;
                return Ok(Tok::IoVar(name));
            }
        }

        if c == '(' && self.chars.get(self.pos + 1) == Some(&'(') {
            if let Some(t) = self.try_arith()? {
                return Ok(t);
            }
        }

        let procsub = matches!(c, '<' | '>') && self.chars.get(self.pos + 1) == Some(&'(');
        let range = numeric_range_len(&self.chars, self.pos).is_some();
        if is_operator_start(c) && !procsub && !range {
            for op in OPERATORS {
                let len = op.chars().count();
                if self.pos + len <= self.chars.len()
                    && self.chars[self.pos..self.pos + len]
                        .iter()
                        .copied()
                        .eq(op.chars())
                {
                    self.pos += len;
                    return Ok(Tok::Op(op));
                }
            }
        }

        self.read_word().map(Tok::Word)
    }

    // `((` starts an arithmetic command when its parentheses close with
    // `))`; otherwise it is two subshell openers, as in `((cmd); cmd)`.
    fn try_arith(&mut self) -> Result<Option<Tok>, LexError> {
        if let Some(end) = scan_arith(&self.chars, self.pos) {
            let expr: String = self.chars[self.pos + 2..end - 2].iter().collect();
            self.pos = end;
            return Ok(Some(Tok::Arith(expr)));
        }
        if scan_paren(&self.chars, self.pos).is_none() && !self.final_input {
            return Err(LexError::Incomplete);
        }
        Ok(None)
    }

    fn read_word(&mut self) -> Result<String, LexError> {
        let mut out = String::new();
        while self.pos < self.chars.len() {
            let c = self.chars[self.pos];
            match c {
                ' ' | '\t' | '\n' => break,
                // Process substitution `<(...)` / `>(...)` at word start.
                '<' | '>' if out.is_empty() && self.chars.get(self.pos + 1) == Some(&'(') => {
                    let end = scan_paren(&self.chars, self.pos + 1).ok_or_else(|| {
                        self.incomplete("unexpected end of file while looking for matching `)'")
                    })?;
                    out.extend(&self.chars[self.pos..end]);
                    self.pos = end;
                }
                '<' if numeric_range_len(&self.chars, self.pos).is_some() => {
                    let len = numeric_range_len(&self.chars, self.pos).unwrap();
                    out.extend(&self.chars[self.pos..self.pos + len]);
                    self.pos += len;
                }
                // zsh: `(` inside a word continues it, as in the glob
                // qualifier `*(N)` or the pattern `x_(a|b)_*`. Not after
                // `name=` (array value) and not `name()` (function).
                '(' if !out.is_empty()
                    && !is_assign_prefix(&out)
                    && self.chars.get(self.pos + 1) != Some(&')') =>
                {
                    let end = scan_paren(&self.chars, self.pos).ok_or_else(|| {
                        self.incomplete("unexpected end of file while looking for matching `)'")
                    })?;
                    out.extend(&self.chars[self.pos..end]);
                    self.pos = end;
                }
                c if is_operator_start(c) => break,
                '\\' => {
                    if self.pos + 1 >= self.chars.len() {
                        out.push('\\');
                        self.pos += 1;
                    } else if self.chars[self.pos + 1] == '\n' {
                        self.pos += 2;
                        if self.pos >= self.chars.len() && !self.final_input {
                            return Err(LexError::Incomplete);
                        }
                    } else {
                        out.push('\\');
                        out.push(self.chars[self.pos + 1]);
                        self.pos += 2;
                    }
                }
                '\'' => {
                    let end = scan_squote(&self.chars, self.pos).ok_or_else(|| {
                        self.incomplete("unexpected end of file while looking for matching `''")
                    })?;
                    out.extend(&self.chars[self.pos..end]);
                    self.pos = end;
                }
                '"' => {
                    let end = scan_dquote(&self.chars, self.pos).ok_or_else(|| {
                        self.incomplete("unexpected end of file while looking for matching `\"'")
                    })?;
                    out.extend(&self.chars[self.pos..end]);
                    self.pos = end;
                }
                '`' => {
                    let end = scan_bquote(&self.chars, self.pos).ok_or_else(|| {
                        self.incomplete("unexpected end of file while looking for matching ``'")
                    })?;
                    out.extend(&self.chars[self.pos..end]);
                    self.pos = end;
                }
                '$' if self.pos + 1 < self.chars.len() => {
                    let next = self.chars[self.pos + 1];
                    let end = match next {
                        '(' => Some(
                            match scan_arith(&self.chars, self.pos + 1) {
                                Some(e) => Some(e),
                                None => scan_paren(&self.chars, self.pos + 1),
                            }
                            .ok_or_else(|| {
                                self.incomplete(
                                    "unexpected end of file while looking for matching `)'",
                                )
                            })?,
                        ),
                        '{' => {
                            Some(scan_brace(&self.chars, self.pos + 1, false).ok_or_else(|| {
                                self.incomplete(
                                    "unexpected end of file while looking for matching `}'",
                                )
                            })?)
                        }
                        '\'' => Some(scan_dollar_squote(&self.chars, self.pos + 1).ok_or_else(
                            || {
                                self.incomplete(
                                    "unexpected end of file while looking for matching `''",
                                )
                            },
                        )?),
                        _ => None,
                    };
                    match end {
                        Some(end) => {
                            out.extend(&self.chars[self.pos..end]);
                            self.pos = end;
                        }
                        None => {
                            out.push('$');
                            self.pos += 1;
                        }
                    }
                }
                c => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(s: &str) -> Vec<Tok> {
        let mut lx = Lexer::new(s, true);
        let mut out = Vec::new();
        loop {
            let t = lx.next_token().unwrap();
            if t == Tok::Eof {
                break;
            }
            out.push(t);
        }
        out
    }

    #[test]
    fn non_ascii_words_do_not_panic() {
        assert_eq!(
            tokens("echo é 'ü' \"ß\" # ç"),
            vec![
                Tok::Word("echo".into()),
                Tok::Word("é".into()),
                Tok::Word("'ü'".into()),
                Tok::Word("\"ß\"".into()),
            ]
        );
    }

    #[test]
    fn nested_substitutions_stay_one_word() {
        assert_eq!(
            tokens("echo ${x:-$(echo \"}\")}y z"),
            vec![
                Tok::Word("echo".into()),
                Tok::Word("${x:-$(echo \"}\")}y".into()),
                Tok::Word("z".into())
            ]
        );
    }

    #[test]
    fn case_inside_command_substitution() {
        assert_eq!(
            tokens("echo $(case a in a) echo y;; esac)"),
            vec![
                Tok::Word("echo".into()),
                Tok::Word("$(case a in a) echo y;; esac)".into())
            ]
        );
    }

    #[test]
    fn unterminated_quote_is_incomplete_when_streaming() {
        let mut lx = Lexer::new("echo 'abc\n", false);
        assert_eq!(lx.next_token(), Ok(Tok::Word("echo".into())));
        assert_eq!(lx.next_token(), Err(LexError::Incomplete));
    }

    #[test]
    fn assignment_detection() {
        assert!(is_assignment("a=1"));
        assert!(is_assignment("a+=1"));
        assert!(is_assignment("a[1]=x"));
        assert!(!is_assignment("1a=b"));
        assert!(!is_assignment("=b"));
        assert!(!is_assignment("a"));
    }
}
