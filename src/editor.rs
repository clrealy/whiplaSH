//! The fish-style interactive line editor: syntax highlighting,
//! history autosuggestions, tab completion and multi-line input.

use rustyline::completion::{Completer, FilenameCompleter, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::{Hinter, HistoryHinter};
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{Context, Helper};
use std::borrow::Cow;
use std::collections::BTreeSet;

pub struct WhipHelper {
    files: FilenameCompleter,
    hinter: HistoryHinter,
    /// every command name we know: builtins, keywords, functions, aliases, $PATH
    pub commands: BTreeSet<String>,
    pub colored_prompt: String,
}

const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const BLUE: &str = "\x1b[34m";
const GRAY: &str = "\x1b[90m";

impl WhipHelper {
    pub fn new() -> Self {
        WhipHelper {
            files: FilenameCompleter::new(),
            hinter: HistoryHinter::new(),
            commands: BTreeSet::new(),
            colored_prompt: String::new(),
        }
    }

    pub fn refresh_commands(&mut self, path: &str, extra: impl Iterator<Item = String>) {
        let mut set: BTreeSet<String> = BTreeSet::new();
        for dir in path.split(':') {
            if let Ok(rd) = std::fs::read_dir(if dir.is_empty() { "." } else { dir }) {
                for e in rd.flatten() {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(m) = e.metadata()
                        && m.is_file()
                        && m.permissions().mode() & 0o111 != 0
                    {
                        set.insert(e.file_name().to_string_lossy().into_owned());
                    }
                }
            }
        }
        set.extend(crate::builtins::BUILTINS.iter().map(|s| s.to_string()));
        set.extend(crate::builtins::KEYWORDS.iter().map(|s| s.to_string()));
        set.extend(extra);
        self.commands = set;
    }

    fn command_exists(&self, w: &str) -> bool {
        if w.contains('/') {
            return std::path::Path::new(w).exists();
        }
        self.commands.contains(w) || crate::parser::is_assignment(w)
    }
}

/// Byte index where the word under the cursor starts, and whether it's in command position.
fn word_start(line: &str, pos: usize) -> (usize, bool) {
    let before = &line[..pos];
    let start = before
        .rfind(|c: char| c.is_whitespace() || ";|&()<>".contains(c))
        .map(|i| i + 1)
        .unwrap_or(0);
    let prev = before[..start].trim_end();
    let cmd_pos = prev.is_empty()
        || prev.ends_with([';', '|', '&', '(', '{'])
        || [
            "then", "do", "else", "elif", "if", "while", "until", "and", "or", "not", "!", "sudo",
            "doas", "time", "exec", "command", "builtin",
        ]
        .iter()
        .any(|k| {
            prev == *k || prev.ends_with(&format!(" {k}")) || prev.ends_with(&format!(";{k}"))
        });
    (start, cmd_pos)
}

impl Completer for WhipHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let (start, cmd_pos) = word_start(line, pos);
        let word = &line[start..pos];
        if cmd_pos && !word.contains('/') && !word.is_empty() {
            let cands: Vec<Pair> = self
                .commands
                .range(word.to_string()..)
                .take_while(|c| c.starts_with(word))
                .map(|c| Pair {
                    display: c.clone(),
                    replacement: format!("{c} "),
                })
                .collect();
            if !cands.is_empty() {
                return Ok((start, cands));
            }
        }
        // $VAR completion
        if let Some(var) = word.strip_prefix('$') {
            let cands: Vec<Pair> = std::env::vars()
                .map(|(k, _)| k)
                .filter(|k| k.starts_with(var))
                .map(|k| Pair {
                    display: k.clone(),
                    replacement: format!("${k}"),
                })
                .collect();
            return Ok((start, cands));
        }
        self.files.complete(line, pos, ctx)
    }
}

impl Hinter for WhipHelper {
    type Hint = String;
    fn hint(&self, line: &str, pos: usize, ctx: &Context<'_>) -> Option<String> {
        if line.is_empty() || pos < line.len() {
            return None;
        }
        self.hinter.hint(line, pos, ctx)
    }
}

