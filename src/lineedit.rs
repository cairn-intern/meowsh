use crate::exec::InputLine;
use crate::shell::build_prompt;
use crate::strip_ansi;
use rustyline::completion::Completer;
use rustyline::config::Configurer;
use rustyline::error::ReadlineError;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::history::FileHistory;
use rustyline::validate::Validator;
use rustyline::{ColorMode, Editor, Helper};
use std::borrow::Cow;
use std::cell::RefCell;

pub struct MeowshHelper;

impl MeowshHelper {
    pub fn command_exists(&self, name: &str) -> bool {
        crate::shell::command_exists(name)
    }

    fn fuzzy_match(&self, query: &str, candidate: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        let mut query_chars = query.chars().peekable();
        for c in candidate.chars() {
            if let Some(&q) = query_chars.peek() {
                if c.to_lowercase().next() == q.to_lowercase().next() {
                    query_chars.next();
                }
            } else {
                return true;
            }
        }
        query_chars.peek().is_none()
    }

    fn get_completions(&self, line: &str) -> Vec<rustyline::completion::Pair> {
        let mut completions = Vec::new();
        let shell = crate::shell::SHELL.shell.lock().unwrap();

        let parts: Vec<&str> = line.split_whitespace().collect();
        // Command position: empty line, single word without '/', or after a pipe/operator
        let trailing_space = line.ends_with(' ') || line.ends_with('\t');
        let is_cmd_position =
            parts.is_empty() || (parts.len() == 1 && !trailing_space && !parts[0].contains('/'));

        if is_cmd_position {
            let current = parts.last().unwrap_or(&"");

            for name in shell.aliases.keys() {
                if self.fuzzy_match(current, name) {
                    completions.push(rustyline::completion::Pair {
                        display: name.clone(),
                        replacement: name.clone(),
                    });
                }
            }

            for name in shell.functions.keys() {
                if self.fuzzy_match(current, name) {
                    completions.push(rustyline::completion::Pair {
                        display: name.clone(),
                        replacement: name.clone(),
                    });
                }
            }

            let builtins = crate::builtins::builtin_names();
            for b in builtins {
                if self.fuzzy_match(current, b) {
                    completions.push(rustyline::completion::Pair {
                        display: b.to_string(),
                        replacement: b.to_string(),
                    });
                }
            }

            let path = shell
                .vars
                .get("PATH")
                .map(|v| v.value.as_str())
                .unwrap_or("");
            for dir in path.split(':') {
                if dir.is_empty() {
                    continue;
                }
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        if let Ok(name) = entry.file_name().into_string() {
                            if self.fuzzy_match(current, &name) {
                                completions.push(rustyline::completion::Pair {
                                    display: name.clone(),
                                    replacement: name,
                                });
                            }
                        }
                    }
                }
            }
        } else {
            // Argument position: complete file/directory paths
            let current = if trailing_space {
                ""
            } else {
                parts.last().unwrap_or(&"")
            };
            let cmd = parts.first().unwrap_or(&"");
            let dirs_only = matches!(*cmd, "cd" | "pushd" | "rmdir");
            self.complete_paths(current, &mut completions, dirs_only);
        }

        completions
    }

    fn complete_paths(
        &self,
        partial: &str,
        completions: &mut Vec<rustyline::completion::Pair>,
        dirs_only: bool,
    ) {
        use std::path::Path;

        // Expand ~ to home directory
        let expanded = if partial.starts_with('~') {
            if let Some(home) = crate::shell::var_get_opt("HOME") {
                if partial == "~" {
                    home.clone()
                } else {
                    partial.replacen('~', &home, 1)
                }
            } else {
                partial.to_string()
            }
        } else {
            partial.to_string()
        };

        let (dir, prefix) = if expanded.ends_with('/') {
            (expanded.as_str(), "")
        } else {
            let path = Path::new(&expanded);
            match (path.parent(), path.file_name()) {
                (Some(parent), Some(name)) => {
                    let dir_str = if parent.as_os_str().is_empty() {
                        "."
                    } else {
                        parent.to_str().unwrap_or(".")
                    };
                    (dir_str, name.to_str().unwrap_or(""))
                }
                _ => (".", &*expanded),
            }
        };

        // We need dir to live long enough - rebind to owned string for the case where
        // dir was borrowed from a temporary
        let dir_owned;
        let dir = if dir == "." && expanded.ends_with('/') {
            &*expanded
        } else if dir == "." && !partial.is_empty() && !partial.starts_with('.') {
            dir
        } else {
            dir_owned = dir.to_string();
            &dir_owned
        };

        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let name = match entry.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue,
            };

            // Skip hidden files unless the user is explicitly typing a dot prefix
            if name.starts_with('.') && !prefix.starts_with('.') {
                continue;
            }

            if !name.starts_with(prefix) {
                continue;
            }

            let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);

            if dirs_only && !is_dir {
                continue;
            }

            // Build the replacement string preserving the user's typed prefix
            let replacement = if partial.is_empty() {
                if is_dir {
                    format!("{}/", name)
                } else {
                    name.clone()
                }
            } else if partial.ends_with('/') {
                if is_dir {
                    format!("{}{}/", partial, name)
                } else {
                    format!("{}{}", partial, name)
                }
            } else {
                // Replace just the filename portion, keeping the directory part
                let dir_prefix = if let Some(slash_pos) = partial.rfind('/') {
                    &partial[..=slash_pos]
                } else {
                    ""
                };
                if is_dir {
                    format!("{}{}/", dir_prefix, name)
                } else {
                    format!("{}{}", dir_prefix, name)
                }
            };

            let display = if is_dir { format!("{}/", name) } else { name };

            completions.push(rustyline::completion::Pair {
                display,
                replacement,
            });
        }

        completions.sort_by(|a, b| a.display.cmp(&b.display));

        // If the only match is a directory, expand into it so the user can
        // keep tabbing into its contents instead of cycling back.
        if completions.len() == 1 && completions[0].replacement.ends_with('/') {
            let dir_path = completions[0].replacement.clone();
            let mut inner = Vec::new();
            self.complete_paths(&dir_path, &mut inner, dirs_only);
            if !inner.is_empty() {
                *completions = inner;
            }
        }
    }
}

