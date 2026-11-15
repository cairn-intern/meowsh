//! Turns parsed commands back into source text the way zsh prints
//! function definitions (`functions`, `whence -f`, `typeset -f`):
//! one command per line, tab indentation (or N spaces with -xN).

use crate::types::{AndOrOp, AssignValue, Node, Redir, RedirOp, SimpleCmd};

pub struct Deparser {
    out: String,
    indent: usize,
    unit: String,
    // Here-document bodies waiting for the end of the current line.
    pending: Vec<(String, String)>,
}

impl Deparser {
    pub fn new(spaces: Option<usize>) -> Self {
        Deparser {
            out: String::new(),
            indent: 0,
            unit: match spaces {
                Some(n) => " ".repeat(n),
                None => "\t".to_string(),
            },
            pending: Vec::new(),
        }
    }

    fn newline(&mut self) {
        self.out.push('\n');
        for (body, delim) in std::mem::take(&mut self.pending) {
            self.out.push_str(&body);
            self.out.push_str(&delim);
            self.out.push('\n');
        }
        for _ in 0..self.indent {
            self.out.push_str(&self.unit);
        }
    }

    fn push(&mut self, s: &str) {
        self.out.push_str(s);
    }

    /// `name () {\n\tbody\n}` for a function.
    pub fn function(mut self, name: &str, body: &Node) -> String {
        self.push(name);
        self.push(" () {");
        self.indent += 1;
        self.body(body);
        self.indent -= 1;
        self.newline();
        self.push("}");
        self.out
    }

    /// A function body alone, one command per line.
    pub fn body_text(mut self, body: &Node) -> String {
        self.node(body);
        self.out
    }

    // The inside of a brace group: each command on its own line.
    fn body(&mut self, node: &Node) {
        let inner = match node {
            Node::Brace(b) => b.as_ref(),
            n => n,
        };
        match inner {
            Node::List(items) => {
                for item in items {
                    self.newline();
                    self.node(&item.node);
                    if item.background {
                        self.push(" &");
                    }
                }
            }
            Node::Empty => {}
            n => {
                self.newline();
                self.node(n);
            }
        }
    }

    fn block(&mut self, node: &Node) {
        self.indent += 1;
        self.body(node);
        self.indent -= 1;
    }

    fn node(&mut self, node: &Node) {
        match node {
            Node::Simple(c) => self.simple(c),
            Node::Pipeline { cmds, bang } => {
                if *bang {
                    self.push(if cmds.is_empty() { "!" } else { "! " });
                }
                for (k, c) in cmds.iter().enumerate() {
                    if k > 0 {
                        self.push(" | ");
                    }
                    self.node(c);
                }
            }
            Node::AndOr { first, rest } => {
                self.node(first);
                for (op, n) in rest {
                    self.push(if *op == AndOrOp::And { " && " } else { " || " });
                    self.node(n);
                }
            }
            Node::List(items) => {
                for (k, item) in items.iter().enumerate() {
                    if k > 0 {
                        self.newline();
                    }
                    self.node(&item.node);
                    if item.background {
                        self.push(" &");
                    }
                }
            }
            Node::Subshell(b) => {
                self.push("(");
                self.block(b);
                self.newline();
                self.push(")");
            }
            Node::Brace(b) => {
                self.push("{");
                self.block(b);
                self.newline();
                self.push("}");
            }
            Node::If {
                branches,
                else_body,
            } => {
                for (k, (cond, body)) in branches.iter().enumerate() {
                    self.push(if k == 0 { "if " } else { "elif " });
                    self.inline_list(cond);
                    self.newline();
                    self.push("then");
                    self.block(body);
                    self.newline();
                }
                if let Some(e) = else_body {
                    self.push("else");
                    self.block(e);
                    self.newline();
                }
                self.push("fi");
            }
            Node::Loop { cond, body, until } => {
                self.push(if *until { "until " } else { "while " });
                self.inline_list(cond);
                self.newline();
                self.push("do");
                self.block(body);
                self.newline();
                self.push("done");
            }
            Node::Repeat { count, body } => {
                self.push(&format!("repeat {}", count));
                self.newline();
                self.push("do");
                self.block(body);
                self.newline();
                self.push("done");
            }
            Node::Select { var, words, body } => {
                self.push("select ");
                self.push(var);
                if let Some(ws) = words {
                    self.push(" in");
                    for w in ws {
                        self.push(" ");
                        self.push(w);
                    }
                }
                self.newline();
                self.push("do");
                self.block(body);
                self.newline();
                self.push("done");
            }
            Node::Coproc(b) => {
                self.push("coproc ");
                self.node(b);
            }
            Node::Arith(e) => {
                self.push("((");
                self.push(e);
                self.push("))");
            }
            Node::Disowned(n) => {
                self.node(n);
                self.push(" &|");
            }
            Node::Always { body, always } => {
                self.node(body);
                self.push(" always ");
                self.node(always);
            }
            Node::AnonFunc { body, args } => {
                self.push("() {");
                self.block(body);
                self.newline();
                self.push("}");
                for a in args {
                    self.push(" ");
                    self.push(a);
                }
            }
            Node::ForArith {
                init,
                cond,
                step,
                body,
            } => {
                self.push(&format!("for (({}; {}; {})) do", init, cond, step));
                self.block(body);
                self.newline();
                self.push("done");
            }
            Node::For { vars, words, body } => {
                self.push("for ");
                self.push(&vars.join(" "));
                if let Some(ws) = words {
                    self.push(" in");
                    for w in ws {
                        self.push(" ");
                        self.push(w);
                    }
                }
                self.newline();
                self.push("do");
                self.block(body);
                self.newline();
                self.push("done");
            }
            Node::Case { word, items } => {
                self.push(&format!("case {} in", word));
                self.indent += 1;
                for item in items {
                    self.newline();
                    self.push(&format!("({}) ", item.patterns.join("|")));
                    if let Some(b) = &item.body {
                        self.inline_list(b);
                        self.push(" ");
                    }
                    self.push(if item.fallthrough {
                        ";&"
                    } else if item.cont {
                        ";|"
                    } else {
                        ";;"
                    });
                }
                self.indent -= 1;
                self.newline();
                self.push("esac");
            }
            Node::FuncDef { name, body } => {
                self.push(name);
                self.push(" () {");
                self.block(body);
                self.newline();
                self.push("}");
            }
            Node::DBracket(words) => {
                self.push("[[ ");
                self.push(&words.join(" "));
                self.push(" ]]");
            }
            Node::Redirected { node, redirs } => {
                self.node(node);
                self.redirs(redirs);
            }
            Node::Empty => {}
        }
    }

