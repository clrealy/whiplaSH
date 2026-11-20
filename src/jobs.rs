//! Job control (process groups, Ctrl-Z, fg/bg/jobs/kill/disown) and traps.

use crate::shell::{Flow, Shell};
use std::sync::atomic::{AtomicBool, Ordering};

pub struct Job {
    pub id: usize,
    pub pgid: i32,
    /// processes not reaped yet
    pub pids: Vec<i32>,
    pub cmd: String,
    pub stopped: bool,
}

pub const EXIT_TRAP: i32 = 0;
pub const ERR_TRAP: i32 = -1;

const NSIG: usize = 65;
#[allow(clippy::declare_interior_mutable_const)]
const FALSE: AtomicBool = AtomicBool::new(false);
static PENDING: [AtomicBool; NSIG] = [FALSE; NSIG];

extern "C" fn on_signal(sig: libc::c_int) {
    if (sig as usize) < NSIG {
        PENDING[sig as usize].store(true, Ordering::SeqCst);
    }
}

const SIGNALS: &[(&str, i32)] = &[
    ("HUP", libc::SIGHUP),
    ("INT", libc::SIGINT),
    ("QUIT", libc::SIGQUIT),
    ("ILL", libc::SIGILL),
    ("TRAP", libc::SIGTRAP),
    ("ABRT", libc::SIGABRT),
    ("BUS", libc::SIGBUS),
    ("FPE", libc::SIGFPE),
    ("KILL", libc::SIGKILL),
    ("USR1", libc::SIGUSR1),
    ("SEGV", libc::SIGSEGV),
    ("USR2", libc::SIGUSR2),
    ("PIPE", libc::SIGPIPE),
    ("ALRM", libc::SIGALRM),
    ("TERM", libc::SIGTERM),
    ("CHLD", libc::SIGCHLD),
    ("CONT", libc::SIGCONT),
    ("STOP", libc::SIGSTOP),
    ("TSTP", libc::SIGTSTP),
    ("TTIN", libc::SIGTTIN),
    ("TTOU", libc::SIGTTOU),
    ("URG", libc::SIGURG),
    ("XCPU", libc::SIGXCPU),
    ("XFSZ", libc::SIGXFSZ),
    ("VTALRM", libc::SIGVTALRM),
    ("PROF", libc::SIGPROF),
    ("WINCH", libc::SIGWINCH),
    ("IO", libc::SIGIO),
    ("PWR", libc::SIGPWR),
    ("SYS", libc::SIGSYS),
];

/// "TERM", "SIGTERM", "term", "15" -> 15. EXIT -> 0, ERR -> -1.
pub fn parse_signal(s: &str) -> Option<i32> {
    let up = s.to_ascii_uppercase();
    let up = up.strip_prefix("SIG").unwrap_or(&up);
    match up {
        "EXIT" => return Some(EXIT_TRAP),
        "ERR" => return Some(ERR_TRAP),
        _ => {}
    }
    if let Ok(n) = up.parse::<i32>() {
        return Some(n);
    }
    SIGNALS.iter().find(|(n, _)| *n == up).map(|(_, v)| *v)
}

pub fn signal_name(sig: i32) -> String {
    match sig {
        EXIT_TRAP => "EXIT".into(),
        ERR_TRAP => "ERR".into(),
        _ => SIGNALS
            .iter()
            .find(|(_, v)| *v == sig)
            .map(|(n, _)| format!("SIG{n}"))
            .unwrap_or(sig.to_string()),
    }
}

impl Shell {
    /// Called in every forked child: back to default signals, no inherited traps.
    pub fn child_setup(&mut self, pgid: i32, foreground: bool) {
        self.in_subshell = true;
        self.interactive = false;
        if self.job_control && pgid >= 0 {
            // SAFETY: process-group + signal setup in the child
            unsafe {
                libc::setpgid(0, pgid);
                if foreground {
                    libc::tcsetpgrp(0, libc::getpgrp());
                }
                for s in [
                    libc::SIGINT,
                    libc::SIGQUIT,
                    libc::SIGTSTP,
                    libc::SIGTTIN,
                    libc::SIGTTOU,
                ] {
                    libc::signal(s, libc::SIG_DFL);
                }
            }
        }
        self.job_control = false;
        self.jobs.clear();
        // traps aren't inherited (ignored signals stay ignored)
        let keep: Vec<(i32, String)> = self
            .traps
            .iter()
            .filter(|(_, a)| a.is_empty())
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        for (sig, _) in self.traps.drain() {
            if sig > 0 && !keep.iter().any(|(k, _)| *k == sig) {
                // SAFETY: restoring the default disposition
                unsafe { libc::signal(sig, libc::SIG_DFL) };
            }
        }
        self.traps.extend(keep);
    }

