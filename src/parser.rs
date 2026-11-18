use crate::lexer::{assignment_split, is_valid_name, LexError, Lexer, Tok};
use crate::types::{
    AndOrOp, Assign, AssignValue, CaseItem, HereDoc, ListItem, Node, Redir, RedirOp, SimpleCmd,
};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
pub enum ParseError {
    // The input ended inside a construct; more input may complete it.
    Incomplete,
    Syntax(String),
}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        match e {
            LexError::Incomplete => ParseError::Incomplete,
            LexError::Syntax(s) => ParseError::Syntax(s),
        }
    }
}

type PResult<T> = Result<T, ParseError>;

// zsh's reserved words, which POSIX_ALIASES keeps from alias expansion.
const RESERVED_WORDS: &[&str] = &[
    "!",
    "[[",
    "{",
    "}",
    "case",
    "coproc",
    "do",
    "done",
    "elif",
    "else",
    "end",
    "esac",
    "fi",
    "for",
    "foreach",
    "function",
    "if",
    "in",
    "nocorrect",
    "repeat",
    "select",
    "then",
    "time",
    "until",
    "while",
];

const LIST_TERMINATORS: &[&str] = &[
    "then", "else", "elif", "fi", "do", "done", "esac", "}", "end",
];

type AliasLookup = Box<dyn Fn(&str) -> Option<String>>;

pub struct Parser {
    lex: Lexer,
    peeked: Option<Tok>,
    // Tokens split off a word (`{cmd` → `{` `cmd`), read before the
    // lexer's next one; the last is next.
    queued: Vec<Tok>,
    // The peeked token came from `queued`, not from the lexer.
    peeked_queued: bool,
    // The last token read other than a newline, for zsh's
    // "parse error near" at end of input.
    last: Option<Tok>,
    final_input: bool,
    // Aliases currently being expanded and the input offset where each
    // expansion ends; an alias is not re-expanded inside its own text.
    alias_guard: Vec<(String, usize)>,
    // Set after an alias whose value ends in a blank: the next word is
    // also checked for alias expansion.
    alias_next: bool,
    alias_lookup: AliasLookup,
    galias_lookup: AliasLookup,
    // Open `{ ... }` groups; inside one, a bare `}` ends a simple command
    // as in zsh (`{ echo hi }`).
    brace_depth: usize,
    // Second half of `&>file`, which redirects both stdout and stderr.
    pending_redir: Option<Redir>,
}

// Whether zsh's brace splitting applies (`{cmd`, `cmd}`): not with
// IGNORE_BRACES or in sh emulation.
fn braces_split() -> bool {
    !crate::options::is_set("ignorebraces") && !crate::exec::sh_emulation_pub()
}

impl Parser {
    pub fn new(input: &str, final_input: bool) -> Self {
        Parser {
            lex: Lexer::new(input, final_input),
            peeked: None,
            queued: Vec::new(),
            peeked_queued: false,
            last: None,
            final_input,
            alias_guard: Vec::new(),
            alias_next: false,
            alias_lookup: Box::new(crate::shell::alias_get),
            galias_lookup: Box::new(crate::shell::galias_get),
            brace_depth: 0,
            pending_redir: None,
        }
    }

    /// Parser that never expands aliases (used by tests and the highlighter).
    pub fn without_aliases(input: &str, final_input: bool) -> Self {
        let mut p = Parser::new(input, final_input);
        p.alias_lookup = Box::new(|_| None);
        p.galias_lookup = Box::new(|_| None);
        p
    }

    pub fn line(&self) -> usize {
        self.lex.line()
    }

    /// Counts lines from `n` + 1: the text continues an input of which
    /// `n` lines were already read.
    pub fn set_line_offset(&mut self, n: usize) {
        self.lex.line_offset = n;
    }

    /// Input not yet consumed. Only meaningful between complete commands.
    pub fn rest(&self) -> String {
        self.lex.rest()
    }

    fn peek(&mut self) -> PResult<&Tok> {
        if self.peeked.is_none() {
            self.peeked_queued = !self.queued.is_empty();
            self.peeked = Some(match self.queued.pop() {
                Some(t) => t,
                None => self.lex.next_token()?,
            });
        }
        Ok(self.peeked.as_ref().unwrap())
    }

    fn next(&mut self) -> PResult<Tok> {
        let t = match self.peeked.take() {
            Some(t) => t,
            None => match self.queued.pop() {
                Some(t) => t,
                None => self.lex.next_token()?,
            },
        };
        // zsh names the last token read, a newline included.
        if !matches!(t, Tok::Eof) {
            self.last = Some(t.clone());
        }
        Ok(t)
    }

    fn eof_error(&self, what: &str) -> ParseError {
        if self.final_input {
            ParseError::Syntax(match &self.last {
                Some(t) => format!("parse error near `{}'", tok_text(t)),
                None => format!("unexpected end of file (expecting {})", what),
            })
        } else {
            ParseError::Incomplete
        }
    }

    fn unexpected(&self, tok: &Tok) -> ParseError {
        ParseError::Syntax(format!("parse error near `{}'", tok_text(tok)))
    }

    fn peek_is_word(&mut self, w: &str) -> PResult<bool> {
        Ok(matches!(self.peek()?, Tok::Word(x) if x == w))
    }

    fn peek_is_op(&mut self, op: &str) -> PResult<bool> {
        Ok(matches!(self.peek()?, Tok::Op(x) if *x == op))
    }

    fn skip_newlines(&mut self) -> PResult<()> {
        while matches!(self.peek()?, Tok::Newline) {
            self.next()?;
        }
        Ok(())
    }

    // After `|`, `&&` or `||`: zsh takes `;` there as it takes a newline.
    fn skip_linebreak(&mut self) -> PResult<()> {
        while matches!(self.peek()?, Tok::Newline | Tok::Op(";")) {
            self.next()?;
        }
        Ok(())
    }

    fn expect_word(&mut self, w: &str) -> PResult<()> {
        self.skip_newlines()?;
        match self.next()? {
            Tok::Word(x) if x == w => Ok(()),
            Tok::Eof => Err(self.eof_error(&format!("`{}'", w))),
            t => Err(self.unexpected(&t)),
        }
    }

    fn expect_op(&mut self, op: &str) -> PResult<()> {
        match self.next()? {
            Tok::Op(x) if x == op => Ok(()),
            Tok::Eof => Err(self.eof_error(&format!("`{}'", op))),
            t => Err(self.unexpected(&t)),
        }
    }

    /// Parses one complete command: everything up to an unquoted newline
    /// or the end of input. Returns None when only blank lines and
    /// comments remain. Here-document bodies are attached before returning.
    pub fn parse_complete_command(&mut self) -> PResult<Option<Node>> {
        self.skip_newlines()?;
        if matches!(self.peek()?, Tok::Eof) {
            return Ok(None);
        }
        let mut node = self.parse_list(true)?;
        match self.next()? {
            Tok::Newline | Tok::Eof => {}
            t => return Err(self.unexpected(&t)),
        }
        if self.lex.has_pending_heredocs() {
            return Err(self.eof_error("here-document body"));
        }
        let mut bodies = std::mem::take(&mut self.lex.heredoc_bodies);
        fill_heredocs(&mut node, &mut bodies);
        Ok(Some(node))
    }

