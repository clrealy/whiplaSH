//! whiplaSH — runs bash scripts, feels like fish. Part of RNG's Not GNU!!!

mod arith;
mod ast;
mod builtins;
mod editor;
mod expand;
mod jobs;
mod parser;
mod pattern;
mod shell;

use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::{CompletionType, Config, EditMode, Editor};
use shell::{Flow, Shell};
use std::io::IsTerminal;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn usage() {
    println!("whiplaSH {VERSION} — bash scripts, fish vibes 🥁");
    println!();
    println!("Usage: whiplash [options] [script [args...]]");
    println!("       whiplash -c 'command' [name [args...]]");
    println!();
    println!("  -c CMD      run CMD and exit");
    println!("  -i          force interactive mode");
    println!("  -e -u -x    like set -e / -u / -x");
    println!("  -o pipefail");
    println!("  --norc      don't read ~/.whiplashrc");
    println!("  --version   print the version");
}

fn main() {
    // children should die from SIGPIPE normally (Rust ignores it by default)
    // SAFETY: setting a signal disposition
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    let mut command: Option<String> = None;
    let mut force_interactive = false;
    let mut norc = false;
    let mut opts = Vec::new();
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "-c" => {
                i += 1;
                match args.get(i) {
                    Some(c) => command = Some(c.clone()),
                    None => {
                        eprintln!("whiplash: -c: option requires an argument");
                        std::process::exit(2);
                    }
                }
            }
            "-i" => force_interactive = true,
            "--norc" | "--noprofile" | "-l" | "--login" => norc = norc || a == "--norc",
            "--version" | "-V" => {
                println!("whiplaSH {VERSION}");
                return;
            }
            "--help" | "-h" => {
                usage();
                return;
            }
            "-o" => {
                i += 1;
                if let Some(o) = args.get(i) {
                    opts.push(format!("-o {o}"));
                }
            }
            "--" => {
                i += 1;
                break;
            }
            s if s.starts_with('-')
                && s.len() > 1
                && s[1..].chars().all(|c| "euxfs".contains(c)) =>
            {
                opts.push(s.to_string())
            }
            _ => break,
        }
        i += 1;
    }

    let rest = &args[i.min(args.len())..];
    let (arg0, positional, script) = if command.is_some() {
        let a0 = rest.first().cloned().unwrap_or_else(|| "whiplash".into());
        (a0, rest.iter().skip(1).cloned().collect(), None)
    } else if let Some(s) = rest.first() {
        (s.clone(), rest[1..].to_vec(), Some(s.clone()))
    } else {
        (args[0].clone(), Vec::new(), None)
    };

    let mut sh = Shell::new(arg0, positional);
    for o in &opts {
        let words: Vec<String> = o.split(' ').map(String::from).collect();
        builtins::run(&mut sh, &[vec!["set".to_string()], words].concat());
    }

    if let Some(cmd) = command {
        let code = sh.run_source(&cmd, true);
        sh.exit_now(code);
    }
    if let Some(path) = script {
        let text = match std::fs::read(&path) {
            Ok(t) => String::from_utf8_lossy(&t).into_owned(),
            Err(e) => {
                let m = e.to_string();
                eprintln!(
                    "whiplash: {path}: {}",
                    m.split(" (os error").next().unwrap_or(&m)
                );
                std::process::exit(127);
            }
        };
        sh.source_name = path.clone();
        let code = run_chunks(&mut sh, text.lines().map(|l| l.to_string()));
        sh.exit_now(code);
    }
    let stdin_tty = std::io::stdin().is_terminal();
    if !stdin_tty && !force_interactive {
        let code = run_chunks(&mut sh, StdinLines);
        sh.exit_now(code);
    }
    interactive(&mut sh, norc);
}

/// Read stdin one byte at a time so commands like `read` see the rest.
struct StdinLines;

impl Iterator for StdinLines {
    type Item = String;
    fn next(&mut self) -> Option<String> {
        let mut buf = Vec::new();
        let mut b = [0u8; 1];
        loop {
            // SAFETY: reading one byte into a valid buffer
            let n = unsafe { libc::read(0, b.as_mut_ptr() as *mut libc::c_void, 1) };
            if n <= 0 {
                return if buf.is_empty() {
                    None
                } else {
                    Some(String::from_utf8_lossy(&buf).into_owned())
                };
            }
            if b[0] == b'\n' {
                return Some(String::from_utf8_lossy(&buf).into_owned());
            }
            buf.push(b[0]);
        }
    }
}

/// Run a script a command at a time, like bash (so an early `exit` or a
/// later syntax error behave the same).
fn run_chunks(sh: &mut Shell, lines: impl Iterator<Item = String>) -> i32 {
    let mut pending = String::new();
    let mut line_no = 0;
    for line in lines {
        line_no += 1;
        pending.push_str(&line);
        pending.push('\n');
        match parser::parse(&pending) {
            Ok(list) => {
                pending.clear();
                if let Flow::Exit(c) = sh.run_list(&list) {
                    return c;
                }
            }
            Err(parser::ParseError::Incomplete) => continue,
            Err(parser::ParseError::Syntax(m)) => {
                eprintln!("{}: line {line_no}: {m}", sh.source_name);
                return 2;
            }
        }
    }
    if !pending.trim().is_empty() {
        eprintln!(
            "{}: line {line_no}: syntax error: unexpected end of file",
            sh.source_name
        );
        return 2;
    }
    sh.last_status
}

