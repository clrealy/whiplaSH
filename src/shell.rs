//! Shell state and the executor: lists, pipelines, redirections, fork/exec.

use crate::ast::*;
use crate::parser::{self, ParseError};
use std::collections::{BTreeMap, HashMap};
use std::ffi::CString;
use std::io::Write;
use std::rc::Rc;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct Var {
    pub value: String,
    pub exported: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Flow {
    Normal,
    Break(u32),
    Continue(u32),
    Return,
    Exit(i32),
}

pub struct Job {
    pub id: usize,
    pub pid: i32,
    pub cmd: String,
}

pub struct Shell {
    pub vars: HashMap<String, Var>,
    pub functions: HashMap<String, Rc<Command>>,
    pub aliases: BTreeMap<String, String>,
    pub positional: Vec<String>,
    pub arg0: String,
    pub last_status: i32,
    pub pid: i32,
    pub last_bg: Option<i32>,
    pub locals: Vec<Vec<(String, Option<Var>)>>,
    pub errexit: bool,
    pub xtrace: bool,
    pub nounset: bool,
    pub noglob: bool,
    pub pipefail: bool,
    pub interactive: bool,
    pub in_subshell: bool,
    pub start: Instant,
    pub jobs: Vec<Job>,
    pub cond_depth: u32,
    pub func_depth: u32,
    pub loop_depth: u32,
    pub source_name: String,
    /// bumped by every command substitution (so we know $? came from one)
    pub subst_count: u64,
    /// getopts: position inside the current bundled option arg (-abc)
    pub getopts_pos: usize,
}

/// Flush Rust's stdout buffer and leave the process right now (for forked children).
pub fn flush_and_exit(code: i32) -> ! {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // SAFETY: _exit skips atexit handlers, which is what a forked child wants
    unsafe { libc::_exit(code) }
}

pub fn wait_pid(pid: i32) -> i32 {
    let mut st = 0;
    loop {
        // SAFETY: valid pointer to an int
        let r = unsafe { libc::waitpid(pid, &mut st, 0) };
        if r == -1 {
            // SAFETY: errno location is always valid
            if unsafe { *libc::__errno_location() } == libc::EINTR {
                continue;
            }
            return 127;
        }
        break;
    }
    decode_status(st)
}

pub fn decode_status(st: i32) -> i32 {
    if libc::WIFEXITED(st) {
        libc::WEXITSTATUS(st)
    } else if libc::WIFSIGNALED(st) {
        128 + libc::WTERMSIG(st)
    } else {
        1
    }
}

impl Shell {
    pub fn new(arg0: String, positional: Vec<String>) -> Self {
        let mut vars = HashMap::new();
        for (k, v) in std::env::vars_os() {
            vars.insert(
                k.to_string_lossy().into_owned(),
                Var {
                    value: v.to_string_lossy().into_owned(),
                    exported: true,
                },
            );
        }
        let lvl: i32 = vars
            .get("SHLVL")
            .and_then(|v| v.value.parse().ok())
            .unwrap_or(0);
        vars.insert(
            "SHLVL".into(),
            Var {
                value: (lvl + 1).to_string(),
                exported: true,
            },
        );
        if let Ok(cwd) = std::env::current_dir() {
            let pwd_ok = vars
                .get("PWD")
                .map(|p| {
                    std::path::Path::new(&p.value).canonicalize().ok() == cwd.canonicalize().ok()
                })
                .unwrap_or(false);
            if !pwd_ok {
                vars.insert(
                    "PWD".into(),
                    Var {
                        value: cwd.display().to_string(),
                        exported: true,
                    },
                );
            }
        }
        vars.entry("IFS".into()).or_insert(Var {
            value: " \t\n".into(),
            exported: false,
        });
        // SAFETY: getpid/getppid can't fail
        let pid = unsafe { libc::getpid() };
        let ppid = unsafe { libc::getppid() };
        vars.insert(
            "PPID".into(),
            Var {
                value: ppid.to_string(),
                exported: false,
            },
        );
        vars.insert(
            "WHIPLASH_VERSION".into(),
            Var {
                value: env!("CARGO_PKG_VERSION").into(),
                exported: false,
            },
        );
        Shell {
            vars,
            functions: HashMap::new(),
            aliases: BTreeMap::new(),
            positional,
            arg0,
            last_status: 0,
            pid,
            last_bg: None,
            locals: Vec::new(),
            errexit: false,
            xtrace: false,
            nounset: false,
            noglob: false,
            pipefail: false,
            interactive: false,
            in_subshell: false,
            start: Instant::now(),
            jobs: Vec::new(),
            cond_depth: 0,
            func_depth: 0,
            loop_depth: 0,
            source_name: "whiplash".into(),
            subst_count: 0,
            getopts_pos: 1,
        }
    }

    pub fn option_flags(&self) -> String {
        let mut s = String::new();
        if self.errexit {
            s.push('e');
        }
        if self.noglob {
            s.push('f');
        }
        if self.interactive {
            s.push('i');
        }
        if self.nounset {
            s.push('u');
        }
        if self.xtrace {
            s.push('x');
        }
        s
    }

    // ---------- variables ----------

    pub fn get_var(&self, name: &str) -> Option<String> {
        self.vars.get(name).map(|v| v.value.clone())
    }

    pub fn set_var(&mut self, name: &str, value: &str) {
        match self.vars.get_mut(name) {
            Some(v) => v.value = value.to_string(),
            None => {
                self.vars.insert(
                    name.to_string(),
                    Var {
                        value: value.to_string(),
                        exported: false,
                    },
                );
            }
        }
    }

    pub fn export_var(&mut self, name: &str, value: Option<&str>) {
        let v = self.vars.entry(name.to_string()).or_insert(Var {
            value: String::new(),
            exported: true,
        });
        v.exported = true;
        if let Some(val) = value {
            v.value = val.to_string();
        }
    }

    pub fn unset_var(&mut self, name: &str) {
        self.vars.remove(name);
    }

    /// `local NAME`: remember the old value so the function's exit restores it.
    pub fn make_local(&mut self, name: &str) {
        if let Some(frame) = self.locals.last_mut()
            && !frame.iter().any(|(n, _)| n == name)
        {
            frame.push((name.to_string(), self.vars.get(name).cloned()));
        }
    }

    fn env_block(&self, extra: &[(String, String)]) -> Vec<CString> {
        let mut map: BTreeMap<&str, &str> = BTreeMap::new();
        for (k, v) in &self.vars {
            if v.exported {
                map.insert(k, &v.value);
            }
        }
        for (k, v) in extra {
            map.insert(k, v);
        }
        map.iter()
            .filter_map(|(k, v)| CString::new(format!("{k}={v}")).ok())
            .collect()
    }

    // ---------- running source text ----------

    /// Parse and run a whole chunk of source. Returns the exit status.
    pub fn run_source(&mut self, src: &str, top: bool) -> i32 {
        match parser::parse(src) {
            Ok(list) => match self.run_list(&list) {
                Flow::Exit(c) => {
                    if top || self.in_subshell {
                        flush_and_exit(c);
                    }
                    c
                }
                _ => self.last_status,
            },
            Err(ParseError::Incomplete) => {
                eprintln!("{}: syntax error: unexpected end of file", self.source_name);
                self.last_status = 2;
                2
            }
            Err(ParseError::Syntax(m)) => {
                eprintln!("{}: {m}", self.source_name);
                self.last_status = 2;
                2
            }
        }
    }

    pub fn run_list(&mut self, list: &List) -> Flow {
        for ao in list {
            let f = self.run_andor(ao);
            if f != Flow::Normal {
                return f;
            }
        }
        Flow::Normal
    }

    fn run_andor(&mut self, ao: &AndOr) -> Flow {
        if ao.background {
            return self.run_background(ao);
        }
        let n = ao.rest.len();
        // everything except the last pipeline is a "condition" for set -e
        if n > 0 {
            self.cond_depth += 1;
        }
        let mut f = self.run_pipeline(&ao.first);
        let mut last_ran_negated = ao.first.negate;
        let mut last_ran_idx = 0;
        for (i, (conn, p)) in ao.rest.iter().enumerate() {
            if f != Flow::Normal {
                break;
            }
            let run = match conn {
                Connector::And => self.last_status == 0,
                Connector::Or => self.last_status != 0,
            };
            if !run {
                continue;
            }
            if i + 1 == n {
                self.cond_depth -= 1;
            }
            f = self.run_pipeline(p);
            last_ran_negated = p.negate;
            last_ran_idx = i + 1;
            if i + 1 == n {
                self.cond_depth += 1;
            }
        }
        if n > 0 {
            self.cond_depth -= 1;
        }
        if f == Flow::Normal
            && self.errexit
            && self.last_status != 0
            && self.cond_depth == 0
            && !last_ran_negated
            && last_ran_idx == n
        {
            return Flow::Exit(self.last_status);
        }
        f
    }

    fn run_background(&mut self, ao: &AndOr) -> Flow {
        // SAFETY: fork in a single-threaded program
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            self.in_subshell = true;
            self.interactive = false;
            let mut fg = ao.clone();
            fg.background = false;
            let f = self.run_andor(&fg);
            let code = if let Flow::Exit(c) = f {
                c
            } else {
                self.last_status
            };
            flush_and_exit(code);
        }
        self.last_bg = Some(pid);
        let id = self.jobs.iter().map(|j| j.id).max().unwrap_or(0) + 1;
        if self.interactive {
            eprintln!("[{id}] {pid}");
        }
        self.jobs.push(Job {
            id,
            pid,
            cmd: describe_andor(ao),
        });
        self.last_status = 0;
        Flow::Normal
    }

    /// Report finished background jobs (interactive prompt does this).
    pub fn reap_jobs(&mut self) {
        let mut done = Vec::new();
        for j in &self.jobs {
            let mut st = 0;
            // SAFETY: valid pointer
            let r = unsafe { libc::waitpid(j.pid, &mut st, libc::WNOHANG) };
            if r == j.pid || r == -1 {
                done.push((j.id, j.cmd.clone(), decode_status(st)));
            }
        }
        for (id, cmd, code) in done {
            self.jobs.retain(|j| j.id != id);
            if self.interactive {
                let what = if code == 0 {
                    "Done".to_string()
                } else {
                    format!("Exit {code}")
                };
                eprintln!("[{id}]+  {what:<24}{cmd}");
            }
        }
    }

    fn run_pipeline(&mut self, p: &Pipeline) -> Flow {
        if p.negate {
            self.cond_depth += 1;
        }
        let f = if p.cmds.len() == 1 {
            self.run_command(&p.cmds[0], false)
        } else {
            self.run_multi(&p.cmds)
        };
        if p.negate {
            self.cond_depth -= 1;
            self.last_status = i32::from(self.last_status == 0);
        }
        f
    }

    fn run_multi(&mut self, cmds: &[Command]) -> Flow {
        let _ = std::io::stdout().flush();
        let mut pids = Vec::new();
        let mut prev_read: i32 = -1;
        for (i, cmd) in cmds.iter().enumerate() {
            let last = i + 1 == cmds.len();
            let mut fds = [-1i32; 2];
            // SAFETY: standard pipe + fork plumbing; every fd is closed on both sides
            unsafe {
                if !last && libc::pipe(fds.as_mut_ptr()) != 0 {
                    eprintln!("{}: pipe failed", self.source_name);
                    self.last_status = 1;
                    return Flow::Normal;
                }
                let pid = libc::fork();
                if pid == 0 {
                    if prev_read != -1 {
                        libc::dup2(prev_read, 0);
                        libc::close(prev_read);
                    }
                    if !last {
                        libc::dup2(fds[1], 1);
                        libc::close(fds[1]);
                        libc::close(fds[0]);
                    }
                    self.in_subshell = true;
                    self.interactive = false;
                    let f = self.run_command(cmd, true);
                    let code = if let Flow::Exit(c) = f {
                        c
                    } else {
                        self.last_status
                    };
                    flush_and_exit(code);
                }
                if prev_read != -1 {
                    libc::close(prev_read);
                }
                if !last {
                    libc::close(fds[1]);
                    prev_read = fds[0];
                }
                pids.push(pid);
            }
        }
        let mut statuses = Vec::new();
        for pid in pids {
            statuses.push(wait_pid(pid));
        }
        self.last_status = if self.pipefail {
            statuses
                .iter()
                .rev()
                .find(|&&s| s != 0)
                .copied()
                .unwrap_or(0)
        } else {
            *statuses.last().unwrap_or(&0)
        };
        Flow::Normal
    }

    /// Run any command. `in_child`: we're already a forked child (exec directly).
    pub fn run_command(&mut self, cmd: &Command, in_child: bool) -> Flow {
        match cmd {
            Command::Simple {
                assigns,
                words,
                redirs,
            } => self.run_simple(assigns, words, redirs, in_child),
            Command::Function { name, body } => {
                self.functions
                    .insert(name.clone(), Rc::new((**body).clone()));
                self.last_status = 0;
                Flow::Normal
            }
            Command::Subshell(list, redirs) => {
                let _ = std::io::stdout().flush();
                if in_child {
                    return self.with_redirs(redirs, |sh| sh.run_list(list));
                }
                // SAFETY: fork in a single-threaded program
                let pid = unsafe { libc::fork() };
                if pid == 0 {
                    self.in_subshell = true;
                    self.interactive = false;
                    let f = self.with_redirs(redirs, |sh| sh.run_list(list));
                    let code = if let Flow::Exit(c) = f {
                        c
                    } else {
                        self.last_status
                    };
                    flush_and_exit(code);
                }
                self.last_status = wait_pid(pid);
                Flow::Normal
            }
            Command::Group(list, redirs) => self.with_redirs(redirs, |sh| sh.run_list(list)),
            Command::If {
                branches,
                otherwise,
                redirs,
            } => self.with_redirs(redirs, |sh| {
                for (cond, body) in branches {
                    sh.cond_depth += 1;
                    let f = sh.run_list(cond);
                    sh.cond_depth -= 1;
                    if f != Flow::Normal {
                        return f;
                    }
                    if sh.last_status == 0 {
                        return sh.run_list(body);
                    }
                }
                match otherwise {
                    Some(b) => sh.run_list(b),
                    None => {
                        sh.last_status = 0;
                        Flow::Normal
                    }
                }
            }),
            Command::While {
                until,
                cond,
                body,
                redirs,
            } => self.with_redirs(redirs, |sh| {
                let mut status = 0;
                sh.loop_depth += 1;
                let result = loop {
                    sh.cond_depth += 1;
                    let f = sh.run_list(cond);
                    sh.cond_depth -= 1;
                    if f != Flow::Normal {
                        break f;
                    }
                    if (sh.last_status == 0) == *until {
                        break Flow::Normal;
                    }
                    match sh.run_list(body) {
                        Flow::Normal => {}
                        Flow::Break(n) => {
                            status = sh.last_status;
                            break if n > 1 {
                                Flow::Break(n - 1)
                            } else {
                                Flow::Normal
                            };
                        }
                        Flow::Continue(n) if n > 1 => break Flow::Continue(n - 1),
                        Flow::Continue(_) => {}
                        f => break f,
                    }
                    status = sh.last_status;
                };
                sh.loop_depth -= 1;
                sh.last_status = status;
                result
            }),
            Command::For {
                var,
                items,
                body,
                redirs,
            } => self.with_redirs(redirs, |sh| {
                let values = match items {
                    Some(ws) => {
                        let mut v = Vec::new();
                        for w in ws {
                            match sh.expand_fields(w) {
                                Ok(f) => v.extend(f),
                                Err(e) => return sh.expand_failed(e),
                            }
                        }
                        v
                    }
                    None => sh.positional.clone(),
                };
                let mut status = 0;
                sh.loop_depth += 1;
                let mut result = Flow::Normal;
                for v in values {
                    sh.set_var(var, &v);
                    match sh.run_list(body) {
                        Flow::Normal => {}
                        Flow::Break(n) => {
                            result = if n > 1 {
                                Flow::Break(n - 1)
                            } else {
                                Flow::Normal
                            };
                            status = sh.last_status;
                            break;
                        }
                        Flow::Continue(n) if n > 1 => {
                            result = Flow::Continue(n - 1);
                            break;
                        }
                        Flow::Continue(_) => {}
                        f => {
                            result = f;
                            break;
                        }
                    }
                    status = sh.last_status;
                }
                sh.loop_depth -= 1;
                sh.last_status = status;
                result
            }),
            Command::Case { word, arms, redirs } => self.with_redirs(redirs, |sh| {
                let w = match sh.expand_string(word) {
                    Ok(w) => w,
                    Err(e) => return sh.expand_failed(e),
                };
                let wc: Vec<char> = w.chars().collect();
                for (pats, body) in arms {
                    for p in pats {
                        let pat = match sh.expand_pattern(p) {
                            Ok(p) => p,
                            Err(e) => return sh.expand_failed(e),
                        };
                        if crate::pattern::matches(&pat, &wc) {
                            return sh.run_list(body);
                        }
                    }
                }
                sh.last_status = 0;
                Flow::Normal
            }),
            Command::Arith(expr, redirs) => self.with_redirs(redirs, |sh| {
                let e = match sh.expand_arith(expr) {
                    Ok(e) => e,
                    Err(e) => return sh.expand_failed(e),
                };
                match crate::arith::eval_str(sh, &e) {
                    Ok(v) => sh.last_status = i32::from(v == 0),
                    Err(m) => {
                        eprintln!("{}: {}: {m}", sh.source_name, expr.trim());
                        sh.last_status = 1;
                    }
                }
                Flow::Normal
            }),
        }
    }

    pub fn expand_failed(&mut self, e: crate::expand::ExpandError) -> Flow {
        eprintln!("{}: {}", self.source_name, e.0);
        self.last_status = 1;
        if !self.interactive {
            return Flow::Exit(1);
        }
        Flow::Normal
    }

    fn with_redirs<F: FnOnce(&mut Shell) -> Flow>(&mut self, redirs: &[Redir], f: F) -> Flow {
        if redirs.is_empty() {
            return f(self);
        }
        let saved = match self.apply_redirs(redirs, true) {
            Ok(s) => s,
            Err(msg) => {
                eprintln!("{}: {msg}", self.source_name);
                self.last_status = 1;
                return Flow::Normal;
            }
        };
        let r = f(self);
        self.restore(saved);
        r
    }

    fn run_simple(
        &mut self,
        assigns: &[(String, String)],
        words: &[String],
        redirs: &[Redir],
        in_child: bool,
    ) -> Flow {
        // aliases (first word, literally written)
        if let Some(first) = words.first().filter(|_| self.interactive)
            && let Some(val) = self.aliases.get(first).cloned()
            && let Ok(list) = parser::parse(&val)
            && let [
                AndOr {
                    first:
                        Pipeline {
                            negate: false,
                            cmds,
                        },
                    rest,
                    background: false,
                },
            ] = list.as_slice()
            && rest.is_empty()
            && cmds.len() == 1
            && let Command::Simple { words: aw, .. } = &cmds[0]
            && (aw.first() != Some(first) || aw.len() > 1 || words.len() > 1)
        {
            let mut nw = aw.clone();
            nw.extend_from_slice(&words[1..]);
            if aw.first() != Some(first) {
                return self.run_simple(assigns, &nw, redirs, in_child);
            }
            return self.run_simple_expanded(assigns, &nw, redirs, in_child);
        }
        self.run_simple_expanded(assigns, words, redirs, in_child)
    }

    fn run_simple_expanded(
        &mut self,
        assigns: &[(String, String)],
        words: &[String],
        redirs: &[Redir],
        in_child: bool,
    ) -> Flow {
        let prev_status = self.last_status;
        let subst_before = self.subst_count;
        // expand the words
        let mut argv: Vec<String> = Vec::new();
        if words.first().map(|w| w == "[[").unwrap_or(false) {
            return self.run_double_bracket(&words[1..]);
        }
        for w in words {
            match self.expand_fields(w) {
                Ok(f) => argv.extend(f),
                Err(e) => return self.expand_failed(e),
            }
        }
        // expand assignment values
        let mut values = Vec::new();
        for (k, raw) in assigns {
            let (k, append) = match k.strip_suffix('+') {
                Some(base) => (base.to_string(), true),
                None => (k.clone(), false),
            };
            match self.expand_assign(raw) {
                Ok(v) => {
                    let v = if append {
                        self.get_var(&k).unwrap_or_default() + &v
                    } else {
                        v
                    };
                    values.push((k, v));
                }
                Err(e) => return self.expand_failed(e),
            }
        }
        let subst_status = if self.subst_count != subst_before {
            self.last_status
        } else {
            0
        };

        if self.xtrace {
            let mut line = String::new();
            for (k, v) in &values {
                line.push_str(&format!("{k}={} ", trace_quote(v)));
            }
            line.push_str(
                &argv
                    .iter()
                    .map(|a| trace_quote(a))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            let ps4 = self.get_var("PS4").unwrap_or("+ ".into());
            eprintln!("{ps4}{}", line.trim_end());
        }

        if argv.is_empty() {
            for (k, v) in &values {
                self.set_var(k, v);
            }
            if !redirs.is_empty() {
                match self.apply_redirs(redirs, true) {
                    Ok(s) => self.restore(s),
                    Err(m) => {
                        eprintln!("{}: {m}", self.source_name);
                        self.last_status = 1;
                        return Flow::Normal;
                    }
                }
            }
            self.last_status = subst_status;
            return Flow::Normal;
        }

        self.last_status = prev_status;
        self.dispatch(argv, values, redirs, in_child, false)
    }

    /// Run an already-expanded command: function, builtin or program.
    pub fn dispatch(
        &mut self,
        argv: Vec<String>,
        values: Vec<(String, String)>,
        redirs: &[Redir],
        in_child: bool,
        skip_functions: bool,
    ) -> Flow {
        let prev_status = self.last_status;
        let name = argv[0].clone();
        // `exec` with only redirections makes them permanent
        if name == "exec" {
            if argv.len() == 1 {
                if let Err(m) = self.apply_redirs(redirs, false) {
                    eprintln!("{}: {m}", self.source_name);
                    self.last_status = 1;
                }
                return Flow::Normal;
            }
            if let Err(m) = self.apply_redirs(redirs, false) {
                eprintln!("{}: {m}", self.source_name);
                self.last_status = 1;
                return Flow::Normal;
            }
            let _ = std::io::stdout().flush();
            self.exec_external(&argv[1..], &values);
        }
        // functions
        if let Some(body) = if skip_functions {
            None
        } else {
            self.functions.get(&name).cloned()
        } {
            return self.with_redirs(redirs, |sh| {
                let saved_vals: Vec<(String, Option<Var>)> = values
                    .iter()
                    .map(|(k, _)| (k.clone(), sh.vars.get(k).cloned()))
                    .collect();
                for (k, v) in &values {
                    sh.export_var(k, Some(v));
                }
                let saved_pos = std::mem::replace(&mut sh.positional, argv[1..].to_vec());
                sh.locals.push(Vec::new());
                sh.func_depth += 1;
                sh.last_status = 0;
                let f = sh.run_command(&body, false);
                sh.func_depth -= 1;
                if let Some(frame) = sh.locals.pop() {
                    for (n, old) in frame.into_iter().rev() {
                        match old {
                            Some(v) => {
                                sh.vars.insert(n, v);
                            }
                            None => {
                                sh.vars.remove(&n);
                            }
                        }
                    }
                }
                sh.positional = saved_pos;
                for (n, old) in saved_vals {
                    match old {
                        Some(v) => {
                            sh.vars.insert(n, v);
                        }
                        None => {
                            sh.vars.remove(&n);
                        }
                    }
                }
                match f {
                    Flow::Return => Flow::Normal,
                    other => other,
                }
            });
        }
        // builtins
        if crate::builtins::is_builtin(&name) {
            let special = crate::builtins::is_special(&name);
            let saved_vals: Vec<(String, Option<Var>)> = values
                .iter()
                .map(|(k, _)| (k.clone(), self.vars.get(k).cloned()))
                .collect();
            for (k, v) in &values {
                if special {
                    self.set_var(k, v);
                } else {
                    self.export_var(k, Some(v));
                }
            }
            let saved = match self.apply_redirs(redirs, true) {
                Ok(s) => s,
                Err(m) => {
                    eprintln!("{}: {m}", self.source_name);
                    self.last_status = 1;
                    return Flow::Normal;
                }
            };
            self.last_status = prev_status;
            let f = crate::builtins::run(self, &argv);
            let _ = std::io::stdout().flush();
            self.restore(saved);
            if !special {
                for (n, old) in saved_vals {
                    match old {
                        Some(v) => {
                            self.vars.insert(n, v);
                        }
                        None => {
                            self.vars.remove(&n);
                        }
                    }
                }
            }
            return f;
        }
        // external program
        if in_child {
            if let Err(m) = self.apply_redirs(redirs, false) {
                eprintln!("{}: {m}", self.source_name);
                flush_and_exit(1);
            }
            self.exec_external(&argv, &values);
        }
        let _ = std::io::stdout().flush();
        // SAFETY: fork in a single-threaded program
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            if let Err(m) = self.apply_redirs(redirs, false) {
                eprintln!("{}: {m}", self.source_name);
                flush_and_exit(1);
            }
            self.exec_external(&argv, &values);
        }
        self.last_status = wait_pid(pid);
        if self.interactive && self.last_status > 128 {
            let sig = self.last_status - 128;
            if sig != libc::SIGINT && sig != libc::SIGPIPE {
                // SAFETY: strsignal returns a static string
                let s = unsafe { std::ffi::CStr::from_ptr(libc::strsignal(sig)) }
                    .to_string_lossy()
                    .into_owned();
                eprintln!("{s}");
            } else if sig == libc::SIGINT {
                eprintln!();
            }
        }
        Flow::Normal
    }

    /// Find a program on $PATH.
    pub fn find_in_path(&self, name: &str) -> Option<String> {
        if name.contains('/') {
            return Some(name.to_string());
        }
        let path = self
            .get_var("PATH")
            .unwrap_or("/usr/local/bin:/usr/bin:/bin".into());
        for dir in path.split(':') {
            let dir = if dir.is_empty() { "." } else { dir };
            let cand = format!("{dir}/{name}");
            if let Ok(m) = std::fs::metadata(&cand) {
                use std::os::unix::fs::PermissionsExt;
                if m.is_file() && m.permissions().mode() & 0o111 != 0 {
                    return Some(cand);
                }
            }
        }
        None
    }

    fn exec_external(&mut self, argv: &[String], extra_env: &[(String, String)]) -> ! {
        let name = &argv[0];
        let Some(path) = self.find_in_path(name) else {
            eprintln!("{}: {name}: command not found", self.source_name);
            flush_and_exit(127);
        };
        let cpath = CString::new(path.clone()).unwrap_or_default();
        let cargs: Vec<CString> = argv
            .iter()
            .filter_map(|a| CString::new(a.as_str()).ok())
            .collect();
        let env = self.env_block(extra_env);
        let mut aptr: Vec<*const libc::c_char> = cargs.iter().map(|c| c.as_ptr()).collect();
        aptr.push(std::ptr::null());
        let mut eptr: Vec<*const libc::c_char> = env.iter().map(|c| c.as_ptr()).collect();
        eptr.push(std::ptr::null());
        let _ = std::io::stdout().flush();
        // SAFETY: null-terminated arrays of valid C strings; signals back to default for the child
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGQUIT, libc::SIG_DFL);
            libc::signal(libc::SIGTSTP, libc::SIG_DFL);
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            libc::execve(cpath.as_ptr(), aptr.as_ptr(), eptr.as_ptr());
            let err = *libc::__errno_location();
            if err == libc::ENOEXEC {
                // no #! line: run it as a shell script, like bash does
                let src = std::fs::read_to_string(&path).unwrap_or_default();
                self.arg0 = path.clone();
                self.positional = argv[1..].to_vec();
                for (k, v) in extra_env {
                    self.export_var(k, Some(v));
                }
                let code = self.run_source(&src, true);
                flush_and_exit(code);
            }
            let msg = std::io::Error::from_raw_os_error(err).to_string();
            let msg = msg.split(" (os error").next().unwrap_or(&msg).to_string();
            if err == libc::ENOENT {
                eprintln!("{}: {name}: No such file or directory", self.source_name);
                flush_and_exit(127);
            }
            if std::path::Path::new(&path).is_dir() {
                eprintln!("{}: {name}: Is a directory", self.source_name);
            } else {
                eprintln!("{}: {name}: {msg}", self.source_name);
            }
            flush_and_exit(126);
        }
    }

    // ---------- redirections ----------

    /// Apply redirections. With `save`, returns (fd, saved copy or -1) to restore later.
    pub fn apply_redirs(
        &mut self,
        redirs: &[Redir],
        save: bool,
    ) -> Result<Vec<(i32, i32)>, String> {
        let _ = std::io::stdout().flush();
        let mut saved = Vec::new();
        for r in redirs {
            let mut targets = vec![r.fd];
            let src_fd: i32 = match &r.kind {
                RedirKind::HereDoc { expand } => {
                    let body = if *expand {
                        self.expand_heredoc(&r.target).map_err(|e| e.0)?
                    } else {
                        r.target.clone()
                    };
                    temp_fd(body.as_bytes())?
                }
                RedirKind::HereString => {
                    let w = self.expand_string(&r.target).map_err(|e| e.0)?;
                    temp_fd(format!("{w}\n").as_bytes())?
                }
                RedirKind::DupOut | RedirKind::DupIn => {
                    let t = self.expand_string(&r.target).map_err(|e| e.0)?;
                    if t == "-" {
                        if save {
                            saved.push((r.fd, save_fd(r.fd)));
                        }
                        // SAFETY: closing an fd number
                        unsafe { libc::close(r.fd) };
                        continue;
                    }
                    let digits = t.strip_suffix('-').unwrap_or(&t);
                    if let Ok(n) = digits.parse::<i32>() {
                        // SAFETY: fcntl just checks the fd is open
                        if unsafe { libc::fcntl(n, libc::F_GETFD) } == -1 {
                            return Err(format!("{n}: Bad file descriptor"));
                        }
                        if save {
                            saved.push((r.fd, save_fd(r.fd)));
                        }
                        // SAFETY: both fds valid
                        unsafe {
                            libc::dup2(n, r.fd);
                            if t.ends_with('-') {
                                libc::close(n);
                            }
                        }
                        continue;
                    }
                    // `>&file` means `&>file`
                    targets = vec![1, 2];
                    open_file(&t, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC)?
                }
                kind => {
                    let t = self.expand_fields(&r.target).map_err(|e| e.0)?;
                    if t.len() != 1 {
                        return Err(format!("{}: ambiguous redirect", r.target));
                    }
                    let t = &t[0];
                    let flags = match kind {
                        RedirKind::In => libc::O_RDONLY,
                        RedirKind::Out | RedirKind::Both => {
                            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC
                        }
                        RedirKind::Append | RedirKind::BothAppend => {
                            libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND
                        }
                        _ => libc::O_RDWR | libc::O_CREAT,
                    };
                    if matches!(kind, RedirKind::Both | RedirKind::BothAppend) {
                        targets = vec![1, 2];
                    }
                    open_file(t, flags)?
                }
            };
            for fd in targets {
                if save {
                    saved.push((fd, save_fd(fd)));
                }
                // SAFETY: src_fd is open
                unsafe { libc::dup2(src_fd, fd) };
            }
            // SAFETY: we own src_fd
            unsafe { libc::close(src_fd) };
        }
        Ok(saved)
    }

    pub fn restore(&mut self, saved: Vec<(i32, i32)>) {
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        for (fd, copy) in saved.into_iter().rev() {
            // SAFETY: restoring fds we saved ourselves
            unsafe {
                if copy >= 0 {
                    libc::dup2(copy, fd);
                    libc::close(copy);
                } else {
                    libc::close(fd);
                }
            }
        }
    }
}