    /// Parses all remaining commands into one list (used by tests).
    pub fn parse_program(&mut self) -> PResult<Vec<Node>> {
        let mut out = Vec::new();
        while let Some(n) = self.parse_complete_command()? {
            out.push(n);
        }
        Ok(out)
    }

    // list: and_or ((';' | '&') and_or)*
    // At top level the list ends at a newline; inside compound commands
    // newlines separate commands and the list ends at a terminator word.
    fn parse_list(&mut self, top_level: bool) -> PResult<Node> {
        let mut items: Vec<ListItem> = Vec::new();
        let mut saw_semicolon = false;
        loop {
            if !top_level {
                self.skip_newlines()?;
            }
            // zsh: `;` with no command before it is an empty command
            // (`do; ...`, `a; ; b`, `; b`).
            while self.peek_is_op(";")? {
                self.next()?;
                saw_semicolon = true;
                if !top_level {
                    self.skip_newlines()?;
                }
            }
            if top_level && matches!(self.peek()?, Tok::Newline | Tok::Eof) {
                break;
            }
            if self.at_list_end()? {
                break;
            }
            let mut node = self.parse_and_or()?;
            let mut background = false;
            match self.peek()? {
                Tok::Op(";") => {
                    self.next()?;
                }
                // `&|` and `&!` (zsh: background without job control).
                Tok::Op(op @ ("&" | "&|" | "&!")) => {
                    let disown = *op != "&";
                    self.next()?;
                    background = true;
                    if disown {
                        node = Node::Disowned(Box::new(node));
                    }
                    // zsh accepts `cmd &|;`.
                    if self.peek_is_op(";")? {
                        self.next()?;
                    }
                }
                Tok::Newline => {
                    if top_level {
                        items.push(ListItem { node, background });
                        break;
                    }
                }
                _ => {
                    items.push(ListItem { node, background });
                    break;
                }
            }
            items.push(ListItem { node, background });
            if top_level && matches!(self.peek()?, Tok::Newline | Tok::Eof) {
                break;
            }
        }
        if items.is_empty() {
            return match self.peek()? {
                // A line of just `;`.
                Tok::Newline | Tok::Eof if top_level && saw_semicolon => Ok(Node::Empty),
                Tok::Eof => Err(self.eof_error("a command")),
                t => {
                    let t = t.clone();
                    if top_level {
                        Err(self.unexpected(&t))
                    } else {
                        Ok(Node::Empty)
                    }
                }
            };
        }
        if items.len() == 1 && !items[0].background {
            return Ok(items.pop().unwrap().node);
        }
        Ok(Node::List(items))
    }

    fn at_list_end(&mut self) -> PResult<bool> {
        // An alias here may expand to a reserved word that ends the list
        // (`alias CLOSE='};'`).
        while self.try_alias()? {}
        Ok(match self.peek()? {
            Tok::Eof => true,
            Tok::Op(")") | Tok::Op(";;") | Tok::Op(";&") | Tok::Op(";|") => true,
            Tok::Word(w) => LIST_TERMINATORS.contains(&w.as_str()),
            _ => false,
        })
    }

    // and_or: pipeline (('&&' | '||') linebreak pipeline)*
    fn parse_and_or(&mut self) -> PResult<Node> {
        let first = self.parse_pipeline()?;
        let mut rest = Vec::new();
        loop {
            let op = match self.peek()? {
                Tok::Op("&&") => AndOrOp::And,
                Tok::Op("||") => AndOrOp::Or,
                _ => break,
            };
            self.next()?;
            self.skip_linebreak()?;
            rest.push((op, self.parse_pipeline()?));
        }
        if rest.is_empty() {
            return Ok(first);
        }
        Ok(Node::AndOr {
            first: Box::new(first),
            rest,
        })
    }

    // pipeline: ['!'] command ('|' linebreak command)*
    fn parse_pipeline(&mut self) -> PResult<Node> {
        // `!` and `time` may be aliases too.
        while self.try_alias()? {}
        // zsh's `time` reserved word times the pipeline after it.
        if self.peek_is_word("time")? && !crate::builtins::is_disabled('r', "time") {
            self.next()?;
            let inner = self.parse_pipeline()?;
            return Ok(Node::Time(Box::new(inner)));
        }
        // `{!}`: the word after `{` is `!}`, a `!` before the closing brace.
        if self.brace_depth > 0 && self.peek_is_word("!}")? {
            self.next()?;
            self.queued.push(Tok::Word("}".to_string()));
            self.peeked = Some(Tok::Word("!".to_string()));
            self.peeked_queued = true;
        }
        let mut bang = false;
        while self.peek_is_word("!")? {
            self.next()?;
            bang = !bang;
        }
        // zsh: `!` alone (before a separator) negates an empty command.
        if bang && self.at_command_end()? {
            return Ok(Node::Pipeline {
                cmds: Vec::new(),
                bang,
            });
        }
        let mut cmds = vec![self.parse_command()?];
        loop {
            let both = if self.peek_is_op("|")? {
                false
            } else if self.peek_is_op("|&")? {
                true
            } else {
                break;
            };
            self.next()?;
            // `a |& b` also sends a's standard error down the pipe.
            if both {
                let last = cmds.pop().unwrap();
                cmds.push(with_redir(
                    last,
                    Redir {
                        fd: 2,
                        op: RedirOp::DupOut,
                        target: "1".to_string(),
                        heredoc: None,
                        var: None,
                    },
                ));
            }
            self.skip_linebreak()?;
            cmds.push(self.parse_command()?);
        }
        if cmds.len() == 1 && !bang {
            return Ok(cmds.pop().unwrap());
        }
        Ok(Node::Pipeline { cmds, bang })
    }

    fn at_command_end(&mut self) -> PResult<bool> {
        Ok(match self.peek()? {
            Tok::Eof | Tok::Newline => true,
            Tok::Op(op) => matches!(
                *op,
                ";" | "&" | "&&" | "||" | ")" | ";;" | ";&" | ";|" | "&|" | "&!"
            ),
            Tok::Word(w) => w == "}",
            _ => false,
        })
    }

    fn alias_blocked(&self, name: &str) -> bool {
        let pos = self.lex.pos();
        self.alias_guard
            .iter()
            .any(|(n, end)| n == name && pos <= *end)
    }

    /// If the next token is an unquoted word naming an alias, replaces it
    /// with the alias text. Returns true if a substitution happened.
    fn try_alias(&mut self) -> PResult<bool> {
        self.try_alias_in(false)
    }