#[derive(Clone, Debug)]
enum CharOrAnsi {
    Char(char),
    Ansi(String),
}

fn get_char_or_ansi(s: &str) -> Vec<CharOrAnsi> {
    let mut out = Vec::new();
    let mut chars = s.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c == '\x1b' {
            let mut ansi = String::new();
            ansi.push(c);
            if let Some(&(_, next_c)) = chars.peek() {
                match next_c {
                    '[' => {
                        // CSI
                        let (_, c2) = chars.next().unwrap();
                        ansi.push(c2);
                        for (_, c) in chars.by_ref() {
                            ansi.push(c);
                            if ('@'..='~').contains(&c) {
                                break;
                            }
                        }
                    }
                    ']' => {
                        // OSC
                        let (_, c2) = chars.next().unwrap();
                        ansi.push(c2);
                        while let Some((_, c)) = chars.next() {
                            ansi.push(c);
                            if c == '\x07' {
                                // BEL
                                break;
                            }
                            if c == '\x1b' {
                                if let Some(&(_, next_next_c)) = chars.peek() {
                                    if next_next_c == '\\' {
                                        // ST
                                        let (_, c3) = chars.next().unwrap();
                                        ansi.push(c3);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    '(' | ')' | '#' | '*' | '+' | '-' | '.' | '/' => {
                        // G0/G1 charset or other 2-char ESC
                        let (_, c2) = chars.next().unwrap();
                        ansi.push(c2);
                        if let Some((_, c3)) = chars.next() {
                            ansi.push(c3);
                        }
                    }
                    _ => {
                        if let Some((_, c2)) = chars.next() {
                            ansi.push(c2);
                        }
                    }
                }
            }
            out.push(CharOrAnsi::Ansi(ansi));
        } else {
            out.push(CharOrAnsi::Char(c));
        }
    }
    out
}

impl Completer for MeowshHelper {
    type Candidate = rustyline::completion::Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> Result<(usize, Vec<rustyline::completion::Pair>), ReadlineError> {
        let (clean_line, mappings) = crate::get_clean_and_mappings(line);
        // `mappings` has one entry per character of `clean_line`, so all
        // positions below are character indices, not byte offsets.
        let chars: Vec<char> = clean_line.chars().collect();
        let clean_pos = mappings
            .iter()
            .position(|&raw| raw >= pos)
            .unwrap_or(chars.len())
            .min(chars.len());
        let word_start = chars[..clean_pos]
            .iter()
            .rposition(|c| c.is_whitespace() || ";|&".contains(*c))
            .map(|i| i + 1)
            .unwrap_or(0);
        let start = mappings[word_start];
        let prefix: String = chars[..clean_pos].iter().collect();
        Ok((start, self.get_completions(&prefix)))
    }
}

impl Highlighter for MeowshHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if line.is_empty() {
            return Cow::Borrowed(line);
        }

        let parts = get_char_or_ansi(line);
        let mut colored = String::with_capacity(line.len() * 2);
        let mut i = 0;
        let mut is_start_of_command = true;

        while i < parts.len() {
            match &parts[i] {
                CharOrAnsi::Ansi(ansi) => {
                    colored.push_str(ansi);
                    i += 1;
                }
                CharOrAnsi::Char(c) => match *c {
                    ' ' | '\t' => {
                        colored.push(*c);
                        i += 1;
                    }
                    '#' if is_start_of_command => {
                        colored.push_str("\x1b[90m");
                        while i < parts.len() {
                            match &parts[i] {
                                CharOrAnsi::Char(c) => colored.push(*c),
                                CharOrAnsi::Ansi(ansi) => colored.push_str(ansi),
                            }
                            i += 1;
                        }
                        colored.push_str("\x1b[0m");
                        break;
                    }
                    ';' | '|' | '&' => {
                        colored.push_str("\x1b[36m");
                        colored.push(*c);
                        i += 1;
                        if i < parts.len() {
                            if let CharOrAnsi::Char(next_c) = &parts[i] {
                                if *next_c == *c && (*c == '|' || *c == '&') {
                                    colored.push(*next_c);
                                    i += 1;
                                }
                            }
                        }
                        colored.push_str("\x1b[0m");
                        is_start_of_command = true;
                    }
                    '\'' | '"' => {
                        let quote = *c;
                        colored.push_str("\x1b[33m");
                        colored.push(quote);
                        i += 1;
                        while i < parts.len() {
                            match &parts[i] {
                                CharOrAnsi::Char(c) if *c == quote => break,
                                CharOrAnsi::Char(c) if *c == '\\' => {
                                    colored.push('\\');
                                    i += 1;
                                    if i < parts.len() {
                                        if let CharOrAnsi::Char(c2) = &parts[i] {
                                            colored.push(*c2);
                                            i += 1;
                                        }
                                    }
                                }
                                CharOrAnsi::Char(c) => {
                                    colored.push(*c);
                                    i += 1;
                                }
                                CharOrAnsi::Ansi(ansi) => {
                                    colored.push_str(ansi);
                                    i += 1;
                                }
                            }
                        }
                        if i < parts.len() {
                            colored.push(quote);
                            i += 1;
                        }
                        colored.push_str("\x1b[0m");
                        is_start_of_command = false;
                    }
                    '$' => {
                        colored.push_str("\x1b[35m");
                        colored.push('$');
                        i += 1;
                        while i < parts.len() {
                            if let CharOrAnsi::Char(c) = &parts[i] {
                                if c.is_alphanumeric() || *c == '_' {
                                    colored.push(*c);
                                    i += 1;
                                    continue;
                                }
                            }
                            break;
                        }
                        colored.push_str("\x1b[0m");
                        is_start_of_command = false;
                    }
                    _ => {
                        let mut word_parts = Vec::new();
                        let mut visible_word = String::new();
                        while i < parts.len() {
                            match &parts[i] {
                                CharOrAnsi::Char(c)
                                    if c.is_whitespace() || ";|&'\"$".contains(*c) =>
                                {
                                    break;
                                }
                                CharOrAnsi::Char(c) => {
                                    visible_word.push(*c);
                                    word_parts.push(&parts[i]);
                                }
                                CharOrAnsi::Ansi(_) => {
                                    word_parts.push(&parts[i]);
                                }
                            }
                            i += 1;
                        }

                        if is_start_of_command {
                            if self.command_exists(&visible_word) {
                                colored.push_str("\x1b[32m");
                            } else {
                                colored.push_str("\x1b[31m");
                            }
                            for p in word_parts {
                                match p {
                                    CharOrAnsi::Char(c) => colored.push(*c),
                                    CharOrAnsi::Ansi(ansi) => colored.push_str(ansi),
                                }
                            }
                            colored.push_str("\x1b[0m");
                            is_start_of_command = false;
                        } else {
                            for p in word_parts {
                                match p {
                                    CharOrAnsi::Char(c) => colored.push(*c),
                                    CharOrAnsi::Ansi(ansi) => colored.push_str(ansi),
                                }
                            }
                        }
                    }
                },
            }
        }

        Cow::Owned(colored)
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(format!("\x1b[90m{}\x1b[0m", hint))
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _kind: CmdKind) -> bool {
        // Always re-render the full line so that command validity coloring
        // updates for the entire word as the user types each character.
        true
    }

    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        // Ensure trailing space for input separation
        if prompt.ends_with(' ') {
            Cow::Borrowed(prompt)
        } else {
            Cow::Owned(format!("{} ", prompt))
        }
    }
}

#[derive(Hash, Debug, PartialEq, Eq)]
pub struct MeowshHint {
    display: String,
    completion: String,
}

impl rustyline::hint::Hint for MeowshHint {
    fn display(&self) -> &str {
        &self.display
    }

    fn completion(&self) -> Option<&str> {
        Some(&self.completion)
    }
}

impl Hinter for MeowshHelper {
    type Hint = MeowshHint;

    fn hint(&self, line: &str, pos: usize, _ctx: &rustyline::Context<'_>) -> Option<MeowshHint> {
        // Only show hint if cursor is at the end of the line
        if pos < line.len() {
            return None;
        }

        let clean_line = strip_ansi(line);
        let trimmed_input = clean_line.trim_start();
        if trimmed_input.is_empty() {
            return None;
        }

        let shell = crate::shell::SHELL.shell.lock().unwrap();
        let history = shell.history.lock().unwrap();

        let lower_input = trimmed_input.to_lowercase();
        for hist in history.iter().rev() {
            let clean_hist = strip_ansi(hist);
            let lower_hist = clean_hist.to_lowercase();
            if lower_hist.starts_with(&lower_input)
                && clean_hist.chars().count() > trimmed_input.chars().count()
            {
                // Return what's left of the history entry after the input
                // We skip the number of characters in the input to get the hint suffix
                let char_count = trimmed_input.chars().count();
                let hint_text: String = clean_hist.chars().skip(char_count).collect();

                return Some(MeowshHint {
                    display: hint_text.clone(),
                    completion: hint_text, // hist is already clean
                });
            }
        }
        None
    }
}

impl Validator for MeowshHelper {}

impl Helper for MeowshHelper {}

fn continuation_prompt() -> String {
    let ps2 = crate::shell::var_get_opt("PS2").unwrap_or_else(|| "> ".to_string());
    crate::expand::expand_prompt_string(&ps2).unwrap_or(ps2)
}

fn primary_prompt() -> String {
    crate::builtins::call_if_defined("precmd");
    build_prompt()
}

/// Reads commands from `fd` until end of input. Prompts go to standard
/// error when `prompts` is set (an interactive shell without a terminal).
pub fn run_fd(fd: i32, prompts: bool) -> i32 {
    let mut reader = crate::sys::LineReader::new(fd);
    crate::exec::run_input(
        |cont| {
            if prompts {
                let p = if cont {
                    continuation_prompt()
                } else {
                    primary_prompt()
                };
                let _ = crate::sys::write_fd(2, p.as_bytes());
            }
            match reader.read_line() {
                Some(l) => InputLine::Line(l),
                None => InputLine::Eof,
            }
        },
        crate::exec::history_add,
    )
}

/// The interactive read-eval loop with line editing and history.
pub fn run_interactive() -> i32 {
    if unsafe { libc::isatty(0) } != 1 {
        return run_fd(0, true);
    }
    let mut rl: Editor<MeowshHelper, FileHistory> = match Editor::new() {
        Ok(rl) => rl,
        Err(_) => return run_fd(0, true),
    };
    rl.set_color_mode(ColorMode::Forced);
    rl.set_helper(Some(MeowshHelper));
    let _ = rl.set_max_history_size(1000);

    let history_file = crate::shell::with_shell(|s| s.history_file.clone());
    if !history_file.is_empty() {
        let _ = rl.load_history(&history_file);
    }
    let entries: Vec<String> = rl.history().iter().cloned().collect();
    crate::shell::with_shell(|s| *s.history.lock().unwrap_or_else(|e| e.into_inner()) = entries);

    let rl = RefCell::new(rl);
    let status = crate::exec::run_input(
        |cont| {
            let prompt = if cont {
                continuation_prompt()
            } else {
                primary_prompt()
            };
            let line = rl.borrow_mut().readline(&prompt);
            match line {
                Ok(line) => InputLine::Line(line + "\n"),
                Err(ReadlineError::Interrupted) => InputLine::Interrupted,
                Err(ReadlineError::Eof) => InputLine::Eof,
                Err(err) => {
                    crate::sys::err_line(&format!("meowsh: {}", err));
                    InputLine::Eof
                }
            }
        },
        |text| {
            let clean = strip_ansi(text);
            crate::exec::history_add(&clean);
            let mut rl = rl.borrow_mut();
            let _ = rl.add_history_entry(clean.as_str());
            if !history_file.is_empty() {
                let _ = rl.append_history(&history_file);
            }
        },
    );
    if !matches!(crate::exec::flow(), crate::types::Flow::Exit(_)) {
        crate::sys::err_line("exit");
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::SHELL;
    use crate::TEST_LOCK;
    use rustyline::hint::Hinter;

    #[test]
    fn test_hinter_no_input() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;
        let history = FileHistory::new();
        let ctx = rustyline::Context::new(&history);
        assert_eq!(helper.hint("", 0, &ctx), None);
        assert_eq!(helper.hint("   ", 3, &ctx), None);
    }

    #[test]
    fn test_hinter_mid_line() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;
        let history = FileHistory::new();
        let ctx = rustyline::Context::new(&history);
        assert_eq!(helper.hint("echo hello", 4, &ctx), None);
    }

    #[test]
    fn test_hinter_match() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        {
            let shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
            let mut history = shell.history.lock().unwrap_or_else(|e| e.into_inner());
            history.clear();
            history.push("ls -l".to_string());
            history.push("echo hello".to_string());
        }

        let helper = MeowshHelper;
        let history = FileHistory::new();
        let ctx = rustyline::Context::new(&history);

        // Case insensitive match and prefix match
        let hint = helper.hint("ec", 2, &ctx);
        assert!(hint.is_some());
        use rustyline::hint::Hint;
        assert_eq!(hint.as_ref().unwrap().display(), "ho hello");
        assert_eq!(hint.as_ref().unwrap().completion(), Some("ho hello"));

        let hint = helper.hint("LS", 2, &ctx);
        assert!(hint.is_some());
        assert_eq!(hint.as_ref().unwrap().display(), " -l");
        assert_eq!(hint.as_ref().unwrap().completion(), Some(" -l"));
    }

    #[test]
    fn test_hinter_no_match() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        {
            let shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
            let mut history = shell.history.lock().unwrap_or_else(|e| e.into_inner());
            history.clear();
            history.push("ls -l".to_string());
        }

        let helper = MeowshHelper;
        let history = FileHistory::new();
        let ctx = rustyline::Context::new(&history);
        assert_eq!(helper.hint("grep", 4, &ctx), None);
    }

    #[test]
    fn test_highlight_command() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        // Known builtin
        let h = helper.highlight("echo hello", 0);
        assert!(h.contains("\x1b[32mecho\x1b[0m"));

        // Unknown command
        let h = helper.highlight("nonexistent_command", 0);
        assert!(h.contains("\x1b[31mnonexistent_command\x1b[0m"));
    }

    #[test]
    fn test_highlight_operators() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        let h = helper.highlight("ls | grep foo && echo bar", 0);
        assert!(h.contains("\x1b[36m|\x1b[0m"));
        assert!(h.contains("\x1b[36m&&\x1b[0m"));
        // Second command 'grep' should be highlighted too
        assert!(h.contains("\x1b[32mgrep\x1b[0m") || h.contains("\x1b[31mgrep\x1b[0m"));
    }

    #[test]
    fn test_highlight_strings() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        let h = helper.highlight("echo \"hello world\"", 0);
        assert!(h.contains("\x1b[33m\"hello world\"\x1b[0m"));
    }

    #[test]
    fn test_highlight_comments() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        let h = helper.highlight("# this is a comment", 0);
        assert!(h.contains("\x1b[90m# this is a comment\x1b[0m"));
    }

    #[test]
    fn test_fuzzy_match() {
        let helper = MeowshHelper;
        assert!(helper.fuzzy_match("msh", "meowsh"));
        assert!(helper.fuzzy_match("echo", "echo"));
        assert!(helper.fuzzy_match("eo", "echo"));
        assert!(helper.fuzzy_match("E", "echo"));
        assert!(!helper.fuzzy_match("abc", "echo"));
        assert!(helper.fuzzy_match("", "anything"));
    }

    #[test]
    fn test_fuzzy_completions() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        // Test builtins fuzzy matching
        let comps = helper.get_completions("eo");
        assert!(comps.iter().any(|c| c.display == "echo"));

        let comps = helper.get_completions("rd");
        assert!(comps.iter().any(|c| c.display == "read"));
    }

    #[test]
    fn test_highlight_char_always_true() {
        let helper = MeowshHelper;
        // highlight_char must return true for all inputs so rustyline
        // re-renders the full line after every keystroke, preventing
        // stale red coloring on partial command words.
        assert!(helper.highlight_char("cargo", 0, CmdKind::MoveCursor));
        assert!(helper.highlight_char("cargo", 3, CmdKind::Other));
        assert!(helper.highlight_char("", 0, CmdKind::ForcedRefresh));
    }

    #[test]
    fn test_highlight_command_with_ansi() {
        let _lock = crate::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        // "ec\x1b[90mho" should be seen as "echo" which is a builtin (green)
        let h = helper.highlight("ec\x1b[90mho hello", 0);
        assert!(h.contains("\x1b[32m")); // Green start
        assert!(h.contains("ec\x1b[90mho")); // Original text preserved
        assert!(h.contains("\x1b[0m")); // Reset at end of word
    }

    #[test]
    fn test_complete_with_ansi() {
        let _lock = crate::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;
        let history = FileHistory::new();
        let ctx = rustyline::Context::new(&history);

        // "\x1b[31mec" -> complete should find "echo" and offset should be at "ec" (index 5)
        use rustyline::completion::Completer;
        let (offset, candidates) = helper.complete("\x1b[31mec", 7, &ctx).unwrap();
        assert_eq!(offset, 5);
        assert!(candidates.iter().any(|c| c.display == "echo"));
    }

    #[test]
    fn test_complete_paths_in_current_dir() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        // Create a temp directory with known files
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("hello.txt");
        std::fs::write(&file_path, "test").unwrap();
        let sub_dir = tmp.path().join("subdir");
        std::fs::create_dir(&sub_dir).unwrap();

        let mut completions = Vec::new();
        let partial = tmp.path().join("hel");
        let partial_str = partial.to_str().unwrap();
        helper.complete_paths(partial_str, &mut completions, false);
        assert!(completions.iter().any(|c| c.display == "hello.txt"));

        // When a single directory matches, it auto-expands into its contents.
        // Add a file inside subdir so the expansion has results.
        std::fs::write(sub_dir.join("inner.txt"), "").unwrap();
        let mut completions = Vec::new();
        let partial = tmp.path().join("sub");
        let partial_str = partial.to_str().unwrap();
        helper.complete_paths(partial_str, &mut completions, false);
        // "subdir/" was the only match, so it expanded into its contents
        assert!(completions.iter().any(|c| c.display == "inner.txt"));

        // When multiple dirs/files match the prefix, directories keep their slash
        std::fs::create_dir(tmp.path().join("submarine")).unwrap();
        let mut completions = Vec::new();
        helper.complete_paths(partial_str, &mut completions, false);
        assert!(completions.iter().any(|c| c.display == "subdir/"));
        assert!(completions.iter().any(|c| c.display == "submarine/"));
    }

    #[test]
    fn test_complete_paths_hidden_files() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".hidden"), "").unwrap();
        std::fs::write(tmp.path().join("visible"), "").unwrap();

        // Without dot prefix, hidden files should be excluded
        let mut completions = Vec::new();
        let partial = tmp.path().join("");
        let partial_str = partial.to_str().unwrap();
        helper.complete_paths(partial_str, &mut completions, false);
        assert!(!completions.iter().any(|c| c.display == ".hidden"));
        assert!(completions.iter().any(|c| c.display == "visible"));

        // With dot prefix, hidden files should appear
        let mut completions = Vec::new();
        let partial = tmp.path().join(".hid");
        let partial_str = partial.to_str().unwrap();
        helper.complete_paths(partial_str, &mut completions, false);
        assert!(completions.iter().any(|c| c.display == ".hidden"));
    }

    #[test]
    fn test_complete_paths_tilde_expansion() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        // ~ should expand to HOME and list files there
        let mut completions = Vec::new();
        helper.complete_paths("~/", &mut completions, false);
        // We can't assert specific files in HOME, but we should get some results
        // and no panics. Just verify it doesn't crash.
        drop(completions);
    }

    #[test]
    fn test_complete_paths_nonexistent_dir() {
        let helper = MeowshHelper;

        // Should return empty, not panic
        let mut completions = Vec::new();
        helper.complete_paths("/nonexistent_dir_12345/foo", &mut completions, false);
        assert!(completions.is_empty());
    }

    #[test]
    fn test_complete_file_in_arg_position() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("myfile.rs"), "").unwrap();

        // Simulate "cat <partial_path>" — argument position
        let partial = tmp.path().join("myf");
        let line = format!("cat {}", partial.to_str().unwrap());
        let completions = helper.get_completions(&line);
        assert!(completions.iter().any(|c| c.display == "myfile.rs"));
    }

    #[test]
    fn test_complete_empty_arg_lists_files() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        // "echo " (with trailing space) should be argument position
        // and complete files in current directory
        let completions = helper.get_completions("echo ");
        // Should have at least some files (Cargo.toml, src/, etc.)
        assert!(!completions.is_empty());
    }

    #[test]
    fn test_complete_dir_adds_trailing_slash() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("mydir")).unwrap();
        std::fs::write(tmp.path().join("myfile"), "").unwrap();

        let mut completions = Vec::new();
        let partial = tmp.path().join("my");
        let partial_str = partial.to_str().unwrap();
        helper.complete_paths(partial_str, &mut completions, false);

        let dir_comp = completions.iter().find(|c| c.display == "mydir/").unwrap();
        assert!(dir_comp.replacement.ends_with('/'));

        let file_comp = completions.iter().find(|c| c.display == "myfile").unwrap();
        assert!(!file_comp.replacement.ends_with('/'));
    }

    #[test]
    fn test_cd_completes_dirs_only() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("alpha")).unwrap();
        std::fs::create_dir(tmp.path().join("bravo")).unwrap();
        std::fs::write(tmp.path().join("afile.txt"), "").unwrap();

        // dirs_only=true should exclude files
        let mut completions = Vec::new();
        let partial = tmp.path().join("a");
        let partial_str = partial.to_str().unwrap();
        helper.complete_paths(partial_str, &mut completions, true);
        assert!(completions.iter().any(|c| c.display == "alpha/"));
        assert!(!completions.iter().any(|c| c.display == "afile.txt"));

        // dirs_only=false should include both
        let mut completions = Vec::new();
        helper.complete_paths(partial_str, &mut completions, false);
        assert!(completions.iter().any(|c| c.display == "alpha/"));
        assert!(completions.iter().any(|c| c.display == "afile.txt"));
    }

    #[test]
    fn test_cd_completion_via_get_completions() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let helper = MeowshHelper;

        // "cd " should only show directories from cwd, not files
        let completions = helper.get_completions("cd ");
        for c in &completions {
            assert!(
                c.display.ends_with('/'),
                "cd completion '{}' should be a directory",
                c.display
            );
        }
        // Should have at least src/ and tests/ from this project
        assert!(completions.iter().any(|c| c.display == "src/"));
    }
}