fn save_fd(fd: i32) -> i32 {
    // SAFETY: duplicating an fd (or -1 if it isn't open)
    unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) }
}

fn open_file(path: &str, flags: i32) -> Result<i32, String> {
    let c = CString::new(path).map_err(|_| format!("{path}: bad file name"))?;
    // SAFETY: valid C string
    let fd = unsafe { libc::open(c.as_ptr(), flags | libc::O_CLOEXEC, 0o666) };
    if fd < 0 {
        let e = std::io::Error::last_os_error().to_string();
        let e = e.split(" (os error").next().unwrap_or(&e).to_string();
        return Err(format!("{path}: {e}"));
    }
    Ok(high_fd(fd))
}

/// Move an fd out of the 0-9 range scripts use, so dup2 never hits itself.
fn high_fd(fd: i32) -> i32 {
    if fd >= 10 {
        return fd;
    }
    // SAFETY: duplicating and closing an fd we own
    unsafe {
        let n = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10);
        if n < 0 {
            return fd;
        }
        libc::close(fd);
        n
    }
}

/// An fd reading back `data` (for heredocs): an unlinked temp file.
fn temp_fd(data: &[u8]) -> Result<i32, String> {
    let mut tmpl = *b"/tmp/whiplash-heredoc-XXXXXX\0";
    // SAFETY: template is a writable, NUL-terminated buffer
    unsafe {
        let fd = libc::mkstemp(tmpl.as_mut_ptr() as *mut libc::c_char);
        if fd < 0 {
            return Err("cannot create temp file for here-document".into());
        }
        libc::unlink(tmpl.as_ptr() as *const libc::c_char);
        let mut off = 0;
        while off < data.len() {
            let n = libc::write(
                fd,
                data[off..].as_ptr() as *const libc::c_void,
                data.len() - off,
            );
            if n <= 0 {
                break;
            }
            off += n as usize;
        }
        libc::lseek(fd, 0, libc::SEEK_SET);
        Ok(high_fd(fd))
    }
}

pub fn trace_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || "-_./=:,+@%^".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn describe_andor(ao: &AndOr) -> String {
    fn cmd(c: &Command) -> String {
        match c {
            Command::Simple { words, .. } => words.join(" "),
            _ => "(compound)".into(),
        }
    }
    let mut s = ao
        .first
        .cmds
        .iter()
        .map(cmd)
        .collect::<Vec<_>>()
        .join(" | ");
    for (c, p) in &ao.rest {
        s.push_str(if *c == Connector::And { " && " } else { " || " });
        s.push_str(&p.cmds.iter().map(cmd).collect::<Vec<_>>().join(" | "));
    }
    s
}