    // With `global`, only zsh global aliases (`alias -g`) apply.
    fn try_alias_in(&mut self, global: bool) -> PResult<bool> {
        let name = match self.peek()? {
            Tok::Word(w) => w.clone(),
            // zsh aliases tokens such as `&&` too.
            Tok::Op(op) => op.to_string(),
            _ => return Ok(false),
        };
        // POSIX_ALIASES: reserved words are never aliases.
        if !global
            && RESERVED_WORDS.contains(&name.as_str())
            && crate::options::is_set("posixaliases")
        {
            return Ok(false);
        }
        // A word split off another has no input position of its own.
        if self.peeked_queued || !self.queued.is_empty() {
            return Ok(false);
        }
        if self.alias_blocked(&name) {
            return Ok(false);
        }
        let found = if global {
            (self.galias_lookup)(&name)
        } else {
            (self.alias_lookup)(&name)
                .or_else(|| (self.galias_lookup)(&name))
                // `file.ext` runs the suffix alias for ext on the file.
                .or_else(|| crate::shell::salias_for(&name).map(|v| format!("{} {}", v, name)))
        };
        let Some(value) = found else {
            return Ok(false);
        };
        // `alias() { ... }` would define a function named after the alias's
        // text; zsh refuses unless ALIAS_FUNC_DEF.
        if !global && self.lex.next_after_blanks_is("()") && !crate::options::is_set("aliasfuncdef")
        {
            return Err(ParseError::Syntax(format!(
                "defining function based on alias `{}'",
                name
            )));
        }
        // The peeked token has been lexed; the lexer sits right after it.
        self.peeked = None;
        let pos = self.lex.pos();
        // A space keeps the text apart from a word the alias was split
        // from (`{end`); otherwise nothing is added, as a global alias may
        // open a quote the input closes (`alias -g S='"'`).
        let joined = self
            .lex
            .next_char()
            .is_some_and(|c| !c.is_whitespace() && !"|&;<>()".contains(c));
        let text = if joined {
            format!("{} ", value)
        } else {
            value.clone()
        };
        let len = text.chars().count();
        for (_, end) in self.alias_guard.iter_mut() {
            if *end >= pos {
                *end += len;
            }
        }
        self.lex.insert_text(&text);
        self.alias_guard.push((name, pos + len));
        self.alias_next = value.ends_with(' ') || value.ends_with('\t');
        Ok(true)
    }

    fn parse_command(&mut self) -> PResult<Node> {
        while self.try_alias()? {}
        let tok = self.peek()?.clone();
        let node = match &tok {
            // `disable -r word`: an ordinary command word.
            Tok::Word(w) if crate::builtins::is_disabled('r', w) => {
                return self.parse_simple_or_funcdef()
            }
            // zsh: `{cmd` at the start of a command opens a group.
            Tok::Word(w)
                if w.len() > 1 && w.starts_with('{') && !w.starts_with("{}") && braces_split() =>
            {
                let rest = w[1..].to_string();
                self.peeked = Some(Tok::Word("{".to_string()));
                self.queued.push(Tok::Word(rest));
                return self.parse_command();
            }
            Tok::Word(w) => match w.as_str() {
                "if" => self.parse_if()?,
                "while" => self.parse_loop(false)?,
                "until" => self.parse_loop(true)?,
                "for" => self.parse_for()?,
                "foreach" => self.parse_foreach()?,
                "repeat" => self.parse_repeat()?,
                "select" => self.parse_select()?,
                "coproc" => {
                    self.next()?;
                    let body = self.parse_command()?;
                    return Ok(Node::Coproc(Box::new(body)));
                }
                "case" => self.parse_case()?,
                "{" => {
                    self.next()?;
                    self.brace_depth += 1;
                    self.lex.brace_depth = self.brace_depth;
                    let body = self.parse_list(false);
                    self.brace_depth -= 1;
                    self.lex.brace_depth = self.brace_depth;
                    let body = body?;
                    self.expect_word("}")?;
                    if self.peek_is_word("always")? {
                        self.next()?;
                        self.skip_newlines()?;
                        if !self.peek_is_word("{")? {
                            let t = self.peek()?.clone();
                            return Err(self.unexpected(&t));
                        }
                        // Only the brace group: redirections after it
                        // apply to the whole construct.
                        self.next()?;
                        self.brace_depth += 1;
                        self.lex.brace_depth = self.brace_depth;
                        let always = self.parse_list(false);
                        self.brace_depth -= 1;
                        self.lex.brace_depth = self.brace_depth;
                        let always = always?;
                        self.expect_word("}")?;
                        Node::Always {
                            body: Box::new(Node::Brace(Box::new(body))),
                            always: Box::new(Node::Brace(Box::new(always))),
                        }
                    } else {
                        Node::Brace(Box::new(body))
                    }
                }
                "[[" => self.parse_dbracket()?,
                "function" => return self.parse_function_keyword(),
                "then" | "else" | "elif" | "fi" | "do" | "done" | "esac" | "}" | "in" | "end" => {
                    return Err(self.unexpected(&tok));
                }
                _ => return self.parse_simple_or_funcdef(),
            },
            Tok::Op("(") => {
                self.next()?;
                if self.peek_is_op(")")? {
                    return self.parse_anon_func();
                }
                let body = self.parse_list(false)?;
                self.expect_op(")")?;
                Node::Subshell(Box::new(body))
            }
            Tok::Arith(e) => {
                let e = e.clone();
                let line = self.lex.line();
                self.next()?;
                Node::Arith(e, line)
            }
            Tok::Eof => return Err(self.eof_error("a command")),
            Tok::IoNumber(_)
            | Tok::IoVar(_)
            | Tok::Op("<")
            | Tok::Op(">")
            | Tok::Op(">>")
            | Tok::Op("<<")
            | Tok::Op("<<-")
            | Tok::Op("<<<")
            | Tok::Op(">&|")
            | Tok::Op(">&!")
            | Tok::Op(">>&")
            | Tok::Op("&>>|")
            | Tok::Op("&>>!")
            | Tok::Op("<&")
            | Tok::Op(">&")
            | Tok::Op("<>")
            | Tok::Op(">|")
            | Tok::Op(">!")
            | Tok::Op(">>|")
            | Tok::Op(">>!")
            | Tok::Op("&>")
            | Tok::Op("&>>")
            | Tok::Op("&>|")
            | Tok::Op("&>!") => return self.parse_simple_or_funcdef(),
            _ => return Err(self.unexpected(&tok)),
        };
        // Redirections after a compound command apply to all of it.
        let mut redirs = Vec::new();
        while let Some(r) = self.try_parse_redirect()? {
            redirs.push(r);
        }
        if redirs.is_empty() {
            Ok(node)
        } else {
            Ok(Node::Redirected {
                node: Box::new(node),
                redirs,
            })
        }
    }

    // `() compound-command [args]`, after the `(`.
    // A function body; `{}` (one word) is an empty one.
    fn parse_func_body(&mut self) -> PResult<Node> {
        if self.peek_is_word("{}")? {
            self.next()?;
            return Ok(Node::Brace(Box::new(Node::Empty)));
        }
        self.parse_command()
    }

    fn parse_anon_func(&mut self) -> PResult<Node> {
        let line = self.lex.line();
        self.next()?; // )
                      // `( )` before a separator is an empty subshell; a newline may
                      // still come before an anonymous function's body.
        if !matches!(self.peek()?, Tok::Newline) && self.at_command_end()? {
            return Ok(Node::Subshell(Box::new(Node::Empty)));
        }
        self.anon_func_rest(line, false)
    }

