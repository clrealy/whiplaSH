//! Word expansion, in bash's order: tilde, parameters, command
//! substitution, arithmetic, then field splitting, globbing and quote removal.

use crate::pattern::{self, P};
use crate::shell::Shell;

/// One character of a word on its way through expansion.
#[derive(Debug, Clone, Copy)]
enum It {
    /// (char, quoted, came-from-expansion)
    C(char, bool, bool),
    /// forced field boundary (from "$@")
    Break,
    /// "$@" with no arguments: this field may vanish
    EmptyAt,
}

#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    Word,
    Assign,
    Heredoc,
    /// inside $(( )): parentheses are just grouping
    Arith,
}

pub struct ExpandError(pub String);

impl Shell {
    /// Expand a word into fields (splitting + globbing).
    pub fn expand_fields(&mut self, raw: &str) -> Result<Vec<String>, ExpandError> {
        let mut out = Vec::new();
        for w in brace_expand(raw) {
            let items = self.expand_items(&w, Ctx::Word)?;
            out.extend(self.split_and_glob(items));
        }
        Ok(out)
    }

    /// Expand without splitting or globbing (assignments, redirect targets, case word).
    pub fn expand_string(&mut self, raw: &str) -> Result<String, ExpandError> {
        let items = self.expand_items(raw, Ctx::Word)?;
        Ok(join_items(&items))
    }

    pub fn expand_assign(&mut self, raw: &str) -> Result<String, ExpandError> {
        let items = self.expand_items(raw, Ctx::Assign)?;
        Ok(join_items(&items))
    }

    pub fn expand_arith(&mut self, raw: &str) -> Result<String, ExpandError> {
        let items = self.expand_items(raw, Ctx::Arith)?;
        Ok(join_items(&items))
    }

    pub fn expand_heredoc(&mut self, body: &str) -> Result<String, ExpandError> {
        let items = self.expand_items(body, Ctx::Heredoc)?;
        Ok(join_items(&items))
    }

    /// Expand into a pattern: quoted characters match literally.
    pub fn expand_pattern(&mut self, raw: &str) -> Result<Vec<P>, ExpandError> {
        let items = self.expand_items(raw, Ctx::Word)?;
        let mut chars = Vec::new();
        let mut quoted = Vec::new();
        for it in items {
            if let It::C(c, q, _) = it {
                chars.push(c);
                quoted.push(q);
            }
        }
        Ok(pattern::compile(&chars, &quoted))
    }

    fn expand_items(&mut self, raw: &str, ctx: Ctx) -> Result<Vec<It>, ExpandError> {
        let s: Vec<char> = raw.chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        // tilde at the start (and after ':' / '=' in assignments)
        let mut tilde_ok = ctx == Ctx::Word || ctx == Ctx::Assign;
        while i < s.len() {
            let c = s[i];
            if tilde_ok && c == '~' {
                let end = s[i..]
                    .iter()
                    .position(|&x| x == '/' || (ctx == Ctx::Assign && x == ':'))
                    .map(|p| i + p)
                    .unwrap_or(s.len());
                let user: String = s[i + 1..end].iter().collect();
                if user
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
                    && let Some(home) = tilde(self, &user)
                {
                    push_str(&mut out, &home, true, false);
                    i = end;
                    tilde_ok = false;
                    continue;
                }
            }
            tilde_ok = ctx == Ctx::Assign && c == ':';
            match c {
                '\\' if ctx == Ctx::Heredoc => match s.get(i + 1) {
                    Some(&n @ ('$' | '`' | '\\')) => {
                        out.push(It::C(n, true, false));
                        i += 2;
                    }
                    Some('\n') => i += 2,
                    _ => {
                        out.push(It::C('\\', true, false));
                        i += 1;
                    }
                },
                '\\' => {
                    if let Some(&n) = s.get(i + 1)
                        && n != '\n'
                    {
                        out.push(It::C(n, true, false));
                    }
                    i += 2;
                }
                '\'' if ctx != Ctx::Heredoc => {
                    let end = s[i + 1..]
                        .iter()
                        .position(|&x| x == '\'')
                        .map(|p| i + 1 + p)
                        .unwrap_or(s.len());
                    for &ch in &s[i + 1..end] {
                        out.push(It::C(ch, true, false));
                    }
                    if end == i + 1 {
                        out.push(It::C('\0', true, false)); // keeps '' as an empty field
                    }
                    i = end + 1;
                }
                '"' if ctx != Ctx::Heredoc => {
                    let start_len = out.len();
                    i = self.double_quoted(&s, i + 1, &mut out)?;
                    if out.len() == start_len {
                        out.push(It::C('\0', true, false));
                    }
                }
                '$' => i = self.dollar(&s, i, &mut out, ctx == Ctx::Heredoc)?,
                '`' => {
                    let (cmd, next) = backtick(&s, i);
                    let text = self.command_subst(&cmd);
                    push_str(&mut out, &text, ctx == Ctx::Heredoc, true);
                    i = next;
                }
                '(' if ctx == Ctx::Word => {
                    // fish-style (cmd) substitution
                    let end = match_paren(&s, i);
                    let cmd: String = s[i + 1..end.saturating_sub(1).max(i + 1)].iter().collect();
                    let text = self.command_subst(&cmd);
                    push_str(&mut out, &text, false, true);
                    i = end;
                }
                _ => {
                    out.push(It::C(c, ctx == Ctx::Heredoc, false));
                    i += 1;
                }
            }
        }
        Ok(out)
    }