/// Expand bash-style prompt escapes (\u \h \w \W \$ \n \t).
fn bash_prompt(sh: &Shell, ps1: &str) -> String {
    let mut out = String::new();
    let mut chars = ps1.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('u') => out.push_str(&sh.get_var("USER").unwrap_or_default()),
            Some('h') => out.push_str(hostname().split('.').next().unwrap_or("")),
            Some('H') => out.push_str(&hostname()),
            Some('w') => {
                let pwd = sh.get_var("PWD").unwrap_or_default();
                let home = sh.get_var("HOME").unwrap_or_default();
                if !home.is_empty() && pwd.starts_with(&home) {
                    out.push('~');
                    out.push_str(&pwd[home.len()..]);
                } else {
                    out.push_str(&pwd);
                }
            }
            Some('W') => out.push_str(
                sh.get_var("PWD")
                    .unwrap_or_default()
                    .rsplit('/')
                    .next()
                    .unwrap_or("/"),
            ),
            // SAFETY: geteuid can't fail
            Some('$') => out.push(if unsafe { libc::geteuid() } == 0 {
                '#'
            } else {
                '$'
            }),
            Some('n') => out.push('\n'),
            Some('e') => out.push('\x1b'),
            Some('[') | Some(']') => {}
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn interactive(sh: &mut Shell, norc: bool) {
    sh.interactive = true;
    // the shell itself shrugs off Ctrl-C / Ctrl-\ / Ctrl-Z; children get them
    // SAFETY: setting signal dispositions
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
        libc::signal(libc::SIGTSTP, libc::SIG_IGN);
        libc::signal(libc::SIGTTIN, libc::SIG_IGN);
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        // job control: our own process group, owning the terminal
        if libc::isatty(0) == 1 {
            let pid = libc::getpid();
            libc::setpgid(0, pid);
            if libc::tcsetpgrp(0, pid) == 0 {
                sh.shell_pgid = pid;
                sh.job_control = true;
            }
        }
    }
    let config = Config::builder()
        .history_ignore_space(true)
        .history_ignore_dups(true)
        .unwrap_or_default()
        .completion_type(CompletionType::List)
        .edit_mode(EditMode::Emacs)
        .auto_add_history(false)
        .max_history_size(10_000)
        .unwrap_or_default()
        .build();
    let mut ed: Editor<editor::WhipHelper, DefaultHistory> = match Editor::with_config(config) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("whiplash: can't start the line editor: {e}");
            let code = run_chunks(sh, StdinLines);
            shell::flush_and_exit(code);
        }
    };
    ed.set_helper(Some(editor::WhipHelper::new()));
    let home = sh.get_var("HOME").unwrap_or_default();
    let hist = format!("{home}/.whiplash_history");
    let _ = ed.load_history(&hist);

    if !norc {
        let rc = format!("{home}/.whiplashrc");
        if std::path::Path::new(&rc).is_file() {
            builtins::run(sh, &["source".to_string(), rc]);
        }
    }
    if sh.get_var("whiplash_greeting").as_deref() != Some("") {
        let g = sh
            .get_var("whiplash_greeting")
            .unwrap_or_else(|| "whiplaSH 🥁 not quite my tempo? type help".into());
        println!("{g}");
    }

    loop {
        sh.reap_jobs();
        // refresh what the highlighter knows about
        let path = sh.get_var("PATH").unwrap_or_default();
        let extra: Vec<String> = sh
            .functions
            .keys()
            .cloned()
            .chain(sh.aliases.keys().cloned())
            .collect();
        let (plain, colored) = match sh.get_var("PS1") {
            Some(ps1) if !ps1.is_empty() => {
                let p = bash_prompt(sh, &ps1);
                (p.clone(), p)
            }
            _ => {
                let pwd = editor::short_pwd(&sh.get_var("PWD").unwrap_or_default(), Some(&home));
                let status = sh.last_status;
                let mark = if status == 0 {
                    "\x1b[32m❯\x1b[0m".to_string()
                } else {
                    format!("\x1b[31m[{status}] ❯\x1b[0m")
                };
                let mark_plain = if status == 0 {
                    "❯".to_string()
                } else {
                    format!("[{status}] ❯")
                };
                (
                    format!("{pwd} {mark_plain} "),
                    format!("\x1b[36m{pwd}\x1b[0m {mark} "),
                )
            }
        };
        if let Some(h) = ed.helper_mut() {
            h.refresh_commands(&path, extra.into_iter());
            h.colored_prompt = colored;
        }
        match ed.readline(&plain) {
            Ok(line) => {
                if line.trim().is_empty() {
                    continue;
                }
                let _ = ed.add_history_entry(line.as_str());
                let _ = ed.append_history(&hist);
                sh.source_name = "whiplash".into();
                match parser::parse(&line) {
                    Ok(list) => {
                        if let Flow::Exit(c) = sh.run_list(&list) {
                            sh.exit_now(c);
                        }
                    }
                    Err(parser::ParseError::Incomplete) => {
                        eprintln!("whiplash: syntax error: unexpected end of input");
                        sh.last_status = 2;
                    }
                    Err(parser::ParseError::Syntax(m)) => {
                        eprintln!("whiplash: {m}");
                        sh.last_status = 2;
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                sh.last_status = 130;
                continue;
            }
            Err(ReadlineError::Eof) => {
                if sh.jobs.iter().any(|j| j.stopped) {
                    eprintln!("There are stopped jobs.");
                }
                println!("exit");
                let c = sh.last_status;
                sh.exit_now(c);
            }
            Err(e) => {
                eprintln!("whiplash: {e}");
                shell::flush_and_exit(1);
            }
        }
    }
}