    // The body of `() ...` or `function { ... }`, then its arguments and
    // redirections. An argument may start with a glob group: `(a|b)*`.
    fn anon_func_rest(&mut self, line: usize, traced: bool) -> PResult<Node> {
        self.skip_newlines()?;
        let mut body = self.parse_command()?;
        let mut args = Vec::new();
        let mut redirs = Vec::new();
        // `() { ... } >file`: the redirection is the call's, so it is still
        // in place while the function's EXIT trap runs.
        if let Node::Redirected { node, redirs: r } = body {
            if matches!(*node, Node::Brace(_)) {
                redirs = r;
                body = *node;
            } else {
                body = Node::Redirected { node, redirs: r };
            }
        }
        loop {
            if let Some(r) = self.try_parse_redirect()? {
                redirs.push(r);
                continue;
            }
            match self.peek()? {
                Tok::Word(w) if !LIST_TERMINATORS.contains(&w.as_str()) => {
                    let w = w.clone();
                    self.next()?;
                    args.push(w);
                }
                Tok::Op("(") => {
                    self.peeked = None;
                    match self.lex.glob_word_after_paren() {
                        Some(w) => args.push(w),
                        None => return Err(ParseError::Syntax("parse error near `('".into())),
                    }
                }
                _ => break,
            }
        }
        let node = Node::AnonFunc {
            body: std::sync::Arc::new(body),
            args,
            line,
            traced,
        };
        Ok(if redirs.is_empty() {
            node
        } else {
            Node::Redirected {
                node: Box::new(node),
                redirs,
            }
        })
    }

    fn try_parse_redirect(&mut self) -> PResult<Option<Redir>> {
        if let Some(r) = self.pending_redir.take() {
            return Ok(Some(r));
        }
        let mut fd = None;
        let mut var = None;
        if let Tok::IoNumber(n) = self.peek()? {
            fd = Some(*n);
            self.next()?;
        } else if let Tok::IoVar(name) = self.peek()? {
            var = Some(name.clone());
            fd = Some(-1);
            self.next()?;
        }
        let op = match self.peek()? {
            Tok::Op(op) => *op,
            t => {
                if fd.is_some() {
                    let t = t.clone();
                    return Err(self.unexpected(&t));
                }
                return Ok(None);
            }
        };
        let (rop, default_fd) = match op {
            "<" => (RedirOp::Read, 0),
            ">" => (RedirOp::Write, 1),
            ">|" | ">!" => (RedirOp::Clobber, 1),
            ">>" => (RedirOp::Append, 1),
            ">>|" | ">>!" => (RedirOp::AppendClobber, 1),
            "<>" => (RedirOp::ReadWrite, 0),
            "<&" => (RedirOp::DupIn, 0),
            ">&" => (RedirOp::DupOut, 1),
            "<<" | "<<-" => (RedirOp::HereDoc, 0),
            "<<<" => (RedirOp::HereStr, 0),
            "&>" | "&>>" | "&>|" | "&>!" | ">&|" | ">&!" | "&>>|" | "&>>!" | ">>&" | ">>&|"
            | ">>&!"
                if fd.is_none() =>
            {
                self.next()?;
                let target = match self.next()? {
                    Tok::Word(w) => w,
                    Tok::Eof => return Err(self.eof_error("a redirection target")),
                    t => return Err(self.unexpected(&t)),
                };
                self.pending_redir = Some(Redir {
                    fd: 2,
                    op: RedirOp::DupOut,
                    target: "1".to_string(),
                    heredoc: None,
                    var: None,
                });
                return Ok(Some(Redir {
                    fd: 1,
                    op: if op.contains(">>") && (op.ends_with('|') || op.ends_with('!')) {
                        RedirOp::AppendClobber
                    } else if op.contains(">>") {
                        RedirOp::Append
                    } else if op.ends_with('|') || op.ends_with('!') {
                        RedirOp::Clobber
                    } else {
                        RedirOp::Write
                    },
                    target,
                    heredoc: None,
                    var: None,
                }));
            }
            _ => {
                if fd.is_some() {
                    return Err(ParseError::Syntax(format!(
                        "syntax error near unexpected token `{}'",
                        op
                    )));
                }
                return Ok(None);
            }
        };
        self.next()?;
        let target = match self.next()? {
            Tok::Word(w) => w,
            Tok::Eof => return Err(self.eof_error("a redirection target")),
            t => return Err(self.unexpected(&t)),
        };
        let heredoc = if rop == RedirOp::HereDoc {
            let (id, expand) = self.lex.add_heredoc(&target, op == "<<-");
            Some(HereDoc {
                id,
                body: String::new(),
                expand,
            })
        } else {
            None
        };
        Ok(Some(Redir {
            fd: fd.unwrap_or(default_fd),
            op: rop,
            target,
            heredoc,
            var,
        }))
    }

