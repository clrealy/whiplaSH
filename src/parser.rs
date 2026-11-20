//! Tokenizer + recursive-descent parser for bash syntax, plus fish-isms:
//! blocks may close with `end`, `if`/`while` work without `then`/`do`,
//! `function name; ...; end`, `begin; ...; end`, and `(cmd)` substitution.

use crate::ast::*;

#[derive(Debug, PartialEq)]
pub enum ParseError {
    /// input ended too early — interactive mode should read another line
    Incomplete,
    Syntax(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Op(&'static str),
    Redir {
        fd: Option<i32>,
        op: &'static str,
        heredoc: Option<usize>,
    },
    Arith(String),
    Newline,
    Eof,
}

const OPS: [&str; 10] = ["&&", "||", ";;", "|&", "|", ";", "&", "(", ")", "\n"];
const REDIRS: [&str; 11] = [
    "<<<", "<<-", "&>>", "<<", ">>", "<>", ">&", "<&", "&>", "<", ">",
];

/// Words after which the next word starts a new command.
const LEADS: [&str; 13] = [
    "if", "then", "elif", "else", "while", "until", "do", "!", "{", "begin", "and", "or", "not",
];

struct Lexer<'a> {
    s: &'a [u8],
    i: usize,
    cmd_pos: bool,
    pending_heredocs: Vec<(usize, String, bool)>, // (index, delimiter, strip tabs)
    heredocs: Vec<String>,
    /// last token was "(" — so "()" (function definition) leaves us at command position
    after_lparen: bool,
}

fn is_name_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

/// Skip a balanced construct starting at s[i] == open; returns index after close.
fn skip_balanced(s: &[u8], mut i: usize, open: u8, close: u8) -> Result<usize, ParseError> {
    let mut depth = 0;
    while i < s.len() {
        let c = s[i];
        match c {
            b'\\' => {
                i += 2;
                continue;
            }
            b'\'' => {
                i = skip_single(s, i)?;
                continue;
            }
            b'"' => {
                i = skip_double(s, i)?;
                continue;
            }
            b'`' => {
                i = skip_backtick(s, i)?;
                continue;
            }
            b'#' if depth > 0
                && open == b'('
                && (i == 0 || matches!(s[i - 1], b' ' | b'\t' | b'\n')) =>
            {
                while i < s.len() && s[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            _ => {}
        }
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return Ok(i + 1);
            }
        }
        i += 1;
    }
    Err(ParseError::Incomplete)
}

fn skip_single(s: &[u8], i: usize) -> Result<usize, ParseError> {
    match s[i + 1..].iter().position(|&c| c == b'\'') {
        Some(p) => Ok(i + 1 + p + 1),
        None => Err(ParseError::Incomplete),
    }
}

fn skip_backtick(s: &[u8], mut i: usize) -> Result<usize, ParseError> {
    i += 1;
    while i < s.len() {
        match s[i] {
            b'\\' => i += 2,
            b'`' => return Ok(i + 1),
            _ => i += 1,
        }
    }
    Err(ParseError::Incomplete)
}

fn skip_double(s: &[u8], mut i: usize) -> Result<usize, ParseError> {
    i += 1;
    while i < s.len() {
        match s[i] {
            b'\\' => i += 2,
            b'"' => return Ok(i + 1),
            b'$' if s.get(i + 1) == Some(&b'(') => i = skip_balanced(s, i + 1, b'(', b')')?,
            b'$' if s.get(i + 1) == Some(&b'{') => i = skip_balanced(s, i + 1, b'{', b'}')?,
            b'`' => i = skip_backtick(s, i)?,
            _ => i += 1,
        }
    }
    Err(ParseError::Incomplete)
}

fn is_meta(c: u8) -> bool {
    matches!(
        c,
        b' ' | b'\t' | b'\n' | b';' | b'&' | b'|' | b'<' | b'>' | b'(' | b')'
    )
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Lexer {
            s: src.as_bytes(),
            i: 0,
            cmd_pos: true,
            pending_heredocs: Vec::new(),
            heredocs: Vec::new(),
            after_lparen: false,
        }
    }