    /// Parent side of a fork: put the child in the job's process group.
    pub fn parent_setup(&mut self, pid: i32, pgid: &mut i32) {
        if self.job_control {
            if *pgid == 0 {
                *pgid = pid;
            }
            // SAFETY: setpgid on our own child
            unsafe { libc::setpgid(pid, *pgid) };
        }
    }

    /// Wait for a foreground job. With job control, Ctrl-Z turns it into a stopped job.
    pub fn wait_fg(&mut self, pids: &[i32], pgid: i32, cmd: &str) -> Vec<i32> {
        if !self.job_control {
            return pids.iter().map(|&p| crate::shell::wait_pid(p)).collect();
        }
        // SAFETY: handing the terminal to the job's process group
        unsafe { libc::tcsetpgrp(0, pgid) };
        let mut statuses = Vec::new();
        let mut stopped_sig = None;
        let mut still: Vec<i32> = Vec::new();
        for &pid in pids {
            let mut st = 0;
            loop {
                // SAFETY: valid pointer
                let r = unsafe { libc::waitpid(pid, &mut st, libc::WUNTRACED) };
                // SAFETY: errno is always valid
                if r == -1 && unsafe { *libc::__errno_location() } == libc::EINTR {
                    continue;
                }
                break;
            }
            if libc::WIFSTOPPED(st) {
                stopped_sig = Some(libc::WSTOPSIG(st));
                still.push(pid);
                statuses.push(128 + libc::WSTOPSIG(st));
            } else {
                statuses.push(crate::shell::decode_status(st));
            }
        }
        // SAFETY: taking the terminal back
        unsafe { libc::tcsetpgrp(0, self.shell_pgid) };
        if stopped_sig.is_some() {
            let id = self.jobs.iter().map(|j| j.id).max().unwrap_or(0) + 1;
            eprintln!("\n[{id}]+  Stopped                 {cmd}");
            self.jobs.push(Job {
                id,
                pgid,
                pids: still,
                cmd: cmd.to_string(),
                stopped: true,
            });
        }
        statuses
    }

    /// Report finished/stopped background jobs (before each prompt).
    pub fn reap_jobs(&mut self) {
        let mut msgs = Vec::new();
        for j in &mut self.jobs {
            j.pids.retain(|&pid| {
                let mut st = 0;
                // SAFETY: valid pointer
                let r = unsafe {
                    libc::waitpid(
                        pid,
                        &mut st,
                        libc::WNOHANG | libc::WUNTRACED | libc::WCONTINUED,
                    )
                };
                if r == 0 {
                    return true;
                }
                if r == -1 {
                    return false;
                }
                if libc::WIFSTOPPED(st) {
                    j.stopped = true;
                    return true;
                }
                if libc::WIFCONTINUED(st) {
                    j.stopped = false;
                    return true;
                }
                if libc::WIFSIGNALED(st) {
                    // SAFETY: strsignal returns a static string
                    let name =
                        unsafe { std::ffi::CStr::from_ptr(libc::strsignal(libc::WTERMSIG(st))) }
                            .to_string_lossy()
                            .into_owned();
                    msgs.push((j.id, name));
                } else {
                    let code = crate::shell::decode_status(st);
                    if code != 0 {
                        msgs.push((j.id, format!("Exit {code}")));
                    }
                }
                false
            });
        }
        let mut done = Vec::new();
        for j in &self.jobs {
            if j.pids.is_empty() {
                let what = msgs
                    .iter()
                    .find(|(id, _)| *id == j.id)
                    .map(|(_, m)| m.clone())
                    .unwrap_or("Done".into());
                done.push((j.id, what, j.cmd.clone()));
            }
        }
        for (id, what, cmd) in done {
            self.jobs.retain(|j| j.id != id);
            if self.interactive {
                eprintln!("[{id}]+  {what:<24}{cmd}");
            }
        }
    }