    fn parse_simple_or_funcdef(&mut self) -> PResult<Node> {
        // The first word is already read: its line is where we start.
        let line = if self.peeked.is_some() && !self.peeked_queued {
            self.lex.tok_line()
        } else {
            self.lex.line()
        };
        let mut cmd = SimpleCmd {
            line,
            ..SimpleCmd::default()
        };
        let line = cmd.line;
        loop {
            if let Some(r) = self.try_parse_redirect()? {
                cmd.redirs.push(r);
                continue;
            }
            if cmd.words.is_empty() || self.alias_next {
                let check = self.alias_next || cmd.words.is_empty();
                self.alias_next = false;
                if check && self.try_alias()? {
                    continue;
                }
            }
            if !cmd.words.is_empty() && self.try_alias_in(true)? {
                continue;
            }
            // Declaration commands take array values: `typeset a=(x y)`.
            if cmd.words.first().is_some_and(|w| is_decl_word(w)) {
                if let Tok::Word(w) = self.peek()?.clone() {
                    if let Some((_, _, _, value)) = assignment_split(&w) {
                        if value.is_empty() {
                            self.next()?;
                            if self.peek_is_op("(")? {
                                self.next()?;
                                let mut items = Vec::new();
                                loop {
                                    match self.next()? {
                                        Tok::Op(")") => break,
                                        Tok::Newline => continue,
                                        Tok::Word(x) => items.push(x),
                                        Tok::Eof => return Err(self.eof_error("`)'")),
                                        t => return Err(self.unexpected(&t)),
                                    }
                                }
                                cmd.decl_arrays.push((cmd.words.len(), items));
                            }
                            cmd.words.push(w);
                            continue;
                        }
                    }
                }
            }
            // zsh: after a command word, `(...)` that is not `()` starts a
            // word with a glob group: `print (a|b)*`.
            if !cmd.words.is_empty()
                && self.peek_is_op("(")?
                && !self.peeked_queued
                && !self.lex.next_after_blanks_is(")")
            {
                self.peeked = None;
                match self.lex.glob_word_after_paren() {
                    Some(w) => {
                        cmd.words.push(w);
                        continue;
                    }
                    None => return Err(ParseError::Syntax("parse error near `('".into())),
                }
            }
            let word = match self.peek()? {
                Tok::Word(w) => w.clone(),
                _ => break,
            };
            // zsh: in a group, `cmd}` ends it (unless the braces pair up).
            if self.brace_depth > 0
                && word.len() > 1
                && ends_with_unpaired_close(&word)
                && braces_split()
            {
                self.peeked = Some(Tok::Word(word[..word.len() - 1].to_string()));
                self.queued.push(Tok::Word("}".to_string()));
                continue;
            }
            // After arguments only when braces are special (not with
            // IGNORE_BRACES, IGNORE_CLOSE_BRACES or in sh).
            if word == "}"
                && self.brace_depth > 0
                && !(cmd.words.is_empty() && cmd.assigns.is_empty() && cmd.redirs.is_empty())
                && braces_split()
                && !crate::options::is_set("ignoreclosebraces")
            {
                break;
            }
            if cmd.words.is_empty() {
                if let Some((name, subscript, append, value)) = assignment_split(&word) {
                    self.next()?;
                    // `name=(...)` array literal.
                    if value.is_empty() && self.peek_is_op("(")? {
                        self.next()?;
                        let mut items = Vec::new();
                        loop {
                            match self.next()? {
                                Tok::Op(")") => break,
                                Tok::Newline => continue,
                                Tok::Word(w) => items.push(w),
                                Tok::Eof => return Err(self.eof_error("`)'")),
                                t => return Err(self.unexpected(&t)),
                            }
                        }
                        cmd.assigns.push(Assign {
                            name,
                            subscript,
                            append,
                            value: AssignValue::Array(items),
                        });
                    } else {
                        cmd.assigns.push(Assign {
                            name,
                            subscript,
                            append,
                            value: AssignValue::Scalar(value),
                        });
                    }
                    continue;
                }
            }
            self.next()?;
            // zsh: name1 name2 () body defines both.
            if !cmd.words.is_empty()
                && cmd.assigns.is_empty()
                && cmd.redirs.is_empty()
                && cmd.words.iter().all(|w| is_func_name(w))
                && is_func_name(&word)
                && self.peek_is_op("(")?
                && (self.peeked_queued || self.lex.next_after_blanks_is(")"))
            {
                self.next()?;
                self.expect_op(")")?;
                self.skip_newlines()?;
                let body = self.parse_func_body()?;
                let mut names = std::mem::take(&mut cmd.words);
                names.push(word);
                let defs = names
                    .into_iter()
                    .map(|name| ListItem {
                        node: Node::FuncDef {
                            name,
                            body: Box::new(body.clone()),
                            line,
                            traced: false,
                        },
                        background: false,
                    })
                    .collect();
                return Ok(Node::List(defs));
            }
            // name ( ) compound-command
            if cmd.words.is_empty()
                && cmd.assigns.is_empty()
                && cmd.redirs.is_empty()
                && self.peek_is_op("(")?
                && (self.peeked_queued || self.lex.next_after_blanks_is(")"))
            {
                self.next()?;
                self.expect_op(")")?;
                if !is_func_name(&word) {
                    return Err(ParseError::Syntax(format!(
                        "`{}': not a valid function name",
                        word
                    )));
                }
                self.skip_newlines()?;
                let body = self.parse_func_body()?;
                return Ok(Node::FuncDef {
                    name: word,
                    body: Box::new(body),
                    line,
                    traced: false,
                });
            }
            cmd.words.push(word);
        }
        if cmd.words.is_empty() && cmd.assigns.is_empty() && cmd.redirs.is_empty() {
            let t = self.peek()?.clone();
            if t == Tok::Eof {
                return Err(self.eof_error("a command"));
            }
            return Err(self.unexpected(&t));
        }
        Ok(Node::Simple(cmd))
    }

    // function name [()] compound-command
    // function name... [()] compound-command; zsh allows several names
    // sharing one body.
    fn parse_function_keyword(&mut self) -> PResult<Node> {
        let line = self.lex.line();
        self.next()?;
        // `function -T name`: traced; `--` ends the options.
        let mut traced = false;
        loop {
            if self.peek_is_word("-T")? {
                self.next()?;
                traced = true;
            } else if self.peek_is_word("--")? {
                self.next()?;
                break;
            } else {
                break;
            }
        }
        // `function { ... } args`: an anonymous function.
        if self.peek_is_word("{")? {
            return self.anon_func_rest(line, traced);
        }
        let mut names = vec![match self.next()? {
            Tok::Word(w) => w,
            Tok::Eof => return Err(self.eof_error("a function name")),
            t => return Err(self.unexpected(&t)),
        }];
        while let Tok::Word(w) = self.peek()? {
            if w == "{" || !is_func_name(w) {
                break;
            }
            let w = w.clone();
            self.next()?;
            names.push(w);
        }
        if self.peek_is_op("(")? {
            self.next()?;
            self.expect_op(")")?;
        }
        self.skip_newlines()?;
        let body = self.parse_func_body()?;
        let mut defs: Vec<ListItem> = names
            .into_iter()
            .map(|name| ListItem {
                node: Node::FuncDef {
                    name,
                    body: Box::new(body.clone()),
                    line,
                    traced,
                },
                background: false,
            })
            .collect();
        if defs.len() == 1 {
            return Ok(defs.pop().unwrap().node);
        }
        Ok(Node::List(defs))
    }

    // if list then list [elif list then list]... [else list] fi
    // zsh also: if list { list } [elif list { list }]... [else { list }]
    // and, with SHORT_LOOPS, if list sublist.
    fn parse_if(&mut self) -> PResult<Node> {
        self.next()?; // if
        let mut branches = Vec::new();
        let mut else_body = None;
        loop {
            let cond = self.parse_list(false)?;
            if matches!(cond, Node::Empty) {
                let t = self.peek()?.clone();
                return Err(if t == Tok::Eof {
                    self.eof_error("`then'")
                } else {
                    self.unexpected(&t)
                });
            }
            if self.peek_is_word("then")? {
                self.next()?;
                let body = self.parse_list(false)?;
                branches.push((cond, body));
                loop {
                    self.skip_newlines()?;
                    match self.next()? {
                        Tok::Word(w) if w == "elif" => {
                            let cond = self.parse_list(false)?;
                            self.expect_word("then")?;
                            let body = self.parse_list(false)?;
                            branches.push((cond, body));
                        }
                        Tok::Word(w) if w == "else" => {
                            else_body = Some(Box::new(self.parse_list(false)?));
                            self.expect_word("fi")?;
                            break;
                        }
                        Tok::Word(w) if w == "fi" => break,
                        Tok::Eof => return Err(self.eof_error("`fi'")),
                        t => return Err(self.unexpected(&t)),
                    }
                }
                break;
            }
            if self.peek_is_word("{")? {
                let body = self.parse_command()?;
                branches.push((cond, body));
                self.skip_newlines_if_followed(&["elif", "else"])?;
                if self.peek_is_word("elif")? {
                    self.next()?;
                    continue;
                }
                if self.peek_is_word("else")? {
                    self.next()?;
                    self.skip_newlines()?;
                    else_body = Some(Box::new(self.parse_command()?));
                }
                break;
            }
            // Short form: the body is one sublist.
            if self.at_command_end()? {
                let t = self.peek()?.clone();
                return Err(if t == Tok::Eof {
                    self.eof_error("`then'")
                } else {
                    self.unexpected(&t)
                });
            }
            let body = self.parse_and_or()?;
            branches.push((cond, body));
            break;
        }
        Ok(Node::If {
            branches,
            else_body,
        })
    }