    fn starts(&self, p: &str) -> bool {
        self.s[self.i..].starts_with(p.as_bytes())
    }

    fn read_heredoc_bodies(&mut self) -> Result<(), ParseError> {
        for (idx, delim, strip) in std::mem::take(&mut self.pending_heredocs) {
            let mut body = String::new();
            let mut found = false;
            while self.i < self.s.len() {
                let end = self.s[self.i..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map(|p| self.i + p)
                    .unwrap_or(self.s.len());
                let mut line = String::from_utf8_lossy(&self.s[self.i..end]).into_owned();
                self.i = (end + 1).min(self.s.len());
                if strip {
                    line = line.trim_start_matches('\t').to_string();
                }
                if line == delim {
                    found = true;
                    break;
                }
                body.push_str(&line);
                body.push('\n');
            }
            if !found {
                return Err(ParseError::Incomplete);
            }
            self.heredocs[idx] = body;
        }
        Ok(())
    }

    fn next(&mut self) -> Result<Tok, ParseError> {
        let t = self.next_inner();
        if !matches!(t, Ok(Tok::Op("("))) {
            self.after_lparen = false;
        }
        t
    }

    fn next_inner(&mut self) -> Result<Tok, ParseError> {
        // skip blanks, comments and line continuations
        loop {
            while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t') {
                self.i += 1;
            }
            if self.starts("\\\n") {
                self.i += 2;
                continue;
            }
            if self.i < self.s.len() && self.s[self.i] == b'#' {
                while self.i < self.s.len() && self.s[self.i] != b'\n' {
                    self.i += 1;
                }
            }
            break;
        }
        if self.i >= self.s.len() {
            if !self.pending_heredocs.is_empty() {
                return Err(ParseError::Incomplete);
            }
            return Ok(Tok::Eof);
        }
        let c = self.s[self.i];
        if c == b'\n' {
            self.i += 1;
            self.read_heredoc_bodies()?;
            self.cmd_pos = true;
            return Ok(Tok::Newline);
        }
        // (( arithmetic ))
        if self.cmd_pos && self.starts("((") {
            let end = skip_balanced(self.s, self.i, b'(', b')')?;
            let inner = &self.s[self.i + 2..end.saturating_sub(2).max(self.i + 2)];
            self.i = end;
            self.cmd_pos = false;
            return Ok(Tok::Arith(String::from_utf8_lossy(inner).into_owned()));
        }
        // redirections, with an optional fd number in front
        let mut j = self.i;
        while j < self.s.len() && self.s[j].is_ascii_digit() {
            j += 1;
        }
        for r in REDIRS {
            if self.s[j..].starts_with(r.as_bytes()) && (j == self.i || !r.starts_with('&')) {
                let fd = if j > self.i {
                    std::str::from_utf8(&self.s[self.i..j])
                        .ok()
                        .and_then(|x| x.parse().ok())
                } else {
                    None
                };
                self.i = j + r.len();
                let mut heredoc = None;
                if r == "<<" || r == "<<-" {
                    // peek the delimiter word now so we know what ends the body
                    let save = self.i;
                    let save_pos = self.cmd_pos;
                    if let Tok::Word(w) = self.next()? {
                        let quoted = w.contains(['\'', '"', '\\']);
                        let delim: String = w
                            .chars()
                            .filter(|c| !matches!(c, '\'' | '"' | '\\'))
                            .collect();
                        let idx = self.heredocs.len();
                        self.heredocs.push(String::new());
                        self.pending_heredocs.push((idx, delim, r == "<<-"));
                        heredoc = Some(idx);
                        let _ = quoted;
                    }
                    self.i = save;
                    self.cmd_pos = save_pos;
                }
                return Ok(Tok::Redir { fd, op: r, heredoc });
            }
        }
        // `name()` in a function definition: emit the parens as operators
        for op in OPS {
            if self.starts(op) {
                if op == "(" && !self.cmd_pos {
                    // fish-style (cmd) substitution, unless it's an empty "()"
                    let mut k = self.i + 1;
                    while k < self.s.len() && matches!(self.s[k], b' ' | b'\t') {
                        k += 1;
                    }
                    if self.s.get(k) != Some(&b')') {
                        break; // fall through to word scanning
                    }
                }
                self.i += op.len();
                // after "name()" the function body follows: that's command position
                self.cmd_pos = op != ")" || self.after_lparen;
                self.after_lparen = op == "(";
                return Ok(Tok::Op(op));
            }
        }
        // a word
        let start = self.i;
        let s = self.s;
        let mut i = self.i;
        while i < s.len() {
            let c = s[i];
            match c {
                b'\\' => i = (i + 2).min(s.len()),
                b'\'' => i = skip_single(s, i)?,
                b'"' => i = skip_double(s, i)?,
                b'`' => i = skip_backtick(s, i)?,
                b'$' if s.get(i + 1) == Some(&b'(') => i = skip_balanced(s, i + 1, b'(', b')')?,
                b'$' if s.get(i + 1) == Some(&b'{') => i = skip_balanced(s, i + 1, b'{', b'}')?,
                b'(' if !self.cmd_pos || i > start => {
                    // fish (cmd) inside/as an argument
                    if i > start
                        && s[start..i]
                            .iter()
                            .all(|&c| is_name_start(c) || c.is_ascii_digit())
                        && s.get(i + 1) == Some(&b')')
                    {
                        break; // foo() function definition
                    }
                    i = skip_balanced(s, i, b'(', b')')?;
                }
                _ if is_meta(c) => break,
                _ => i += 1,
            }
        }
        if i == start {
            // lone '(' at command position that didn't match an op: shouldn't happen
            self.i += 1;
            return Err(ParseError::Syntax(format!("unexpected '{}'", c as char)));
        }
        self.i = i;
        let w = String::from_utf8_lossy(&s[start..i]).into_owned();
        self.cmd_pos = LEADS.contains(&w.as_str()) || (self.cmd_pos && is_assignment(&w));
        Ok(Tok::Word(w))
    }
}

