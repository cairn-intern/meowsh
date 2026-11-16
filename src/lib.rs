pub mod arith;
pub mod builtins;
pub mod deparse;
pub mod exec;
pub mod expand;
pub mod globber;
pub mod jobs;
pub mod lexer;
pub mod lineedit;
pub mod menu;
pub mod options;
pub mod params;
pub mod parser;
pub mod pattern;
pub mod shell;
pub mod sys;
pub mod testexpr;
pub mod trap;
pub mod types;
pub mod typeset;

pub use exec::*;
pub use expand::*;
pub use jobs::*;
pub use lexer::*;
pub use lineedit::*;
pub use parser::*;
pub use shell::*;
pub use types::*;

pub fn get_clean_and_mappings(s: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(s.len());
    let mut mappings = Vec::new();
    let mut chars = s.char_indices().peekable();
    while let Some((idx, c)) = chars.next() {
        if c == '\x1b' {
            if let Some(&(_, next_c)) = chars.peek() {
                match next_c {
                    '[' => {
                        // CSI
                        chars.next();
                        for (_, c) in chars.by_ref() {
                            if ('@'..='~').contains(&c) {
                                break;
                            }
                        }
                    }
                    ']' => {
                        // OSC
                        chars.next();
                        while let Some((_, c)) = chars.next() {
                            if c == '\x07' {
                                // BEL
                                break;
                            }
                            if c == '\x1b' {
                                if let Some(&(_, next_next_c)) = chars.peek() {
                                    if next_next_c == '\\' {
                                        // ST
                                        chars.next();
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    '(' | ')' | '#' | '*' | '+' | '-' | '.' | '/' => {
                        // G0/G1 charset or other 2-char ESC
                        chars.next(); // Consume the qualifier
                        chars.next(); // Consume the character
                    }
                    _ => {
                        // Other ESC sequences are typically 2 characters total (ESC + something)
                        chars.next();
                    }
                }
            }
        } else {
            mappings.push(idx);
            out.push(c);
        }
    }
    mappings.push(s.len());
    (out, mappings)
}

pub fn strip_ansi(s: &str) -> String {
    get_clean_and_mappings(s).0
}

/// Terminal columns taken by `s` (no escape sequences): two for wide
/// East Asian characters and emoji, none for combining marks.
pub fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| match c as u32 {
            0x0300..=0x036F | 0x200B..=0x200F | 0xFE00..=0xFE0F => 0,
            c if c < 0x20 || c == 0x7F => 0,
            0x1100..=0x115F
            | 0x2E80..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1F64F
            | 0x1F900..=0x1F9FF
            | 0x20000..=0x3FFFD => 2,
            _ => 1,
        })
        .sum()
}

pub static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_ansi() {
        assert_eq!(strip_ansi("\x1b[31mhello\x1b[0m"), "hello");
        assert_eq!(strip_ansi("plain text"), "plain text");
        assert_eq!(
            strip_ansi("\x1b[1;32mgreen bold\x1b[0m and \x1b[90mgrey\x1b[0m"),
            "green bold and grey"
        );
        // Test OSC
        assert_eq!(strip_ansi("\x1b]0;terminal title\x07hello"), "hello");
        assert_eq!(strip_ansi("\x1b]0;title\x1b\\world"), "world");
        // Test other ESC
        assert_eq!(strip_ansi("\x1b(Bcharset"), "charset");
        assert_eq!(strip_ansi("\x1bAcursor up"), "cursor up");
    }

    #[test]
    fn display_widths() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("日本"), 4);
        assert_eq!(display_width("e\u{301}"), 1);
    }
}