    // Skips newlines only when one of `words` follows them, so a brace
    // form `if c { } <newline> elif` continues while a following command
    // is left alone.
    fn skip_newlines_if_followed(&mut self, words: &[&str]) -> PResult<()> {
        if !matches!(self.peek()?, Tok::Newline) {
            return Ok(());
        }
        // Look past the newlines without consuming the next command.
        let rest: String = self.lex.rest();
        let trimmed = rest.trim_start_matches(['\n', ' ', '\t']);
        if words.iter().any(|w| {
            trimmed.starts_with(w)
                && !trimmed[w.len()..].starts_with(|c: char| c.is_alphanumeric() || c == '_')
        }) {
            self.skip_newlines()?;
        }
        Ok(())
    }

    // The body of a loop: `do list done`, `{ list }`, or a sublist (short
    // form).
    // `short`: the option allowing a body without `do`/`{` (SHORT_LOOPS,
    // or SHORT_REPEAT for repeat).
    fn parse_loop_body(&mut self) -> PResult<Node> {
        let short = crate::options::is_set("shortloops");
        self.parse_loop_body_with(short)
    }

    fn parse_loop_body_with(&mut self, short: bool) -> PResult<Node> {
        self.skip_newlines()?;
        if self.peek_is_word("do")? {
            self.next()?;
            let body = self.parse_list(false)?;
            self.expect_word("done")?;
            return Ok(body);
        }
        if self.peek_is_word("{")? {
            return self.parse_command();
        }
        // CSH_JUNKIE_LOOPS: `for x in a b; cmds; end`.
        if crate::options::is_set("cshjunkieloops") {
            let body = self.parse_list(false)?;
            self.expect_word("end")?;
            return Ok(body);
        }
        if self.at_command_end()? || !short {
            let t = self.peek()?.clone();
            return Err(if t == Tok::Eof {
                self.eof_error("`do'")
            } else {
                self.unexpected(&t)
            });
        }
        self.parse_and_or()
    }

    // repeat word body
    fn parse_repeat(&mut self) -> PResult<Node> {
        self.next()?;
        let count = match self.next()? {
            Tok::Word(w) => w,
            Tok::Arith(e) => format!("(({}))", e),
            Tok::Eof => return Err(self.eof_error("a count")),
            t => return Err(self.unexpected(&t)),
        };
        if self.peek_is_op(";")? {
            self.next()?;
        }
        let short = crate::options::is_set("shortloops") || crate::options::is_set("shortrepeat");
        let body = self.parse_loop_body_with(short)?;
        Ok(Node::Repeat {
            count,
            body: Box::new(body),
        })
    }

    // foreach name... ( words ) list end
    fn parse_foreach(&mut self) -> PResult<Node> {
        self.next()?;
        let mut vars = Vec::new();
        while let Tok::Word(w) = self.peek()?.clone() {
            self.next()?;
            vars.push(w);
        }
        self.expect_op("(")?;
        let mut words = Vec::new();
        loop {
            match self.next()? {
                Tok::Word(w) => words.push(w),
                Tok::Newline => {}
                Tok::Op(")") => break,
                Tok::Eof => return Err(self.eof_error("`)'")),
                t => return Err(self.unexpected(&t)),
            }
        }
        let body = self.parse_list(false)?;
        self.expect_word("end")?;
        Ok(Node::For {
            vars,
            words: Some(words),
            body: Box::new(body),
        })
    }

    // select name [in words]; body
    fn parse_select(&mut self) -> PResult<Node> {
        self.next()?;
        let var = match self.next()? {
            Tok::Word(w) if is_valid_name(&w) => w,
            Tok::Eof => return Err(self.eof_error("a variable name")),
            t => return Err(self.unexpected(&t)),
        };
        self.skip_newlines()?;
        let mut words = None;
        if self.peek_is_word("in")? {
            self.next()?;
            let mut list = Vec::new();
            loop {
                match self.peek()?.clone() {
                    Tok::Word(w) => {
                        self.next()?;
                        list.push(w);
                    }
                    Tok::Op(";") | Tok::Newline => {
                        self.next()?;
                        break;
                    }
                    _ => break,
                }
            }
            words = Some(list);
        } else if self.peek_is_op(";")? {
            self.next()?;
        }
        let body = self.parse_loop_body()?;
        Ok(Node::Select {
            var,
            words,
            body: Box::new(body),
        })
    }

    fn parse_loop(&mut self, until: bool) -> PResult<Node> {
        self.next()?;
        let cond = self.parse_list(false)?;
        let body = self.parse_loop_body()?;
        Ok(Node::Loop {
            cond: Box::new(cond),
            body: Box::new(body),
            until,
        })
    }

    fn parse_for(&mut self) -> PResult<Node> {
        self.next()?; // for
        if let Tok::Arith(e) = self.peek()? {
            let parts: Vec<String> = e.split(';').map(|p| p.trim().to_string()).collect();
            if parts.len() != 3 {
                return Err(ParseError::Syntax(format!(
                    "syntax error: `(({}))': expected three expressions",
                    e
                )));
            }
            self.next()?;
            if self.peek_is_op(";")? {
                self.next()?;
            }
            let body = self.parse_loop_body()?;
            let mut it = parts.into_iter();
            return Ok(Node::ForArith {
                init: it.next().unwrap(),
                cond: it.next().unwrap(),
                step: it.next().unwrap(),
                body: Box::new(body),
            });
        }
        // zsh also loops over positional parameters: `for 1 in ...`.
        let mut vars = vec![match self.next()? {
            Tok::Word(w) if is_valid_name(&w) || w.chars().all(|c| c.is_ascii_digit()) => w,
            Tok::Eof => return Err(self.eof_error("a variable name")),
            t => return Err(self.unexpected(&t)),
        }];
        // zsh: several names take that many words per iteration.
        while let Tok::Word(w) = self.peek()? {
            let is_var =
                is_valid_name(w) || (!w.is_empty() && w.chars().all(|c| c.is_ascii_digit()));
            if w == "in" || w == "do" || !is_var {
                break;
            }
            let w = w.clone();
            self.next()?;
            vars.push(w);
        }
        // zsh: `for x (a b c) body`
        if self.peek_is_op("(")? {
            self.next()?;
            let mut list = Vec::new();
            loop {
                match self.next()? {
                    Tok::Word(w) => list.push(w),
                    Tok::Newline => {}
                    Tok::Op(")") => break,
                    Tok::Eof => return Err(self.eof_error("`)'")),
                    t => return Err(self.unexpected(&t)),
                }
            }
            if self.peek_is_op(";")? {
                self.next()?;
            }
            let body = self.parse_loop_body()?;
            return Ok(Node::For {
                vars,
                words: Some(list),
                body: Box::new(body),
            });
        }
        self.skip_newlines()?;
        let mut words = None;
        if self.peek_is_word("in")? {
            self.next()?;
            let mut list = Vec::new();
            loop {
                match self.next()? {
                    Tok::Word(w) => list.push(w),
                    Tok::Op(";") | Tok::Newline => break,
                    Tok::Eof => return Err(self.eof_error("`do'")),
                    t => return Err(self.unexpected(&t)),
                }
            }
            words = Some(list);
        } else if self.peek_is_op(";")? {
            self.next()?;
        }
        let body = self.parse_loop_body()?;
        Ok(Node::For {
            vars,
            words,
            body: Box::new(body),
        })
    }