pub fn is_assignment(w: &str) -> bool {
    let b = w.as_bytes();
    if b.is_empty() || !is_name_start(b[0]) {
        return false;
    }
    let mut i = 1;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
        i += 1;
    }
    i < b.len() && (b[i] == b'=' || (b[i] == b'+' && b.get(i + 1) == Some(&b'=')))
}

pub fn is_name(w: &str) -> bool {
    let b = w.as_bytes();
    !b.is_empty()
        && is_name_start(b[0])
        && b.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'_')
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    heredocs: Vec<String>,
    heredoc_quoted: Vec<bool>,
}

const CLOSERS: [&str; 10] = [
    "then", "fi", "do", "done", "else", "elif", "esac", "end", "}", ";;",
];

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos]
    }
    fn bump(&mut self) -> Tok {
        let t = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }
    fn peek_word(&self) -> Option<&str> {
        match self.peek() {
            Tok::Word(w) => Some(w),
            _ => None,
        }
    }
    fn at_word(&self, w: &str) -> bool {
        self.peek_word() == Some(w)
    }
    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Tok::Newline) || *self.peek() == Tok::Op(";") && false {
            self.bump();
        }
    }
    fn skip_separators(&mut self) {
        while matches!(self.peek(), Tok::Newline | Tok::Op(";")) {
            self.bump();
        }
    }
    fn expect_word(&mut self, w: &str) -> Result<(), ParseError> {
        self.skip_separators();
        match self.peek() {
            Tok::Word(x) if x == w => {
                self.bump();
                Ok(())
            }
            Tok::Eof => Err(ParseError::Incomplete),
            t => Err(ParseError::Syntax(format!(
                "expected '{w}', found {}",
                describe(t)
            ))),
        }
    }
    fn expect_one_of(&mut self, ws: &[&str]) -> Result<String, ParseError> {
        self.skip_separators();
        match self.peek().clone() {
            Tok::Word(x) if ws.contains(&x.as_str()) => {
                self.bump();
                Ok(x)
            }
            Tok::Eof => Err(ParseError::Incomplete),
            t => Err(ParseError::Syntax(format!(
                "expected '{}', found {}",
                ws.join("' or '"),
                describe(&t)
            ))),
        }
    }

    fn at_list_end(&self) -> bool {
        match self.peek() {
            Tok::Eof => true,
            Tok::Op(")") | Tok::Op(";;") => true,
            Tok::Word(w) => CLOSERS.contains(&w.as_str()),
            _ => false,
        }
    }

    /// list := and_or ((; | & | newline) and_or)*
    fn list(&mut self) -> Result<List, ParseError> {
        let mut items = Vec::new();
        self.skip_separators();
        while !self.at_list_end() {
            let mut ao = self.and_or()?;
            match self.peek() {
                Tok::Op("&") => {
                    self.bump();
                    ao.background = true;
                }
                Tok::Op(";") | Tok::Newline => {
                    self.bump();
                }
                _ => {}
            }
            items.push(ao);
            self.skip_separators();
        }
        Ok(items)
    }

    fn and_or(&mut self) -> Result<AndOr, ParseError> {
        let first = self.pipeline()?;
        let mut rest = Vec::new();
        loop {
            let c = match self.peek() {
                Tok::Op("&&") => Connector::And,
                Tok::Op("||") => Connector::Or,
                _ => break,
            };
            self.bump();
            self.skip_newlines();
            if *self.peek() == Tok::Eof {
                return Err(ParseError::Incomplete);
            }
            rest.push((c, self.pipeline()?));
        }
        Ok(AndOr {
            first,
            rest,
            background: false,
        })
    }

    fn pipeline(&mut self) -> Result<Pipeline, ParseError> {
        let mut negate = false;
        while self.at_word("!") {
            self.bump();
            negate = !negate;
        }
        let mut cmds = vec![self.command()?];
        loop {
            match self.peek() {
                Tok::Op("|") => {
                    self.bump();
                }
                Tok::Op("|&") => {
                    self.bump();
                    // cmd |& next  ==  cmd 2>&1 | next
                    if let Some(last) = cmds.last_mut() {
                        push_redir(
                            last,
                            Redir {
                                fd: 2,
                                kind: RedirKind::DupOut,
                                target: "1".into(),
                            },
                        );
                    }
                }
                _ => break,
            }
            self.skip_newlines();
            if *self.peek() == Tok::Eof {
                return Err(ParseError::Incomplete);
            }
            cmds.push(self.command()?);
        }
        Ok(Pipeline { negate, cmds })
    }

    fn redirs(&mut self) -> Result<Vec<Redir>, ParseError> {
        let mut v = Vec::new();
        while let Tok::Redir { .. } = self.peek() {
            v.push(self.redir()?);
        }
        Ok(v)
    }

    fn redir(&mut self) -> Result<Redir, ParseError> {
        let Tok::Redir { fd, op, heredoc } = self.bump() else {
            unreachable!()
        };
        let target = match self.bump() {
            Tok::Word(w) => w,
            Tok::Eof | Tok::Newline => {
                return Err(ParseError::Syntax(
                    "syntax error near unexpected token `newline'".to_string(),
                ));
            }
            t => {
                return Err(ParseError::Syntax(format!(
                    "syntax error near unexpected token `{}'",
                    describe(&t)
                )));
            }
        };
        let kind = match op {
            "<" => RedirKind::In,
            ">" => RedirKind::Out,
            ">>" => RedirKind::Append,
            "<>" => RedirKind::InOut,
            ">&" => RedirKind::DupOut,
            "<&" => RedirKind::DupIn,
            "&>" => RedirKind::Both,
            "&>>" => RedirKind::BothAppend,
            "<<<" => RedirKind::HereString,
            _ => RedirKind::HereDoc {
                expand: !target.contains(['\'', '"', '\\']),
            },
        };
        let default_fd = match kind {
            RedirKind::In
            | RedirKind::InOut
            | RedirKind::DupIn
            | RedirKind::HereDoc { .. }
            | RedirKind::HereString => 0,
            _ => 1,
        };
        let target = match (heredoc, &kind) {
            (Some(idx), RedirKind::HereDoc { .. }) => {
                self.heredocs.get(idx).cloned().unwrap_or_default()
            }
            _ => target,
        };
        let _ = &self.heredoc_quoted;
        Ok(Redir {
            fd: fd.unwrap_or(default_fd),
            kind,
            target,
        })
    }

    /// Parse a block body that ends in one of `enders` (consumed). Returns the ender.
    fn body_until(&mut self, enders: &[&str]) -> Result<(List, String), ParseError> {
        let l = self.list()?;
        let e = self.expect_one_of(enders)?;
        Ok((l, e))
    }

    fn command(&mut self) -> Result<Command, ParseError> {
        let w = match self.peek() {
            Tok::Arith(e) => {
                let e = e.clone();
                self.bump();
                let r = self.redirs()?;
                return Ok(Command::Arith(e, r));
            }
            Tok::Op("(") => {
                self.bump();
                let l = self.list()?;
                match self.bump() {
                    Tok::Op(")") => {}
                    Tok::Eof => return Err(ParseError::Incomplete),
                    t => {
                        return Err(ParseError::Syntax(format!(
                            "expected ')', found {}",
                            describe(&t)
                        )));
                    }
                }
                let r = self.redirs()?;
                return Ok(Command::Subshell(l, r));
            }
            Tok::Word(w) => w.clone(),
            Tok::Eof => return Err(ParseError::Incomplete),
            Tok::Redir { .. } => String::new(),
            t => {
                return Err(ParseError::Syntax(format!(
                    "syntax error near unexpected token `{}'",
                    describe(t)
                )));
            }
        };
        match w.as_str() {
            "[[" => {
                self.bump();
                let mut words = vec!["[[".to_string()];
                loop {
                    match self.bump() {
                        Tok::Word(x) if x == "]]" => break,
                        Tok::Word(x) if x.starts_with('(') && x.ends_with(')') && x.len() >= 2 => {
                            // the lexer grabbed "(a -eq b)" as one word: split it back up
                            words.push("(".into());
                            let mut lx = Lexer::new(&x[1..x.len() - 1]);
                            lx.cmd_pos = false;
                            while let Ok(t) = lx.next() {
                                match t {
                                    Tok::Word(w) => words.push(w),
                                    Tok::Op(o) => words.push(o.to_string()),
                                    Tok::Redir { op, .. } => words.push(op.to_string()),
                                    _ => break,
                                }
                            }
                            words.push(")".into());
                        }
                        Tok::Word(x) => words.push(x),
                        Tok::Op(o) => words.push(o.to_string()),
                        Tok::Redir { op, .. } => words.push(op.to_string()),
                        Tok::Newline => continue,
                        Tok::Eof => return Err(ParseError::Incomplete),
                        Tok::Arith(a) => words.push(format!("(({a}))")),
                    }
                }
                let redirs = self.redirs()?;
                return Ok(Command::Simple {
                    assigns: Vec::new(),
                    words,
                    redirs,
                });
            }
            "if" => {
                self.bump();
                return self.if_rest();
            }
            "while" | "until" => {
                self.bump();
                let until = w == "until";
                let (cond, body) = self.cond_and_body("do", &["done", "end"])?;
                let r = self.redirs()?;
                return Ok(Command::While {
                    until,
                    cond,
                    body,
                    redirs: r,
                });
            }
            "for" => {
                self.bump();
                return self.for_rest();
            }
            "case" => {
                self.bump();
                return self.case_rest();
            }
            "{" => {
                self.bump();
                let (l, _) = self.body_until(&["}"])?;
                let r = self.redirs()?;
                return Ok(Command::Group(l, r));
            }
            "begin" => {
                self.bump();
                let (l, _) = self.body_until(&["end"])?;
                let r = self.redirs()?;
                return Ok(Command::Group(l, r));
            }
            "function" => {
                self.bump();
                let name = match self.bump() {
                    Tok::Word(n) => n,
                    Tok::Eof => return Err(ParseError::Incomplete),
                    t => {
                        return Err(ParseError::Syntax(format!(
                            "bad function name {}",
                            describe(&t)
                        )));
                    }
                };
                if *self.peek() == Tok::Op("(") {
                    self.bump();
                    if self.bump() != Tok::Op(")") {
                        return Err(ParseError::Syntax("expected ')'".into()));
                    }
                }
                self.skip_newlines();
                // bash: function f { ...; }   fish: function f; ...; end
                let body = if self.at_word("{")
                    || self.at_word("(")
                    || matches!(self.peek(), Tok::Op("("))
                    || self.at_compound_start()
                {
                    self.command()?
                } else {
                    let (l, _) = self.body_until(&["end"])?;
                    Command::Group(l, Vec::new())
                };
                return Ok(Command::Function {
                    name,
                    body: Box::new(body),
                });
            }
            _ => {}
        }
        // name() compound
        if (is_name(&w) || w.contains(['-', '.', ':']) && !w.contains(['$', '\'', '"']))
            && self.toks.get(self.pos + 1) == Some(&Tok::Op("("))
            && self.toks.get(self.pos + 2) == Some(&Tok::Op(")"))
        {
            self.pos += 3;
            self.skip_newlines();
            let body = self.command()?;
            return Ok(Command::Function {
                name: w,
                body: Box::new(body),
            });
        }
        self.simple()
    }

    fn at_compound_start(&self) -> bool {
        matches!(
            self.peek_word(),
            Some("if" | "while" | "until" | "for" | "case" | "{" | "begin")
        )
    }

    /// condition + body for if/while: bash `cond; then|do body; fi|done`,
    /// or fish `cond; body; end` (condition is the first command).
    fn cond_and_body(
        &mut self,
        keyword: &str,
        enders: &[&str],
    ) -> Result<(List, List), ParseError> {
        let mut cond = Vec::new();
        self.skip_separators();
        loop {
            if self.at_word(keyword) {
                self.bump();
                let (body, _) = self.body_until(enders)?;
                return Ok((cond, body));
            }
            if *self.peek() == Tok::Eof {
                return Err(ParseError::Incomplete);
            }
            if self.at_list_end() {
                // fish style: no then/do; first command was the condition
                if cond.is_empty() {
                    return Err(ParseError::Syntax(format!("expected '{keyword}'")));
                }
                let rest: List = cond.split_off(1);
                let e = self.expect_one_of(enders)?;
                let _ = e;
                return Ok((cond, rest));
            }
            let mut ao = self.and_or()?;
            if *self.peek() == Tok::Op("&") {
                self.bump();
                ao.background = true;
            }
            cond.push(ao);
            self.skip_separators();
        }
    }

    fn if_rest(&mut self) -> Result<Command, ParseError> {
        let mut branches = Vec::new();
        let mut otherwise = None;
        // condition
        let mut cond = Vec::new();
        self.skip_separators();
        let bash_style;
        loop {
            if self.at_word("then") {
                self.bump();
                bash_style = true;
                break;
            }
            if *self.peek() == Tok::Eof {
                return Err(ParseError::Incomplete);
            }
            if self.at_list_end() {
                bash_style = false;
                break;
            }
            cond.push(self.and_or()?);
            self.skip_separators();
        }
        if bash_style {
            let mut cur_cond = cond;
            loop {
                let body = self.list()?;
                let kw = self.expect_one_of(&["elif", "else", "fi", "end"])?;
                branches.push((std::mem::take(&mut cur_cond), body));
                match kw.as_str() {
                    "elif" => {
                        let (c, _) = self.body_until(&["then"])?;
                        cur_cond = c;
                    }
                    "else" => {
                        let (b, _) = self.body_until(&["fi", "end"])?;
                        otherwise = Some(b);
                        break;
                    }
                    _ => break,
                }
            }
        } else {
            // fish: if COND; BODY; [else if COND; BODY;] [else; BODY;] end
            if cond.is_empty() {
                return Err(ParseError::Syntax("expected 'then'".into()));
            }
            let mut body = cond.split_off(1);
            let mut cur_cond = cond;
            loop {
                let more = self.list()?;
                body.extend(more);
                let kw = self.expect_one_of(&["else", "end", "fi"])?;
                branches.push((std::mem::take(&mut cur_cond), std::mem::take(&mut body)));
                if kw != "else" {
                    break;
                }
                if self.at_word("if") {
                    self.bump();
                    self.skip_separators();
                    let c = self.and_or()?;
                    cur_cond = vec![c];
                    continue;
                }
                let (b, _) = self.body_until(&["end", "fi"])?;
                otherwise = Some(b);
                break;
            }
        }
        let redirs = self.redirs()?;
        Ok(Command::If {
            branches,
            otherwise,
            redirs,
        })
    }

    fn for_rest(&mut self) -> Result<Command, ParseError> {
        let var = match self.bump() {
            Tok::Word(v) => v,
            Tok::Eof => return Err(ParseError::Incomplete),
            t => {
                return Err(ParseError::Syntax(format!(
                    "bad for variable {}",
                    describe(&t)
                )));
            }
        };
        self.skip_newlines();
        let mut items = None;
        if self.at_word("in") {
            self.bump();
            let mut v = Vec::new();
            while let Tok::Word(w) = self.peek() {
                v.push(w.clone());
                self.bump();
            }
            items = Some(v);
        }
        self.skip_separators();
        let body = if self.at_word("do") {
            self.bump();
            self.body_until(&["done", "end"])?.0
        } else if self.at_word("{") {
            self.bump();
            self.body_until(&["}"])?.0
        } else {
            self.body_until(&["end", "done"])?.0
        };
        let redirs = self.redirs()?;
        Ok(Command::For {
            var,
            items,
            body,
            redirs,
        })
    }

    fn case_rest(&mut self) -> Result<Command, ParseError> {
        let word = match self.bump() {
            Tok::Word(w) => w,
            Tok::Eof => return Err(ParseError::Incomplete),
            t => {
                return Err(ParseError::Syntax(format!(
                    "bad case word {}",
                    describe(&t)
                )));
            }
        };
        self.expect_word("in")?;
        let mut arms = Vec::new();
        loop {
            self.skip_separators();
            if self.at_word("esac") || self.at_word("end") {
                self.bump();
                break;
            }
            if *self.peek() == Tok::Eof {
                return Err(ParseError::Incomplete);
            }
            if *self.peek() == Tok::Op("(") {
                self.bump();
            }
            let mut pats = Vec::new();
            loop {
                match self.bump() {
                    Tok::Word(p) => pats.push(p),
                    Tok::Eof => return Err(ParseError::Incomplete),
                    t => {
                        return Err(ParseError::Syntax(format!(
                            "bad case pattern {}",
                            describe(&t)
                        )));
                    }
                }
                match self.bump() {
                    Tok::Op("|") => continue,
                    Tok::Op(")") => break,
                    Tok::Eof => return Err(ParseError::Incomplete),
                    t => {
                        return Err(ParseError::Syntax(format!(
                            "expected ')' in case, found {}",
                            describe(&t)
                        )));
                    }
                }
            }
            let body = self.list()?;
            arms.push((pats, body));
            match self.peek() {
                Tok::Op(";;") => {
                    self.bump();
                }
                Tok::Word(w) if w == "esac" || w == "end" => {}
                Tok::Eof => return Err(ParseError::Incomplete),
                t => {
                    return Err(ParseError::Syntax(format!(
                        "expected ';;', found {}",
                        describe(t)
                    )));
                }
            }
        }
        let redirs = self.redirs()?;
        Ok(Command::Case { word, arms, redirs })
    }

    fn simple(&mut self) -> Result<Command, ParseError> {
        let mut assigns = Vec::new();
        let mut words = Vec::new();
        let mut redirs = Vec::new();
        loop {
            match self.peek().clone() {
                Tok::Word(w) => {
                    if words.is_empty() && is_assignment(&w) {
                        let (k, v) = w.split_once('=').unwrap();
                        assigns.push((k.to_string(), v.to_string()));
                    } else {
                        words.push(w);
                    }
                    self.bump();
                }
                Tok::Redir { .. } => redirs.push(self.redir()?),
                _ => break,
            }
        }
        if assigns.is_empty() && words.is_empty() && redirs.is_empty() {
            return Err(ParseError::Syntax(format!(
                "syntax error near unexpected token `{}'",
                describe(self.peek())
            )));
        }
        Ok(Command::Simple {
            assigns,
            words,
            redirs,
        })
    }
}