    /// Inside "...": only $, ` and \ are special. Returns index after the closing quote.
    fn double_quoted(
        &mut self,
        s: &[char],
        mut i: usize,
        out: &mut Vec<It>,
    ) -> Result<usize, ExpandError> {
        while i < s.len() {
            match s[i] {
                '"' => return Ok(i + 1),
                '\\' => {
                    match s.get(i + 1) {
                        Some(&n @ ('$' | '`' | '"' | '\\')) => out.push(It::C(n, true, false)),
                        Some('\n') => {}
                        Some(&n) => {
                            out.push(It::C('\\', true, false));
                            out.push(It::C(n, true, false));
                        }
                        None => out.push(It::C('\\', true, false)),
                    }
                    i += 2;
                }
                '$' => i = self.dollar(s, i, out, true)?,
                '`' => {
                    let (cmd, next) = backtick(s, i);
                    let text = self.command_subst(&cmd);
                    push_str(out, &text, true, true);
                    i = next;
                }
                c => {
                    out.push(It::C(c, true, false));
                    i += 1;
                }
            }
        }
        Ok(i)
    }

    /// Handle `$...` at s[i]. Returns the index after it.
    fn dollar(
        &mut self,
        s: &[char],
        i: usize,
        out: &mut Vec<It>,
        quoted: bool,
    ) -> Result<usize, ExpandError> {
        let next = s.get(i + 1).copied();
        match next {
            Some('(') if s.get(i + 2) == Some(&'(') => {
                // $(( arithmetic )) — find the matching "))"
                let end = match_paren(s, i + 1);
                let inner: String = s[i + 3..end.saturating_sub(2).max(i + 3)].iter().collect();
                let expanded = self.expand_arith(&inner)?;
                match crate::arith::eval_str(self, &expanded) {
                    Ok(v) => push_str(out, &v.to_string(), quoted, true),
                    Err(e) => return Err(ExpandError(format!("{}: {e}", inner.trim()))),
                }
                Ok(end)
            }
            Some('(') => {
                let end = match_paren(s, i + 1);
                let cmd: String = s[i + 2..end.saturating_sub(1).max(i + 2)].iter().collect();
                let text = self.command_subst(&cmd);
                push_str(out, &text, quoted, true);
                Ok(end)
            }
            Some('{') => {
                let end = match_brace(s, i + 1);
                let inner: String = s[i + 2..end.saturating_sub(1).max(i + 2)].iter().collect();
                self.brace_param(&inner, out, quoted)?;
                Ok(end)
            }
            Some('@') if quoted => {
                let args = self.positional.clone();
                if args.is_empty() {
                    out.push(It::EmptyAt);
                }
                for (n, a) in args.iter().enumerate() {
                    if n > 0 {
                        out.push(It::Break);
                    }
                    if a.is_empty() {
                        out.push(It::C('\0', true, false));
                    }
                    push_str(out, a, true, true);
                }
                Ok(i + 2)
            }
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                let mut j = i + 1;
                while j < s.len() && (s[j].is_ascii_alphanumeric() || s[j] == '_') {
                    j += 1;
                }
                let name: String = s[i + 1..j].iter().collect();
                let v = self.lookup(&name)?;
                push_str(out, &v.unwrap_or_default(), quoted, true);
                Ok(j)
            }
            Some(c)
                if c.is_ascii_digit() || matches!(c, '#' | '?' | '$' | '!' | '*' | '@' | '-') =>
            {
                let v = self.lookup(&c.to_string())?;
                push_str(out, &v.unwrap_or_default(), quoted, true);
                Ok(i + 2)
            }
            Some('\'') if !quoted => {
                // $'ansi-c' quoting
                let mut j = i + 2;
                let mut buf = String::new();
                while j < s.len() && s[j] != '\'' {
                    if s[j] == '\\' && j + 1 < s.len() {
                        j += 1;
                        let (ch, adv) = ansi_escape(&s[j..]);
                        if let Some(ch) = ch {
                            buf.push(ch);
                        }
                        j += adv;
                    } else {
                        buf.push(s[j]);
                        j += 1;
                    }
                }
                push_str(out, &buf, true, false);
                if buf.is_empty() {
                    out.push(It::C('\0', true, false));
                }
                Ok(j + 1)
            }
            _ => {
                out.push(It::C('$', quoted, false));
                Ok(i + 1)
            }
        }
    }

    /// Variable lookup including special parameters; honours `set -u`.
    fn lookup(&mut self, name: &str) -> Result<Option<String>, ExpandError> {
        let v = self.special_or_var(name);
        if v.is_none() && self.nounset && name != "@" && name != "*" {
            return Err(ExpandError(format!("{name}: unbound variable")));
        }
        Ok(v)
    }

    pub fn special_or_var(&self, name: &str) -> Option<String> {
        match name {
            "?" => Some(self.last_status.to_string()),
            "$" => Some(self.pid.to_string()),
            "!" => self.last_bg.map(|p| p.to_string()),
            "#" => Some(self.positional.len().to_string()),
            "0" => Some(self.arg0.clone()),
            "-" => Some(self.option_flags()),
            "*" | "@" => {
                let sep = self
                    .get_var("IFS")
                    .map(|s| s.chars().next().map(|c| c.to_string()).unwrap_or_default())
                    .unwrap_or(" ".into());
                Some(self.positional.join(&sep))
            }
            _ if name.chars().all(|c| c.is_ascii_digit()) => {
                let n: usize = name.parse().ok()?;
                self.positional.get(n.checked_sub(1)?).cloned()
            }
            "RANDOM" => {
                // SAFETY: libc::rand has no preconditions
                Some((unsafe { libc::rand() } % 32768).to_string())
            }
            "SECONDS" => Some(self.start.elapsed().as_secs().to_string()),
            // fish: $argv is the argument list
            "argv" if !self.vars.contains_key("argv") => Some(self.positional.join(" ")),
            "PWD" => self.get_var("PWD").or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.display().to_string())
            }),
            _ => self.get_var(name),
        }
    }

    /// ${...} forms.
    fn brace_param(
        &mut self,
        inner: &str,
        out: &mut Vec<It>,
        quoted: bool,
    ) -> Result<(), ExpandError> {
        let b: Vec<char> = inner.chars().collect();
        // ${!name[@]} keys, ${!name} indirection
        if b.first() == Some(&'!') && b.len() > 1 {
            let body: String = b[1..].iter().collect();
            if let Some(n) = body
                .strip_suffix("[@]")
                .or_else(|| body.strip_suffix("[*]"))
            {
                let keys = self.array_keys(n);
                push_list(out, &keys, quoted && body.ends_with("[@]"), quoted, self);
                return Ok(());
            }
            let target = self.special_or_var(&body).unwrap_or_default();
            if target.is_empty() {
                return Err(ExpandError(format!("{body}: invalid indirect expansion")));
            }
            let inner2 = target;
            return self.brace_param(&inner2, out, quoted);
        }
        // ${#...}
        if b.first() == Some(&'#') && b.len() > 1 {
            let name: String = b[1..].iter().collect();
            let v = if name == "@" || name == "*" {
                Some(self.positional.len().to_string())
            } else if let Some(n) = name
                .strip_suffix("[@]")
                .or_else(|| name.strip_suffix("[*]"))
            {
                Some(match self.vars.get(n) {
                    Some(crate::shell::Var { arr: Some(_), .. }) => {
                        self.array_values(n).len().to_string()
                    }
                    Some(_) => "1".into(),
                    None => "0".into(),
                })
            } else if let Some((n, rest)) = name.split_once('[') {
                let k = rest.strip_suffix(']').unwrap_or(rest);
                self.get_elem(n, k)
                    .map_err(ExpandError)?
                    .map(|v| v.chars().count().to_string())
            } else {
                self.lookup(&name)?.map(|v| v.chars().count().to_string())
            };
            push_str(out, &v.unwrap_or("0".into()), quoted, true);
            return Ok(());
        }
        // name part
        let mut j = 0;
        if b.first()
            .is_some_and(|c| c.is_ascii_alphabetic() || *c == '_')
        {
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == '_') {
                j += 1;
            }
        } else if b.first().is_some_and(|c| c.is_ascii_digit()) {
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
        } else if b
            .first()
            .is_some_and(|c| matches!(c, '?' | '$' | '!' | '#' | '*' | '@' | '-'))
        {
            j = 1;
        }
        if j == 0 {
            return Err(ExpandError(format!("${{{inner}}}: bad substitution")));
        }
        let name: String = b[..j].iter().collect();
        // optional [subscript]
        let mut sub: Option<String> = None;
        if b.get(j) == Some(&'[') {
            let mut depth = 0;
            let mut k = j;
            while k < b.len() {
                if b[k] == '[' {
                    depth += 1;
                } else if b[k] == ']' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                k += 1;
            }
            sub = Some(b[j + 1..k.min(b.len())].iter().collect());
            j = (k + 1).min(b.len());
        }
        let rest: String = b[j..].iter().collect();
        if name == "@" && quoted && rest.is_empty() {
            let s: Vec<char> = "$@".chars().collect();
            self.dollar(&s, 0, out, true)?;
            return Ok(());
        }
        // whole-array forms: ${a[@]} ${a[*]} and ops applied to each element
        if let Some(sb) = sub.as_deref().filter(|s| *s == "@" || *s == "*") {
            let mut vals = self.array_values(&name);
            if let Some(r) = rest
                .strip_prefix(':')
                .filter(|r| !r.starts_with(['-', '=', '+', '?']))
            {
                let (o, l) = match split_unescaped_opt(r, ':') {
                    Some((a, b)) => (a, Some(b)),
                    None => (r.to_string(), None),
                };
                let oe = self.expand_arith(&o)?;
                let off = crate::arith::eval_str(self, &oe).map_err(ExpandError)?;
                let n = vals.len() as i64;
                let st = if off < 0 {
                    (n + off).max(0)
                } else {
                    off.min(n)
                } as usize;
                let en = match l {
                    Some(l) => {
                        let le = self.expand_arith(&l)?;
                        let len = crate::arith::eval_str(self, &le).map_err(ExpandError)?;
                        (st + len.max(0) as usize).min(vals.len())
                    }
                    None => vals.len(),
                };
                vals = vals[st..en.max(st)].to_vec();
            } else if !rest.is_empty() {
                let mut mapped = Vec::new();
                for v in vals {
                    if let Some(r) = self.param_op(&name, None, Some(v), &rest)? {
                        mapped.push(r);
                    }
                }
                vals = mapped;
            }
            push_list(out, &vals, quoted && sb == "@", quoted, self);
            return Ok(());
        }
        let val = match &sub {
            Some(k) => self.get_elem(&name, k).map_err(ExpandError)?,
            None => self.special_or_var(&name),
        };
        let result = self.param_op(&name, sub.as_deref(), val, &rest)?;
        push_str(out, &result.unwrap_or_default(), quoted, true);
        Ok(())
    }

    /// The ${name<op>...} operators on one value.
    fn param_op(
        &mut self,
        name: &str,
        sub: Option<&str>,
        val: Option<String>,
        rest: &str,
    ) -> Result<Option<String>, ExpandError> {
        let rest = rest.to_string();
        let set = val.is_some();
        let nonempty = val.as_deref().is_some_and(|v| !v.is_empty());

        let result: Option<String> = if rest.is_empty() {
            if !set && self.nounset {
                return Err(ExpandError(format!("{name}: unbound variable")));
            }
            val
        } else if let Some((op, word)) =
            split_op(&rest, &[":-", ":=", ":+", ":?", "-", "=", "+", "?"])
        {
            let colon = op.starts_with(':');
            let use_val = if colon { nonempty } else { set };
            match op.trim_start_matches(':') {
                "-" => {
                    if use_val {
                        val
                    } else {
                        Some(self.expand_string(word)?)
                    }
                }
                "=" => {
                    if use_val {
                        val
                    } else {
                        let w = self.expand_string(word)?;
                        match sub {
                            Some(k) => self.set_elem(name, k, &w).map_err(ExpandError)?,
                            None => self.set_var(name, &w),
                        }
                        Some(w)
                    }
                }
                "+" => {
                    if use_val {
                        Some(self.expand_string(word)?)
                    } else {
                        Some(String::new())
                    }
                }
                _ => {
                    if use_val {
                        val
                    } else {
                        let msg = if word.is_empty() {
                            "parameter null or not set".to_string()
                        } else {
                            self.expand_string(word)?
                        };
                        return Err(ExpandError(format!("{name}: {msg}")));
                    }
                }
            }
        } else if let Some((op, word)) = split_op(&rest, &["##", "#", "%%", "%"]) {
            let v = val.unwrap_or_default();
            let p = self.expand_pattern(word)?;
            let chars: Vec<char> = v.chars().collect();
            let n = chars.len();
            let r = match op {
                "#" => (0..=n)
                    .find(|&k| pattern::matches(&p, &chars[..k]))
                    .map(|k| chars[k..].iter().collect()),
                "##" => (0..=n)
                    .rev()
                    .find(|&k| pattern::matches(&p, &chars[..k]))
                    .map(|k| chars[k..].iter().collect()),
                "%" => (0..=n)
                    .rev()
                    .find(|&k| pattern::matches(&p, &chars[k..]))
                    .map(|k| chars[..k].iter().collect()),
                _ => (0..=n)
                    .find(|&k| pattern::matches(&p, &chars[k..]))
                    .map(|k| chars[..k].iter().collect()),
            };
            Some(r.unwrap_or(v))
        } else if rest.starts_with('/') {
            let v = val.unwrap_or_default();
            let (all, body) = match rest.strip_prefix("//") {
                Some(b) => (true, b.to_string()),
                None => (false, rest.strip_prefix('/').unwrap_or(&rest).to_string()),
            };
            let (anchor, body) = match body.chars().next() {
                Some('#') => (1, body[1..].to_string()),
                Some('%') => (2, body[1..].to_string()),
                _ => (0, body),
            };
            let (pat, rep) = split_unescaped(&body, '/');
            let p = self.expand_pattern(&pat)?;
            let rep = self.expand_string(&rep)?;
            Some(replace(&v, &p, &rep, all, anchor))
        } else if let Some(r) = rest.strip_prefix(':') {
            // ${name:offset[:length]}
            let v = val.unwrap_or_default();
            let chars: Vec<char> = v.chars().collect();
            let (o, l) = match split_unescaped_opt(r, ':') {
                Some((a, b)) => (a, Some(b)),
                None => (r.to_string(), None),
            };
            let oe = self.expand_arith(&o)?;
            let off = crate::arith::eval_str(self, &oe).map_err(ExpandError)?;
            let n = chars.len() as i64;
            let start = if off < 0 {
                (n + off).max(0)
            } else {
                off.min(n)
            } as usize;
            let end = match l {
                Some(l) => {
                    let le = self.expand_arith(&l)?;
                    let len = crate::arith::eval_str(self, &le).map_err(ExpandError)?;
                    if len < 0 {
                        ((n + len).max(start as i64)) as usize
                    } else {
                        (start + len as usize).min(chars.len())
                    }
                }
                None => chars.len(),
            };
            Some(chars[start..end.max(start)].iter().collect())
        } else if let Some(op) = ["^^", ",,", "^", ","].iter().find(|o| rest == **o) {
            let v = val.unwrap_or_default();
            Some(match *op {
                "^^" => v.to_uppercase(),
                ",," => v.to_lowercase(),
                "^" => {
                    let mut c = v.chars();
                    c.next()
                        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                        .unwrap_or_default()
                }
                _ => {
                    let mut c = v.chars();
                    c.next()
                        .map(|f| f.to_lowercase().collect::<String>() + c.as_str())
                        .unwrap_or_default()
                }
            })
        } else {
            return Err(ExpandError(format!("${{{name}{rest}}}: bad substitution")));
        };
        Ok(result)
    }

    fn split_and_glob(&mut self, items: Vec<It>) -> Vec<String> {
        let ifs: Vec<char> = self
            .get_var("IFS")
            .unwrap_or(" \t\n".into())
            .chars()
            .collect();
        // fields as (chars, quoted flags, has_quoted_part)
        let mut fields: Vec<(Vec<char>, Vec<bool>, bool)> = Vec::new();
        let mut cur: (Vec<char>, Vec<bool>, bool) = (Vec::new(), Vec::new(), false);
        let mut cur_live = false; // the current field has started
        let mut only_empty_at = true;
        for it in items {
            match it {
                It::Break => {
                    fields.push(std::mem::take(&mut cur));
                    cur_live = false;
                }
                It::EmptyAt => {}
                It::C(c, q, exp) => {
                    only_empty_at = false;
                    if !q && exp && ifs.contains(&c) {
                        // split here; whitespace IFS chars collapse
                        if cur_live {
                            fields.push(std::mem::take(&mut cur));
                            cur_live = false;
                        } else if !c.is_whitespace() {
                            fields.push(std::mem::take(&mut cur));
                        }
                        continue;
                    }
                    if c == '\0' && q {
                        cur.2 = true;
                        cur_live = true;
                        continue;
                    }
                    cur.0.push(c);
                    cur.1.push(q);
                    if q {
                        cur.2 = true;
                    }
                    cur_live = true;
                }
            }
        }
        if cur_live || !cur.0.is_empty() || cur.2 {
            fields.push(cur);
        }
        let _ = only_empty_at;

        let mut result = Vec::new();
        for (chars, quoted, _) in fields {
            let p = pattern::compile(&chars, &quoted);
            if !self.noglob && pattern::has_magic(&p) {
                let mut hits = glob(&chars, &quoted);
                if !hits.is_empty() {
                    hits.sort_by(|a, b| collate(a, b));
                    result.extend(hits);
                    continue;
                }
            }
            result.push(chars.into_iter().collect());
        }
        result
    }

    /// Run `cmd` in a subshell and capture its output (trailing newlines removed).
    pub fn command_subst(&mut self, cmd: &str) -> String {
        self.subst_count += 1;
        let mut fds = [0i32; 2];
        // SAFETY: plain pipe/fork/dup2 sequence, single-threaded shell
        unsafe {
            if libc::pipe(fds.as_mut_ptr()) != 0 {
                return String::new();
            }
            let pid = libc::fork();
            if pid == 0 {
                libc::close(fds[0]);
                libc::dup2(fds[1], 1);
                libc::close(fds[1]);
                self.child_setup(-1, false);
                let code = self.run_source(cmd, false);
                self.exit_now(code);
            }
            libc::close(fds[1]);
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let n = libc::read(fds[0], chunk.as_mut_ptr() as *mut libc::c_void, chunk.len());
                if n <= 0 {
                    if n < 0 && *libc::__errno_location() == libc::EINTR {
                        continue;
                    }
                    break;
                }
                buf.extend_from_slice(&chunk[..n as usize]);
            }
            libc::close(fds[0]);
            self.last_status = crate::shell::wait_pid(pid);
            while buf.last() == Some(&b'\n') {
                buf.pop();
            }
            String::from_utf8_lossy(&buf).into_owned()
        }
    }
}