    fn find_job(&self, spec: Option<&String>) -> Option<usize> {
        match spec.map(|s| s.as_str()) {
            None | Some("%%") | Some("%+") | Some("%") => self.jobs.len().checked_sub(1),
            Some("%-") => self.jobs.len().checked_sub(2),
            Some(s) => {
                let s = s.strip_prefix('%').unwrap_or(s);
                if let Ok(n) = s.parse::<usize>() {
                    self.jobs.iter().position(|j| j.id == n)
                } else {
                    self.jobs.iter().position(|j| j.cmd.starts_with(s))
                }
            }
        }
    }

    pub fn builtin_fg(&mut self, args: &[String]) -> Flow {
        let Some(i) = self.find_job(args.first()) else {
            eprintln!(
                "{}: fg: {}: no such job",
                self.source_name,
                args.first().map(|s| s.as_str()).unwrap_or("current")
            );
            self.last_status = 1;
            return Flow::Normal;
        };
        let job = self.jobs.remove(i);
        println!("{}", job.cmd);
        // SAFETY: waking the job's process group
        unsafe { libc::kill(-job.pgid, libc::SIGCONT) };
        let st = self.wait_fg(&job.pids, job.pgid, &job.cmd);
        self.last_status = *st.last().unwrap_or(&0);
        Flow::Normal
    }

    pub fn builtin_bg(&mut self, args: &[String]) -> Flow {
        let Some(i) = self.find_job(args.first()) else {
            eprintln!("{}: bg: no such job", self.source_name);
            self.last_status = 1;
            return Flow::Normal;
        };
        let j = &mut self.jobs[i];
        j.stopped = false;
        println!("[{}]+ {} &", j.id, j.cmd);
        // SAFETY: continuing the job's process group
        unsafe { libc::kill(-j.pgid, libc::SIGCONT) };
        self.last_status = 0;
        Flow::Normal
    }

    pub fn builtin_jobs(&mut self, args: &[String]) -> i32 {
        let pids_only = args.iter().any(|a| a == "-p");
        let n = self.jobs.len();
        for (k, j) in self.jobs.iter().enumerate() {
            if pids_only {
                println!("{}", j.pgid);
                continue;
            }
            let mark = if k + 1 == n {
                '+'
            } else if k + 2 == n {
                '-'
            } else {
                ' '
            };
            let state = if j.stopped { "Stopped" } else { "Running" };
            println!("[{}]{mark}  {state:<24}{}", j.id, j.cmd);
        }
        0
    }

    pub fn builtin_kill(&mut self, args: &[String]) -> i32 {
        if args.first().map(|a| a == "-l").unwrap_or(false) {
            let names: Vec<String> = SIGNALS
                .iter()
                .map(|(n, v)| format!("{v}) SIG{n}"))
                .collect();
            println!("{}", names.join("\t"));
            return 0;
        }
        let mut sig = libc::SIGTERM;
        let mut i = 0;
        if let Some(a) = args.first() {
            if a == "-s" || a == "-n" {
                match args.get(1).and_then(|s| parse_signal(s)) {
                    Some(s) => sig = s,
                    None => {
                        eprintln!("{}: kill: invalid signal specification", self.source_name);
                        return 1;
                    }
                }
                i = 2;
            } else if let Some(s) = a.strip_prefix('-')
                && !s.is_empty()
                && !s.starts_with('-')
            {
                match parse_signal(s) {
                    Some(n) => sig = n,
                    None => {
                        eprintln!(
                            "{}: kill: {s}: invalid signal specification",
                            self.source_name
                        );
                        return 1;
                    }
                }
                i = 1;
            }
        }
        if i >= args.len() {
            eprintln!(
                "{}: kill: usage: kill [-s sigspec | -n signum | -sigspec] pid | jobspec ...",
                self.source_name
            );
            return 2;
        }
        let mut code = 0;
        for t in &args[i..] {
            let target = if t.starts_with('%') {
                match self.find_job(Some(t)) {
                    Some(j) => -self.jobs[j].pgid,
                    None => {
                        eprintln!("{}: kill: {t}: no such job", self.source_name);
                        code = 1;
                        continue;
                    }
                }
            } else {
                match t.parse::<i32>() {
                    Ok(p) => p,
                    Err(_) => {
                        eprintln!(
                            "{}: kill: {t}: arguments must be process or job IDs",
                            self.source_name
                        );
                        code = 1;
                        continue;
                    }
                }
            };
            // SAFETY: sending a signal
            if unsafe { libc::kill(target, sig) } != 0 {
                let e = std::io::Error::last_os_error().to_string();
                eprintln!(
                    "{}: kill: ({t}) - {}",
                    self.source_name,
                    e.split(" (os error").next().unwrap_or(&e)
                );
                code = 1;
            } else if (sig == libc::SIGCONT || sig == libc::SIGKILL || sig == libc::SIGTERM)
                && let Some(j) = self.jobs.iter_mut().find(|j| -j.pgid == target)
            {
                j.stopped = false;
            }
        }
        code
    }