impl Highlighter for WhipHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        let mut out = String::with_capacity(line.len() * 2);
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        let mut cmd_pos = true;
        while i < chars.len() {
            let c = chars[i];
            match c {
                '#' if i == 0 || chars[i - 1].is_whitespace() => {
                    out.push_str(GRAY);
                    out.extend(&chars[i..]);
                    out.push_str(RESET);
                    break;
                }
                ' ' | '\t' | '\n' => {
                    out.push(c);
                    i += 1;
                }
                ';' | '|' | '&' | '(' | ')' => {
                    out.push_str(CYAN);
                    out.push(c);
                    out.push_str(RESET);
                    cmd_pos = c != ')';
                    i += 1;
                }
                '<' | '>' => {
                    out.push_str(CYAN);
                    out.push(c);
                    out.push_str(RESET);
                    i += 1;
                }
                _ => {
                    // a whole word
                    let st = i;
                    let mut quote: Option<char> = None;
                    while i < chars.len() {
                        let ch = chars[i];
                        if let Some(q) = quote {
                            if ch == q {
                                quote = None;
                            } else if ch == '\\' && q == '"' {
                                i += 1;
                            }
                        } else if ch == '\'' || ch == '"' {
                            quote = Some(ch);
                        } else if ch == '\\' {
                            i += 1;
                        } else if ch.is_whitespace() || ";|&()<>".contains(ch) {
                            break;
                        }
                        i += 1;
                    }
                    let i_end = i.min(chars.len());
                    let word: String = chars[st..i_end].iter().collect();
                    if cmd_pos {
                        let color = if crate::builtins::KEYWORDS.contains(&word.as_str()) {
                            BLUE
                        } else if self.command_exists(&word) {
                            GREEN
                        } else {
                            RED
                        };
                        out.push_str(color);
                        out.push_str(&word);
                        out.push_str(RESET);
                        cmd_pos = crate::builtins::KEYWORDS.contains(&word.as_str())
                            || crate::parser::is_assignment(&word)
                            || ["and", "or", "not", "sudo", "doas", "time"]
                                .contains(&word.as_str());
                    } else {
                        out.push_str(&color_arg(&word));
                    }
                }
            }
        }
        Cow::Owned(out)
    }

    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        default: bool,
    ) -> Cow<'b, str> {
        if default && !self.colored_prompt.is_empty() {
            Cow::Borrowed(&self.colored_prompt)
        } else {
            Cow::Borrowed(prompt)
        }
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(format!("{GRAY}{hint}{RESET}"))
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _forced: bool) -> bool {
        true
    }
}

/// Arguments: strings yellow, $vars cyan, options plain.
fn color_arg(w: &str) -> String {
    let mut out = String::new();
    let mut quote: Option<char> = None;
    for ch in w.chars() {
        match quote {
            Some(q) => {
                out.push(ch);
                if ch == q {
                    out.push_str(RESET);
                    quote = None;
                }
            }
            None if ch == '\'' || ch == '"' => {
                out.push_str(YELLOW);
                out.push(ch);
                quote = Some(ch);
            }
            None if ch == '$' => {
                out.push_str(CYAN);
                out.push(ch);
                out.push_str(RESET);
            }
            None => out.push(ch),
        }
    }
    if quote.is_some() {
        out.push_str(RESET);
    }
    out
}

impl Validator for WhipHelper {
    fn validate(&self, ctx: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
        match crate::parser::parse(ctx.input()) {
            Err(crate::parser::ParseError::Incomplete) => Ok(ValidationResult::Incomplete),
            _ => Ok(ValidationResult::Valid(None)),
        }
    }
}

impl Helper for WhipHelper {}

/// fish-style short path: ~/Projects/rng/src -> ~/P/r/src
pub fn short_pwd(pwd: &str, home: Option<&str>) -> String {
    let mut p = pwd.to_string();
    if let Some(h) = home
        && !h.is_empty()
        && (p == h || p.starts_with(&format!("{h}/")))
    {
        p = format!("~{}", &p[h.len()..]);
    }
    let parts: Vec<&str> = p.split('/').collect();
    let n = parts.len();
    parts
        .iter()
        .enumerate()
        .map(|(i, s)| {
            if i + 1 == n || s.is_empty() || *s == "~" {
                s.to_string()
            } else {
                let keep = if s.starts_with('.') { 2 } else { 1 };
                s.chars().take(keep).collect()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::short_pwd;

    #[test]
    fn short_paths() {
        assert_eq!(
            short_pwd("/home/c/Projects/rng", Some("/home/c")),
            "~/P/rng"
        );
        assert_eq!(short_pwd("/home/c", Some("/home/c")), "~");
        assert_eq!(short_pwd("/usr/local/bin", Some("/home/c")), "/u/l/bin");
        assert_eq!(short_pwd("/home/c/.config/x", Some("/home/c")), "~/.c/x");
        assert_eq!(short_pwd("/", None), "/");
    }
}