    fn parse_case(&mut self) -> PResult<Node> {
        self.next()?; // case
        let word = match self.next()? {
            Tok::Word(w) => w,
            Tok::Eof => return Err(self.eof_error("a word")),
            t => return Err(self.unexpected(&t)),
        };
        self.skip_newlines()?;
        // zsh: `case word { ... }` as well as `case word in ... esac`.
        let close = if self.peek_is_word("{")? {
            self.next()?;
            "}"
        } else {
            self.expect_word("in")?;
            "esac"
        };
        let mut items = Vec::new();
        loop {
            self.skip_newlines()?;
            if self.peek_is_word(close)? {
                self.next()?;
                break;
            }
            // The patterns are read as raw text, as zsh's lexer reads them,
            // so the token just peeked is read again.
            if self.peeked_queued {
                let t = self.next()?;
                return Err(self.unexpected(&t));
            }
            self.peeked = None;
            self.lex.unread_token();
            let patterns = self.lex.case_patterns()?;
            let body = self.parse_list(false)?;
            let body = match body {
                Node::Empty => None,
                b => Some(b),
            };
            self.skip_newlines()?;
            let (mut fallthrough, mut cont) = (false, false);
            match self.peek()?.clone() {
                Tok::Op(";;") => {
                    self.next()?;
                }
                Tok::Op(";&") => {
                    self.next()?;
                    fallthrough = true;
                }
                Tok::Op(";|") => {
                    self.next()?;
                    cont = true;
                }
                Tok::Word(w) if w == close => {}
                Tok::Eof => return Err(self.eof_error(&format!("`{}'", close))),
                t => return Err(self.unexpected(&t)),
            }
            items.push(CaseItem {
                patterns,
                body,
                fallthrough,
                cont,
            });
        }
        Ok(Node::Case { word, items })
    }

    // [[ expr ]]: operands and operators are kept as raw words and
    // evaluated by the executor.
    fn parse_dbracket(&mut self) -> PResult<Node> {
        let line = self.lex.line();
        self.next()?; // [[
        let mut args = Vec::new();
        loop {
            match self.next()? {
                Tok::Word(w) if w == "]]" => break,
                Tok::Word(w) => args.push(w),
                Tok::Op(op) => args.push(op.to_string()),
                Tok::IoNumber(n) => args.push(n.to_string()),
                Tok::IoVar(v) => args.push(format!("{{{}}}", v)),
                Tok::Arith(e) => {
                    args.push("(".into());
                    args.push(format!("({})", e));
                    args.push(")".into());
                }
                Tok::Newline => {}
                Tok::Eof => return Err(self.eof_error("`]]'")),
            }
        }
        Ok(Node::DBracket(args, line))
    }
}

/// Commands whose arguments may be array assignments.
pub fn is_decl_word(w: &str) -> bool {
    matches!(
        w,
        "typeset" | "declare" | "local" | "export" | "readonly" | "integer" | "float" | "private"
    ) && !crate::builtins::is_disabled('r', w)
}

fn is_func_name(s: &str) -> bool {
    !s.is_empty() && !s.contains(['=', '/']) && !LIST_TERMINATORS.contains(&s)
}

fn with_redir(node: Node, r: Redir) -> Node {
    match node {
        Node::Simple(mut c) => {
            c.redirs.push(r);
            Node::Simple(c)
        }
        Node::Redirected { node, mut redirs } => {
            redirs.push(r);
            Node::Redirected { node, redirs }
        }
        n => Node::Redirected {
            node: Box::new(n),
            redirs: vec![r],
        },
    }
}

fn tok_text(t: &Tok) -> String {
    match t {
        Tok::Word(w) => w.clone(),
        Tok::IoNumber(n) => n.to_string(),
        Tok::IoVar(v) => format!("{{{}}}", v),
        Tok::Op(o) => o.to_string(),
        Tok::Arith(e) => format!("(({}))", e),
        Tok::Newline => "\\n".to_string(),
        Tok::Eof => "end of file".to_string(),
    }
}

fn fill_redirs(redirs: &mut [Redir], bodies: &mut HashMap<usize, String>) {
    for r in redirs {
        if let Some(hd) = r.heredoc.as_mut() {
            if let Some(body) = bodies.remove(&hd.id) {
                hd.body = body;
            }
        }
    }
}

fn fill_heredocs(node: &mut Node, bodies: &mut HashMap<usize, String>) {
    if bodies.is_empty() {
        return;
    }
    match node {
        Node::Simple(cmd) => fill_redirs(&mut cmd.redirs, bodies),
        Node::Pipeline { cmds, .. } => cmds.iter_mut().for_each(|c| fill_heredocs(c, bodies)),
        Node::AndOr { first, rest } => {
            fill_heredocs(first, bodies);
            rest.iter_mut().for_each(|(_, n)| fill_heredocs(n, bodies));
        }
        Node::List(items) => items
            .iter_mut()
            .for_each(|i| fill_heredocs(&mut i.node, bodies)),
        Node::Subshell(b) | Node::Brace(b) | Node::Disowned(b) => fill_heredocs(b, bodies),
        Node::Always { body, always } => {
            fill_heredocs(body, bodies);
            fill_heredocs(always, bodies);
        }
        Node::If {
            branches,
            else_body,
        } => {
            for (c, b) in branches {
                fill_heredocs(c, bodies);
                fill_heredocs(b, bodies);
            }
            if let Some(e) = else_body {
                fill_heredocs(e, bodies);
            }
        }
        Node::Loop { cond, body, .. } => {
            fill_heredocs(cond, bodies);
            fill_heredocs(body, bodies);
        }
        Node::For { body, .. }
        | Node::ForArith { body, .. }
        | Node::Repeat { body, .. }
        | Node::Select { body, .. }
        | Node::Coproc(body)
        | Node::Time(body) => fill_heredocs(body, bodies),
        Node::Case { items, .. } => {
            for item in items {
                if let Some(b) = item.body.as_mut() {
                    fill_heredocs(b, bodies);
                }
            }
        }
        Node::FuncDef { body, .. } => fill_heredocs(body, bodies),
        Node::Redirected { node, redirs } => {
            fill_heredocs(node, bodies);
            fill_redirs(redirs, bodies);
        }
        Node::AnonFunc { body, .. } => {
            if let Some(b) = std::sync::Arc::get_mut(body) {
                fill_heredocs(b, bodies);
            }
        }
        Node::DBracket(..) | Node::Arith(..) | Node::Empty => {}
    }
}