fn push_str(out: &mut Vec<It>, s: &str, quoted: bool, expanded: bool) {
    for c in s.chars() {
        out.push(It::C(c, quoted, expanded));
    }
}

fn join_items(items: &[It]) -> String {
    let mut s = String::new();
    for it in items {
        match it {
            It::C('\0', true, _) => {}
            It::C(c, _, _) => s.push(*c),
            It::Break => s.push(' '),
            It::EmptyAt => {}
        }
    }
    s
}

fn tilde(sh: &Shell, user: &str) -> Option<String> {
    if user.is_empty() {
        return sh.get_var("HOME");
    }
    if user == "+" {
        return sh.get_var("PWD");
    }
    if user == "-" {
        return sh.get_var("OLDPWD");
    }
    let c = std::ffi::CString::new(user).ok()?;
    // SAFETY: getpwnam returns static storage or null
    unsafe {
        let pw = libc::getpwnam(c.as_ptr());
        if pw.is_null() {
            return None;
        }
        Some(
            std::ffi::CStr::from_ptr((*pw).pw_dir)
                .to_string_lossy()
                .into_owned(),
        )
    }
}

fn backtick(s: &[char], i: usize) -> (String, usize) {
    let mut j = i + 1;
    let mut cmd = String::new();
    while j < s.len() && s[j] != '`' {
        if s[j] == '\\' && matches!(s.get(j + 1), Some('`' | '\\' | '$')) {
            cmd.push(s[j + 1]);
            j += 2;
            continue;
        }
        cmd.push(s[j]);
        j += 1;
    }
    (cmd, j + 1)
}