    pub fn builtin_disown(&mut self, args: &[String]) -> i32 {
        if args.iter().any(|a| a == "-a") {
            self.jobs.clear();
            return 0;
        }
        match self.find_job(args.first()) {
            Some(i) => {
                self.jobs.remove(i);
                0
            }
            None => {
                eprintln!("{}: disown: no such job", self.source_name);
                1
            }
        }
    }

    // ---------- traps ----------

    pub fn builtin_trap(&mut self, args: &[String]) -> i32 {
        if args.is_empty() || args[0] == "-p" {
            let mut v: Vec<_> = self.traps.iter().collect();
            v.sort_by_key(|(k, _)| **k);
            for (sig, action) in v {
                let name = signal_name(*sig);
                let name = name.strip_prefix("SIG").unwrap_or(&name).to_string();
                println!(
                    "trap -- '{}' {}",
                    action.replace('\'', "'\\''"),
                    if *sig > 0 { format!("SIG{name}") } else { name }
                );
            }
            return 0;
        }
        if args[0] == "-l" {
            return self.builtin_kill(&["-l".to_string()]);
        }
        let (action, sigs): (Option<&str>, &[String]) = if args.len() == 1 || args[0] == "-" {
            (None, if args[0] == "-" { &args[1..] } else { args })
        } else {
            (Some(args[0].as_str()), &args[1..])
        };
        let mut code = 0;
        for s in sigs {
            let Some(sig) = parse_signal(s) else {
                eprintln!(
                    "{}: trap: {s}: invalid signal specification",
                    self.source_name
                );
                code = 1;
                continue;
            };
            match action {
                None => {
                    self.traps.remove(&sig);
                    if sig > 0 {
                        let keep_ignored = self.interactive
                            && [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP].contains(&sig);
                        // SAFETY: restoring a disposition
                        unsafe {
                            libc::signal(
                                sig,
                                if keep_ignored {
                                    libc::SIG_IGN
                                } else {
                                    libc::SIG_DFL
                                },
                            )
                        };
                    }
                }
                Some(a) => {
                    if sig > 0 {
                        if sig == libc::SIGKILL || sig == libc::SIGSTOP {
                            continue;
                        }
                        let handler = if a.is_empty() {
                            libc::SIG_IGN
                        } else {
                            on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t
                        };
                        // SAFETY: installing a handler that only sets an atomic flag
                        unsafe { libc::signal(sig, handler) };
                    }
                    self.traps.insert(sig, a.to_string());
                }
            }
        }
        code
    }

    /// Run handlers for any signals that arrived.
    pub fn run_pending_traps(&mut self) -> Flow {
        if self.in_trap {
            return Flow::Normal;
        }
        for (sig, flag) in PENDING.iter().enumerate().skip(1) {
            if flag.swap(false, Ordering::SeqCst)
                && let Some(action) = self.traps.get(&(sig as i32)).cloned()
            {
                let f = self.run_trap_action(&action);
                if f != Flow::Normal {
                    return f;
                }
            }
        }
        Flow::Normal
    }

    pub fn run_trap_action(&mut self, action: &str) -> Flow {
        let saved = self.last_status;
        self.in_trap = true;
        let f = match crate::parser::parse(action) {
            Ok(list) => self.run_list(&list),
            Err(_) => Flow::Normal,
        };
        self.in_trap = false;
        if let Flow::Exit(_) = f {
            return f;
        }
        self.last_status = saved;
        Flow::Normal
    }

    /// Leave the shell, running the EXIT trap first.
    pub fn exit_now(&mut self, code: i32) -> ! {
        let mut code = code;
        if let Some(action) = self.traps.remove(&EXIT_TRAP) {
            self.last_status = code;
            if let Flow::Exit(c) = self.run_trap_action(&action) {
                code = c;
            }
        }
        crate::shell::flush_and_exit(code)
    }
}