    // A list on one line (conditions of if/while, case bodies).
    fn inline_list(&mut self, node: &Node) {
        match node {
            Node::List(items) => {
                for (k, item) in items.iter().enumerate() {
                    if k > 0 {
                        self.push("; ");
                    }
                    self.node(&item.node);
                    if item.background {
                        self.push(" &");
                    }
                }
            }
            n => self.node(n),
        }
    }

    fn simple(&mut self, c: &SimpleCmd) {
        let mut parts: Vec<String> = Vec::new();
        for a in &c.assigns {
            let sub = a
                .subscript
                .as_ref()
                .map(|s| format!("[{}]", s))
                .unwrap_or_default();
            let op = if a.append { "+=" } else { "=" };
            let v = match &a.value {
                AssignValue::Scalar(v) => v.clone(),
                AssignValue::Array(items) => format!("({})", items.join(" ")),
            };
            parts.push(format!("{}{}{}{}", a.name, sub, op, v));
        }
        for (k, w) in c.words.iter().enumerate() {
            match c.decl_arrays.iter().find(|(i, _)| *i == k) {
                Some((_, items)) => parts.push(format!("{}({})", w, items.join(" "))),
                None => parts.push(w.clone()),
            }
        }
        self.push(&parts.join(" "));
        // zsh leaves a space after assignment-only and declaration
        // commands.
        if c.words.is_empty() && !c.assigns.is_empty()
            || c.words
                .first()
                .is_some_and(|w| crate::parser::is_decl_word(w))
        {
            self.push(" ");
        }
        self.redirs(&c.redirs);
    }

    fn redirs(&mut self, redirs: &[Redir]) {
        for r in redirs {
            let (op, default_fd) = match r.op {
                RedirOp::Read => ("<", 0),
                RedirOp::Write => (">", 1),
                RedirOp::Clobber => (">|", 1),
                RedirOp::Append => (">>", 1),
                RedirOp::ReadWrite => ("<>", 0),
                RedirOp::DupIn => ("<&", 0),
                RedirOp::DupOut => (">&", 1),
                RedirOp::HereDoc => ("<<", 0),
                RedirOp::HereStr => ("<<<", 0),
            };
            self.push(" ");
            if r.fd != default_fd {
                self.push(&r.fd.to_string());
            }
            self.push(op);
            if r.op != RedirOp::DupIn && r.op != RedirOp::DupOut {
                self.push(" ");
            }
            self.push(&r.target);
            if let Some(h) = &r.heredoc {
                let (delim, _) = crate::lexer::unquote_delimiter(&r.target);
                self.pending.push((h.body.clone(), delim));
            }
        }
    }
}

/// zsh's `functions name` output.
pub fn function_text(name: &str, body: &Node, spaces: Option<usize>) -> String {
    let mut s = Deparser::new(spaces).function(name, body);
    s.push('\n');
    s
}