/// s[i] == '('; returns the index just after the matching ')'.
fn match_paren(s: &[char], i: usize) -> usize {
    let mut depth = 0;
    let mut j = i;
    while j < s.len() {
        match s[j] {
            '\\' => j += 1,
            '\'' => {
                j += 1;
                while j < s.len() && s[j] != '\'' {
                    j += 1;
                }
            }
            '"' => {
                j += 1;
                while j < s.len() && s[j] != '"' {
                    if s[j] == '\\' {
                        j += 1;
                    }
                    j += 1;
                }
            }
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return j + 1;
                }
            }
            _ => {}
        }
        j += 1;
    }
    s.len()
}

fn match_brace(s: &[char], i: usize) -> usize {
    let mut depth = 0;
    let mut j = i;
    while j < s.len() {
        match s[j] {
            '\\' => j += 1,
            '\'' => {
                j += 1;
                while j < s.len() && s[j] != '\'' {
                    j += 1;
                }
            }
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return j + 1;
                }
            }
            _ => {}
        }
        j += 1;
    }
    s.len()
}

fn split_op<'a>(rest: &'a str, ops: &[&'static str]) -> Option<(&'static str, &'a str)> {
    ops.iter()
        .find(|o| rest.starts_with(**o))
        .map(|o| (*o, &rest[o.len()..]))
}