// Whether `word` ends with a `}` outside quotes that no unquoted `{`
// in it opened: zsh then reads that `}` as the end of a brace group
// (`{ echo 'a'}`).
fn ends_with_unpaired_close(word: &str) -> bool {
    let c: Vec<char> = word.chars().collect();
    let (mut depth, mut i) = (0i32, 0);
    let mut last_unquoted_close = false;
    while i < c.len() {
        last_unquoted_close = false;
        match c[i] {
            '\\' => i += 1,
            '\'' => {
                // $'...' allows backslash escapes inside.
                let ansi = i > 0 && c[i - 1] == '$';
                i += 1;
                while i < c.len() && c[i] != '\'' {
                    if ansi && c[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            '"' => {
                i += 1;
                while i < c.len() && c[i] != '"' {
                    if c[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                last_unquoted_close = true;
            }
            _ => {}
        }
        i += 1;
    }
    last_unquoted_close && depth < 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_one(s: &str) -> Node {
        let _g = crate::zsh_test_guard();
        let mut p = Parser::without_aliases(s, true);
        p.parse_complete_command().unwrap().unwrap()
    }

    #[test]
    fn and_or_is_left_associative() {
        match parse_one("false && echo a || echo b") {
            Node::AndOr { rest, .. } => {
                assert_eq!(rest.len(), 2);
                assert_eq!(rest[0].0, AndOrOp::And);
                assert_eq!(rest[1].0, AndOrOp::Or);
            }
            n => panic!("unexpected {:?}", n),
        }
    }

    #[test]
    fn heredoc_body_read_after_newline() {
        match parse_one("cat <<EOF | tr a-z A-Z\nabc\nEOF\n") {
            Node::Pipeline { cmds, .. } => match &cmds[0] {
                Node::Simple(c) => {
                    let hd = c.redirs[0].heredoc.as_ref().unwrap();
                    assert_eq!(hd.body, "abc\n");
                    assert!(hd.expand);
                }
                n => panic!("unexpected {:?}", n),
            },
            n => panic!("unexpected {:?}", n),
        }
    }

    #[test]
    fn quoted_heredoc_delimiter_disables_expansion() {
        match parse_one("cat <<'EOF'\n$x\nEOF\n") {
            Node::Simple(c) => {
                let hd = c.redirs[0].heredoc.as_ref().unwrap();
                assert_eq!(hd.body, "$x\n");
                assert!(!hd.expand);
            }
            n => panic!("unexpected {:?}", n),
        }
    }

    #[test]
    fn case_alternation() {
        match parse_one("case a in a|b) echo alt;; esac") {
            Node::Case { items, .. } => assert_eq!(items[0].patterns, vec!["a", "b"]),
            n => panic!("unexpected {:?}", n),
        }
    }

    #[test]
    fn incomplete_constructs() {
        for s in [
            "if true; then\n",
            "for i in 1 2\n",
            "echo a &&\n",
            "cat <<EOF\nabc\n",
            "{ echo\n",
        ] {
            let mut p = Parser::without_aliases(s, false);
            assert!(
                matches!(p.parse_complete_command(), Err(ParseError::Incomplete)),
                "{:?}",
                s
            );
        }
    }

    #[test]
    fn syntax_errors_when_final() {
        let mut p = Parser::without_aliases("if true; then\n", true);
        assert!(matches!(
            p.parse_complete_command(),
            Err(ParseError::Syntax(_))
        ));
        let mut p = Parser::without_aliases("fi\n", true);
        assert!(matches!(
            p.parse_complete_command(),
            Err(ParseError::Syntax(_))
        ));
    }

    #[test]
    fn arithmetic_command_and_c_style_for() {
        assert!(matches!(parse_one("(( x > 1 ))"), Node::Arith(e, _) if e == " x > 1 "));
        assert!(matches!(
            parse_one("for ((i=0; i<3; i++)); do echo $i; done"),
            Node::ForArith { .. }
        ));
        assert!(matches!(parse_one("((echo a); echo b)"), Node::Subshell(_)));
    }

    #[test]
    fn zsh_short_for_anonymous_function_and_both_redirect() {
        assert!(matches!(parse_one("for x (a b) echo $x"), Node::For { .. }));
        assert!(
            matches!(parse_one("for k v (a 1 b 2); do :; done"), Node::For { vars, .. } if vars.len() == 2)
        );
        assert!(
            matches!(parse_one("() { echo $1 } a"), Node::AnonFunc { args, .. } if args == vec!["a"])
        );
        match parse_one("cmd &>out") {
            Node::Simple(c) => assert_eq!(c.redirs.len(), 2),
            n => panic!("{:?}", n),
        }
    }

    #[test]
    fn case_patterns_read_as_zsh_reads_them() {
        let pats = |src: &str| match parse_one(src) {
            Node::Case { items, .. } => items.into_iter().map(|i| i.patterns).collect::<Vec<_>>(),
            n => panic!("{:?}", n),
        };
        // Blanks and `|` inside a group belong to the pattern.
        assert_eq!(
            pats("case x in ( no | (grumph) ) :;; esac"),
            [vec!["no", "(grumph)"]]
        );
        assert_eq!(
            pats("case x in (*with sep*)) :;; esac"),
            [vec!["(*with sep*)"]]
        );
        assert_eq!(
            pats("case x in (*with sep*) :;; esac"),
            [vec!["*with sep*"]]
        );
        assert_eq!(
            pats("case x in ([a-m])(|[n-z])rr) :;; esac"),
            [vec!["([a-m])(|[n-z])rr"]]
        );
        // Empty alternatives match the empty string.
        assert_eq!(
            pats("case x in ||jurgle) :;; esac"),
            [vec!["", "", "jurgle"]]
        );
        // Quoted `)` and `|` don't end or split a pattern.
        assert_eq!(
            pats("case x in 'a)'|\"b|c\") :;; esac"),
            [vec!["'a)'", "\"b|c\""]]
        );
    }

    #[test]
    fn case_patterns_with_zsh_groups() {
        match parse_one("case x in (a|b)*) echo g;; (c) echo p;; plugin::(x|y)) echo m;; esac") {
            Node::Case { items, .. } => {
                assert_eq!(items[0].patterns, vec!["(a|b)*"]);
                assert_eq!(items[1].patterns, vec!["c"]);
                assert_eq!(items[2].patterns, vec!["plugin::(x|y)"]);
            }
            n => panic!("{:?}", n),
        }
    }

    #[test]
    fn closing_brace_ends_command_inside_group() {
        assert!(matches!(parse_one("{ echo hi }"), Node::Brace(_)));
        assert!(matches!(
            parse_one("f() { echo a b }"),
            Node::FuncDef { .. }
        ));
        match parse_one("echo }") {
            Node::Simple(c) => assert_eq!(c.words, vec!["echo", "}"]),
            n => panic!("{:?}", n),
        }
    }

    #[test]
    fn redirections_on_compound_commands() {
        match parse_one("{ echo hi; } > out") {
            Node::Redirected { redirs, .. } => assert_eq!(redirs[0].target, "out"),
            n => panic!("unexpected {:?}", n),
        }
    }
}