fn push_redir(c: &mut Command, r: Redir) {
    match c {
        Command::Simple { redirs, .. }
        | Command::If { redirs, .. }
        | Command::While { redirs, .. }
        | Command::For { redirs, .. }
        | Command::Case { redirs, .. }
        | Command::Group(_, redirs)
        | Command::Subshell(_, redirs)
        | Command::Arith(_, redirs) => redirs.push(r),
        Command::Function { .. } => {}
    }
}

fn describe(t: &Tok) -> String {
    match t {
        Tok::Word(w) => w.clone(),
        Tok::Op(o) => o.to_string(),
        Tok::Redir { op, .. } => op.to_string(),
        Tok::Arith(_) => "((".into(),
        Tok::Newline => "newline".into(),
        Tok::Eof => "end of file".into(),
    }
}

pub fn parse(src: &str) -> Result<List, ParseError> {
    let mut lx = Lexer::new(src);
    let mut toks = Vec::new();
    loop {
        let t = lx.next()?;
        let eof = t == Tok::Eof;
        toks.push(t);
        if eof {
            break;
        }
    }
    let heredocs = lx.heredocs;
    let mut p = Parser {
        toks,
        pos: 0,
        heredocs,
        heredoc_quoted: Vec::new(),
    };
    let list = p.list()?;
    match p.peek() {
        Tok::Eof => Ok(list),
        t => Err(ParseError::Syntax(format!(
            "syntax error near unexpected token `{}'",
            describe(t)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) {
        if let Err(e) = parse(s) {
            panic!("{s:?} failed: {e:?}");
        }
    }

    #[test]
    fn bash_forms() {
        ok("echo hi | tr a-z A-Z && echo ok || echo no; echo done &");
        ok("if true; then echo a; elif false; then echo b; else echo c; fi");
        ok("for i in 1 2 3; do echo $i; done");
        ok("while false; do :; done");
        ok("case $x in a|b) echo ab;; *) echo other;; esac");
        ok("f() { echo $1; }; f hi");
        ok("g() (echo subshell body)");
        ok("function g { echo g; }");
        ok("(cd /tmp && ls) > out 2>&1");
        ok("cat <<EOF\nhello $USER\nEOF\necho after");
        ok("echo $(echo $(echo nested)) \"q $(x) q\" `y`");
        ok("((x = 1 + 2))");
        ok("a=1 b=2 env");
        ok("echo 'unterminated' \"also\" \\\n continued");
    }

    #[test]
    fn fish_forms() {
        ok("if test -n x; echo yes; else; echo no; end");
        ok("if test 1; echo a; else if test 2; echo b; end");
        ok("for x in a b; echo $x; end");
        ok("while false; echo x; end");
        ok("function hi; echo hi; end");
        ok("begin; echo a; echo b; end");
        ok("echo (pwd) (echo a b)");
    }

    #[test]
    fn incomplete() {
        for s in [
            "if true; then",
            "echo 'abc",
            "echo hi |",
            "cat <<EOF\nabc",
            "for x in a b; do",
        ] {
            assert!(
                matches!(parse(s), Err(ParseError::Incomplete)),
                "{s:?} should be incomplete"
            );
        }
    }
}