fn split_unescaped(s: &str, sep: char) -> (String, String) {
    split_unescaped_opt(s, sep).unwrap_or((s.to_string(), String::new()))
}

fn split_unescaped_opt(s: &str, sep: char) -> Option<(String, String)> {
    let mut prev_bs = false;
    for (i, c) in s.char_indices() {
        if c == sep && !prev_bs {
            return Some((s[..i].to_string(), s[i + 1..].to_string()));
        }
        prev_bs = c == '\\' && !prev_bs;
    }
    None
}

fn replace(v: &str, p: &[P], rep: &str, all: bool, anchor: u8) -> String {
    let chars: Vec<char> = v.chars().collect();
    let n = chars.len();
    if anchor == 1 {
        return match (0..=n).rev().find(|&k| pattern::matches(p, &chars[..k])) {
            Some(k) => rep.to_string() + &chars[k..].iter().collect::<String>(),
            None => v.to_string(),
        };
    }
    if anchor == 2 {
        return match (0..=n).find(|&k| pattern::matches(p, &chars[k..])) {
            Some(k) => chars[..k].iter().collect::<String>() + rep,
            None => v.to_string(),
        };
    }
    let mut out = String::new();
    let mut i = 0;
    let mut done = false;
    while i < n {
        if !done {
            // longest match starting at i
            if let Some(k) = (i + 1..=n)
                .rev()
                .find(|&k| pattern::matches(p, &chars[i..k]))
            {
                out.push_str(rep);
                i = k;
                if !all {
                    done = true;
                }
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn ansi_escape(s: &[char]) -> (Option<char>, usize) {
    match s[0] {
        'n' => (Some('\n'), 1),
        't' => (Some('\t'), 1),
        'r' => (Some('\r'), 1),
        'a' => (Some('\x07'), 1),
        'b' => (Some('\x08'), 1),
        'e' | 'E' => (Some('\x1b'), 1),
        'f' => (Some('\x0c'), 1),
        'v' => (Some('\x0b'), 1),
        '\\' => (Some('\\'), 1),
        '\'' => (Some('\''), 1),
        '"' => (Some('"'), 1),
        'x' => {
            let hex: String = s[1..]
                .iter()
                .take(2)
                .take_while(|c| c.is_ascii_hexdigit())
                .collect();
            let v = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32);
            (v, 1 + hex.len())
        }
        '0'..='7' => {
            let oct: String = s
                .iter()
                .take(3)
                .take_while(|c| ('0'..='7').contains(c))
                .collect();
            (
                u32::from_str_radix(&oct, 8).ok().and_then(char::from_u32),
                oct.len(),
            )
        }
        c => (Some(c), 1),
    }
}

fn collate(a: &str, b: &str) -> std::cmp::Ordering {
    match (std::ffi::CString::new(a), std::ffi::CString::new(b)) {
        // SAFETY: valid C strings
        (Ok(x), Ok(y)) => unsafe { libc::strcoll(x.as_ptr(), y.as_ptr()) }.cmp(&0),
        _ => a.cmp(b),
    }
}

/// Filesystem globbing, component by component.
fn glob(chars: &[char], quoted: &[bool]) -> Vec<String> {
    // split into components on '/'
    let mut comps: Vec<(Vec<char>, Vec<bool>)> = vec![(Vec::new(), Vec::new())];
    for (c, q) in chars.iter().zip(quoted) {
        if *c == '/' {
            comps.push((Vec::new(), Vec::new()));
        } else {
            let last = comps.last_mut().unwrap();
            last.0.push(*c);
            last.1.push(*q);
        }
    }
    let absolute = chars.first() == Some(&'/');
    let mut paths: Vec<String> = vec![if absolute { "/".into() } else { String::new() }];
    let n = comps.len();
    for (idx, (cc, cq)) in comps.into_iter().enumerate() {
        if cc.is_empty() {
            if idx == 0 && absolute {
                continue;
            }
            // trailing slash: keep only directories
            if idx == n - 1 {
                paths = paths
                    .into_iter()
                    .filter(|p| std::path::Path::new(p).is_dir())
                    .map(|p| format!("{p}/"))
                    .collect();
            }
            continue;
        }
        let p = pattern::compile(&cc, &cq);
        let mut next = Vec::new();
        for base in &paths {
            if !pattern::has_magic(&p) {
                let lit: String = cc.iter().collect();
                let cand = join(base, &lit);
                if std::fs::symlink_metadata(&cand).is_ok() {
                    next.push(cand);
                }
                continue;
            }
            let dir = if base.is_empty() { "." } else { base.as_str() };
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            let want_hidden = cc.first() == Some(&'.');
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') && !want_hidden {
                    continue;
                }
                let nc: Vec<char> = name.chars().collect();
                if pattern::matches(&p, &nc) {
                    next.push(join(base, &name));
                }
            }
        }
        paths = next;
        if paths.is_empty() {
            break;
        }
    }
    paths
}

fn join(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.to_string()
    } else if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// Push a list of values: separate fields for "${a[@]}", joined for "${a[*]}".
fn push_list(out: &mut Vec<It>, vals: &[String], separate: bool, quoted: bool, sh: &Shell) {
    if !separate && quoted {
        let sep = sh
            .get_var("IFS")
            .map(|s| s.chars().next().map(|c| c.to_string()).unwrap_or_default())
            .unwrap_or(" ".into());
        push_str(out, &vals.join(&sep), true, true);
        return;
    }
    if vals.is_empty() && quoted {
        out.push(It::EmptyAt);
    }
    for (n, v) in vals.iter().enumerate() {
        if n > 0 {
            out.push(It::Break);
        }
        if v.is_empty() && quoted {
            out.push(It::C('\0', true, false));
        }
        push_str(out, v, quoted, true);
    }
}

/// Bash brace expansion: a{b,c}d -> abd acd, {1..5}, {a..e}, {01..10..2}.
pub fn brace_expand(raw: &str) -> Vec<String> {
    let c: Vec<char> = raw.chars().collect();
    // find the first unquoted '{' that has a matching '}' with a top-level ',' or '..'
    let mut i = 0;
    let mut quote: Option<char> = None;
    while i < c.len() {
        let ch = c[i];
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                } else if ch == '\\' && q == '"' {
                    i += 1;
                }
            }
            None => match ch {
                '\\' => i += 1,
                '\'' | '"' => quote = Some(ch),
                '$' if c.get(i + 1) == Some(&'{') => {
                    // skip ${...}
                    let mut depth = 0;
                    while i < c.len() {
                        if c[i] == '{' {
                            depth += 1;
                        } else if c[i] == '}' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        i += 1;
                    }
                }
                '{' => {
                    if let Some((end, parts)) = brace_body(&c, i) {
                        let pre: String = c[..i].iter().collect();
                        let post: String = c[end + 1..].iter().collect();
                        let mut out = Vec::new();
                        for p in parts {
                            for tail in brace_expand(&post) {
                                out.extend(brace_expand(&format!("{pre}{p}{tail}")));
                            }
                        }
                        return out;
                    }
                }
                _ => {}
            },
        }
        i += 1;
    }
    vec![raw.to_string()]
}

/// For a '{' at `start`: the matching '}' index and the alternatives.
fn brace_body(c: &[char], start: usize) -> Option<(usize, Vec<String>)> {
    let mut depth = 0;
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut i = start;
    while i < c.len() {
        let ch = c[i];
        if let Some(q) = quote {
            cur.push(ch);
            if ch == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match ch {
            '\\' => {
                cur.push(ch);
                if let Some(&n) = c.get(i + 1) {
                    cur.push(n);
                }
                i += 2;
                continue;
            }
            '\'' | '"' => {
                quote = Some(ch);
                cur.push(ch);
            }
            '{' => {
                if depth > 0 {
                    cur.push(ch);
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    parts.push(cur);
                    if parts.len() > 1 {
                        return Some((i, parts));
                    }
                    return sequence(&parts[0]).map(|v| (i, v));
                }
                cur.push(ch);
            }
            ',' if depth == 1 => parts.push(std::mem::take(&mut cur)),
            c2 if c2.is_whitespace() && depth >= 1 => return None,
            _ => cur.push(ch),
        }
        i += 1;
    }
    None
}

/// {1..5}, {5..1}, {a..e}, {01..10}, {1..10..3}
fn sequence(body: &str) -> Option<Vec<String>> {
    let parts: Vec<&str> = body.split("..").collect();
    if parts.len() != 2 && parts.len() != 3 {
        return None;
    }
    let step: i64 = if parts.len() == 3 {
        parts[2].parse::<i64>().ok()?.abs().max(1)
    } else {
        1
    };
    if let (Ok(a), Ok(b)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) {
        let width = if (parts[0].starts_with('0') && parts[0].len() > 1)
            || (parts[1].starts_with('0') && parts[1].len() > 1)
        {
            parts[0].len().max(parts[1].len())
        } else {
            0
        };
        let mut v = Vec::new();
        let mut x = a;
        loop {
            v.push(format!("{x:0width$}"));
            if x == b {
                break;
            }
            if a < b {
                x += step;
                if x > b {
                    break;
                }
            } else {
                x -= step;
                if x < b {
                    break;
                }
            }
        }
        return Some(v);
    }
    let (ca, cb) = (
        parts[0].chars().collect::<Vec<_>>(),
        parts[1].chars().collect::<Vec<_>>(),
    );
    if ca.len() == 1 && cb.len() == 1 && ca[0].is_ascii_alphabetic() && cb[0].is_ascii_alphabetic()
    {
        let (a, b) = (ca[0] as i64, cb[0] as i64);
        let mut v = Vec::new();
        let mut x = a;
        loop {
            v.push(char::from_u32(x as u32)?.to_string());
            if x == b {
                break;
            }
            x += if a < b { step } else { -step };
            if (a < b && x > b) || (a > b && x < b) {
                break;
            }
        }
        return Some(v);
    }
    None
}
