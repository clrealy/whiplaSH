//! Builtins: the bash ones scripts rely on, plus fish-style `set`, `and`,
//! `or`, `not` and `functions`.

use crate::parser;
use crate::shell::{Flow, Shell};
use std::io::Write;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

pub const BUILTINS: &[&str] = &[
    ":",
    ".",
    "[",
    "alias",
    "and",
    "break",
    "builtin",
    "cd",
    "command",
    "continue",
    "declare",
    "echo",
    "eval",
    "exec",
    "exit",
    "export",
    "false",
    "functions",
    "getopts",
    "help",
    "history",
    "jobs",
    "let",
    "local",
    "not",
    "or",
    "pwd",
    "read",
    "readonly",
    "return",
    "set",
    "shift",
    "source",
    "test",
    "true",
    "type",
    "typeset",
    "umask",
    "unalias",
    "unset",
    "wait",
];

pub const KEYWORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "while", "until", "do", "done", "for", "in", "case",
    "esac", "function", "end", "begin", "{", "}", "!", "[[", "]]",
];

/// Builtins whose variable assignments stick around (POSIX "special builtins").
pub fn is_special(name: &str) -> bool {
    matches!(
        name,
        ":" | "."
            | "break"
            | "continue"
            | "eval"
            | "exec"
            | "exit"
            | "export"
            | "readonly"
            | "return"
            | "set"
            | "shift"
            | "source"
            | "unset"
    )
}

pub fn is_builtin(name: &str) -> bool {
    BUILTINS.contains(&name)
}

fn err(sh: &mut Shell, msg: String, code: i32) -> Flow {
    eprintln!("{}: {msg}", sh.source_name);
    sh.last_status = code;
    Flow::Normal
}

pub fn run(sh: &mut Shell, argv: &[String]) -> Flow {
    let name = argv[0].as_str();
    let args = &argv[1..];
    sh.last_status = match name {
        ":" | "true" => 0,
        "false" => 1,
        "echo" => echo(args),
        "pwd" => pwd(sh, args),
        "cd" => return cd(sh, args),
        "exit" => {
            let code = match args.first() {
                Some(a) => match a.parse::<i64>() {
                    Ok(n) => (n & 0xff) as i32,
                    Err(_) => {
                        eprintln!("{}: exit: {a}: numeric argument required", sh.source_name);
                        2
                    }
                },
                None => sh.last_status,
            };
            return Flow::Exit(code);
        }
        "return" => {
            if sh.func_depth == 0 {
                return err(
                    sh,
                    "return: can only `return' from a function or sourced script".into(),
                    1,
                );
            }
            if let Some(a) = args.first() {
                match a.parse::<i64>() {
                    Ok(n) => sh.last_status = (n & 0xff) as i32,
                    Err(_) => {
                        eprintln!("{}: return: {a}: numeric argument required", sh.source_name);
                        sh.last_status = 2;
                    }
                }
            }
            return Flow::Return;
        }
        "break" | "continue" => {
            let n: u32 = args
                .first()
                .and_then(|a| a.parse().ok())
                .unwrap_or(1)
                .max(1);
            if sh.loop_depth == 0 {
                eprintln!(
                    "{}: {name}: only meaningful in a `for', `while', or `until' loop",
                    sh.source_name
                );
                sh.last_status = 0;
                return Flow::Normal;
            }
            let n = n.min(sh.loop_depth);
            sh.last_status = 0;
            return if name == "break" {
                Flow::Break(n)
            } else {
                Flow::Continue(n)
            };
        }
        "export" => export(sh, args),
        "readonly" => {
            for a in args.iter().filter(|a| !a.starts_with('-')) {
                if let Some((k, v)) = a.split_once('=') {
                    sh.set_var(k, v);
                }
            }
            0
        }
        "unset" => {
            let funcs = args.first().map(|a| a == "-f").unwrap_or(false);
            for a in args.iter().filter(|a| !a.starts_with('-')) {
                if funcs {
                    sh.functions.remove(a);
                } else if sh.vars.contains_key(a) {
                    sh.unset_var(a);
                } else {
                    sh.functions.remove(a);
                }
            }
            0
        }
        "set" => return set(sh, args),
        "shift" => {
            let n: usize = args.first().and_then(|a| a.parse().ok()).unwrap_or(1);
            if n > sh.positional.len() {
                1
            } else {
                sh.positional.drain(..n);
                0
            }
        }
        "source" | "." => return source(sh, args),
        "eval" => {
            let src = args.join(" ");
            return eval(sh, &src);
        }
        "read" => read(sh, args),
        "test" => test_cmd(sh, args, false),
        "[" => test_cmd(sh, args, true),
        "alias" => alias(sh, args),
        "unalias" => {
            if args.first().map(|a| a == "-a").unwrap_or(false) {
                sh.aliases.clear();
                0
            } else {
                let mut code = 0;
                for a in args {
                    if sh.aliases.remove(a).is_none() {
                        eprintln!("{}: unalias: {a}: not found", sh.source_name);
                        code = 1;
                    }
                }
                code
            }
        }
        "type" => type_cmd(sh, args),
        "local" => {
            if sh.func_depth == 0 {
                return err(sh, "local: can only be used in a function".into(), 1);
            }
            for a in args.iter().filter(|a| !a.starts_with('-')) {
                let (k, v) = match a.split_once('=') {
                    Some((k, v)) => (k, Some(v)),
                    None => (a.as_str(), None),
                };
                sh.make_local(k);
                match v {
                    Some(v) => sh.set_var(k, v),
                    None => {
                        if !sh.vars.contains_key(k) {
                            sh.set_var(k, "");
                        }
                    }
                }
            }
            0
        }
        "declare" | "typeset" => {
            let export_flag = args.iter().any(|a| a.starts_with('-') && a.contains('x'));
            let print_funcs = args.iter().any(|a| a == "-f" || a == "-F");
            if print_funcs {
                let mut names: Vec<_> = sh.functions.keys().cloned().collect();
                names.sort();
                for n in names {
                    println!("declare -f {n}");
                }
                return Flow::Normal;
            }
            for a in args.iter().filter(|a| !a.starts_with('-')) {
                let (k, v) = match a.split_once('=') {
                    Some((k, v)) => (k, Some(v)),
                    None => (a.as_str(), None),
                };
                if sh.func_depth > 0 && !args.iter().any(|x| x == "-g") {
                    sh.make_local(k);
                }
                if export_flag {
                    sh.export_var(k, v);
                } else if let Some(v) = v {
                    sh.set_var(k, v);
                } else if !sh.vars.contains_key(k) {
                    sh.set_var(k, "");
                }
            }
            0
        }
        "jobs" => {
            for j in &sh.jobs {
                println!("[{}]  Running                 {}", j.id, j.cmd);
            }
            0
        }
        "wait" => {
            let mut code = 0;
            if args.is_empty() {
                let pids: Vec<i32> = sh.jobs.iter().map(|j| j.pid).collect();
                for p in pids {
                    code = crate::shell::wait_pid(p);
                }
                sh.jobs.clear();
            } else {
                for a in args {
                    let pid = if let Some(id) = a.strip_prefix('%') {
                        sh.jobs
                            .iter()
                            .find(|j| j.id.to_string() == id)
                            .map(|j| j.pid)
                            .unwrap_or(-1)
                    } else {
                        a.parse().unwrap_or(-1)
                    };
                    code = if pid > 0 {
                        crate::shell::wait_pid(pid)
                    } else {
                        127
                    };
                    sh.jobs.retain(|j| j.pid != pid);
                }
            }
            code
        }
        "and" | "or" | "not" => {
            if name == "and" && sh.last_status != 0 {
                return Flow::Normal;
            }
            if name == "or" && sh.last_status == 0 {
                return Flow::Normal;
            }
            if args.is_empty() {
                return if name == "not" {
                    err(sh, "not: missing command".into(), 1)
                } else {
                    Flow::Normal
                };
            }
            let f = sh.dispatch(args.to_vec(), Vec::new(), &[], false, false);
            if name == "not" {
                sh.last_status = i32::from(sh.last_status == 0);
            }
            return f;
        }
        "let" => {
            let mut last = 0;
            for a in args {
                match crate::arith::eval_str(sh, a) {
                    Ok(v) => last = v,
                    Err(e) => return err(sh, format!("let: {e}"), 1),
                }
            }
            i32::from(last == 0)
        }
        "umask" => {
            match args.first() {
                Some(m) => match u32::from_str_radix(m, 8) {
                    // SAFETY: umask can't fail
                    Ok(v) => unsafe {
                        libc::umask(v as libc::mode_t);
                        0
                    },
                    Err(_) => return err(sh, format!("umask: {m}: invalid octal number"), 1),
                },
                None => {
                    // SAFETY: read and restore
                    let cur = unsafe {
                        let c = libc::umask(0o022);
                        libc::umask(c);
                        c
                    };
                    println!("{cur:04o}");
                    0
                }
            }
        }
        "command" => {
            if args.is_empty() {
                return Flow::Normal;
            }
            if args[0] == "-v" || args[0] == "-V" {
                let mut code = 0;
                for n in &args[1..] {
                    if sh.functions.contains_key(n)
                        || is_builtin(n)
                        || KEYWORDS.contains(&n.as_str())
                    {
                        println!("{n}");
                    } else if let Some(p) = sh.find_in_path(n) {
                        println!("{p}");
                    } else {
                        code = 1;
                    }
                }
                code
            } else {
                return sh.dispatch(args.to_vec(), Vec::new(), &[], false, true);
            }
        }
        "builtin" => {
            if args.is_empty() {
                return Flow::Normal;
            }
            if !is_builtin(&args[0]) {
                return err(sh, format!("builtin: {}: not a shell builtin", args[0]), 1);
            }
            return run(sh, args);
        }
        "functions" => {
            let mut names: Vec<_> = sh.functions.keys().cloned().collect();
            names.sort();
            for n in names {
                println!("{n}");
            }
            0
        }
        "history" => {
            let path = sh.get_var("HOME").unwrap_or_default() + "/.whiplash_history";
            let text = std::fs::read_to_string(path).unwrap_or_default();
            for (i, l) in text.lines().filter(|l| !l.starts_with('#')).enumerate() {
                println!("{:>5}  {l}", i + 1);
            }
            0
        }
        "getopts" => getopts(sh, args),
        "help" => {
            println!(
                "whiplaSH {} — bash scripts, fish vibes 🥁",
                env!("CARGO_PKG_VERSION")
            );
            println!("Builtins: {}", BUILTINS.join(" "));
            println!(
                "Blocks close with fi/done/esac OR end. `set NAME value` and `set -x NAME value` work like fish."
            );
            0
        }
        _ => 127,
    };
    Flow::Normal
}

fn echo(args: &[String]) -> i32 {
    let mut newline = true;
    let mut escapes = false;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a.len() > 1 && a.starts_with('-') && a[1..].chars().all(|c| matches!(c, 'n' | 'e' | 'E'))
        {
            for c in a[1..].chars() {
                match c {
                    'n' => newline = false,
                    'e' => escapes = true,
                    _ => escapes = false,
                }
            }
            i += 1;
        } else {
            break;
        }
    }
    let mut out = String::new();
    for (n, a) in args[i..].iter().enumerate() {
        if n > 0 {
            out.push(' ');
        }
        if escapes {
            let chars: Vec<char> = a.chars().collect();
            let mut j = 0;
            while j < chars.len() {
                if chars[j] == '\\' && j + 1 < chars.len() {
                    j += 1;
                    match chars[j] {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'a' => out.push('\x07'),
                        'b' => out.push('\x08'),
                        'e' | 'E' => out.push('\x1b'),
                        'f' => out.push('\x0c'),
                        'v' => out.push('\x0b'),
                        '\\' => out.push('\\'),
                        'c' => {
                            print!("{out}");
                            let _ = std::io::stdout().flush();
                            return 0;
                        }
                        '0' => {
                            let oct: String = chars[j + 1..]
                                .iter()
                                .take(3)
                                .take_while(|c| ('0'..='7').contains(c))
                                .collect();
                            j += oct.len();
                            out.push(
                                char::from_u32(
                                    u32::from_str_radix(if oct.is_empty() { "0" } else { &oct }, 8)
                                        .unwrap_or(0),
                                )
                                .unwrap_or('\0'),
                            );
                        }
                        'x' => {
                            let hex: String = chars[j + 1..]
                                .iter()
                                .take(2)
                                .take_while(|c| c.is_ascii_hexdigit())
                                .collect();
                            if hex.is_empty() {
                                out.push_str("\\x");
                            } else {
                                j += hex.len();
                                out.push(
                                    char::from_u32(u32::from_str_radix(&hex, 16).unwrap_or(0))
                                        .unwrap_or('\0'),
                                );
                            }
                        }
                        c => {
                            out.push('\\');
                            out.push(c);
                        }
                    }
                    j += 1;
                } else {
                    out.push(chars[j]);
                    j += 1;
                }
            }
        } else {
            out.push_str(a);
        }
    }
    if newline {
        out.push('\n');
    }
    let mut so = std::io::stdout();
    if so
        .write_all(out.as_bytes())
        .and_then(|_| so.flush())
        .is_err()
    {
        return 1;
    }
    0
}

fn pwd(sh: &Shell, args: &[String]) -> i32 {
    let physical = args.iter().any(|a| a == "-P");
    let p = if physical {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    } else {
        sh.get_var("PWD").unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        })
    };
    println!("{p}");
    0
}

fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            x => parts.push(x),
        }
    }
    format!("/{}", parts.join("/"))
}

fn cd(sh: &mut Shell, args: &[String]) -> Flow {
    let args: Vec<&String> = args.iter().filter(|a| *a != "-L" && *a != "-P").collect();
    let mut print = false;
    let target = match args.first().map(|s| s.as_str()) {
        None | Some("~") => match sh.get_var("HOME") {
            Some(h) => h,
            None => return err(sh, "cd: HOME not set".into(), 1),
        },
        Some("-") => match sh.get_var("OLDPWD") {
            Some(o) => {
                print = true;
                o
            }
            None => return err(sh, "cd: OLDPWD not set".into(), 1),
        },
        Some(t) => t.to_string(),
    };
    if args.len() > 1 {
        return err(sh, "cd: too many arguments".into(), 1);
    }
    let cur = sh.get_var("PWD").unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    });
    let logical = if target.starts_with('/') {
        normalize(&target)
    } else {
        normalize(&format!("{cur}/{target}"))
    };
    // try the logical path first (keeps symlinked paths), then the literal one
    let ok =
        std::env::set_current_dir(&logical).is_ok() || std::env::set_current_dir(&target).is_ok();
    if !ok {
        let e = std::env::set_current_dir(&target)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        let e = e.split(" (os error").next().unwrap_or("").to_string();
        return err(sh, format!("cd: {target}: {e}"), 1);
    }
    let newpwd = if std::path::Path::new(&logical).canonicalize().ok()
        == std::env::current_dir()
            .ok()
            .and_then(|p| p.canonicalize().ok())
    {
        logical
    } else {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or(logical)
    };
    sh.export_var("OLDPWD", Some(&cur));
    sh.export_var("PWD", Some(&newpwd));
    if print {
        println!("{newpwd}");
    }
    sh.last_status = 0;
    Flow::Normal
}

fn shell_quote(v: &str) -> String {
    format!("'{}'", v.replace('\'', "'\\''"))
}

fn export(sh: &mut Shell, args: &[String]) -> i32 {
    let unexport = args.iter().any(|a| a == "-n");
    let names: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    if names.is_empty() {
        let mut v: Vec<_> = sh.vars.iter().filter(|(_, v)| v.exported).collect();
        v.sort_by(|a, b| a.0.cmp(b.0));
        for (k, var) in v {
            println!(
                "declare -x {k}=\"{}\"",
                var.value
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('$', "\\$")
            );
        }
        return 0;
    }
    let mut code = 0;
    for a in names {
        let (k, v) = match a.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (a.as_str(), None),
        };
        if !parser::is_name(k) {
            eprintln!("{}: export: `{a}': not a valid identifier", sh.source_name);
            code = 1;
            continue;
        }
        if unexport {
            if let Some(var) = sh.vars.get_mut(k) {
                var.exported = false;
            }
        } else {
            sh.export_var(k, v);
        }
    }
    code
}

fn set(sh: &mut Shell, args: &[String]) -> Flow {
    if args.is_empty() {
        let mut v: Vec<_> = sh.vars.iter().collect();
        v.sort_by(|a, b| a.0.cmp(b.0));
        for (k, var) in v {
            let val = &var.value;
            let shown = if val
                .chars()
                .all(|c| c.is_alphanumeric() || "-_./:,+@%".contains(c))
                && !val.is_empty()
            {
                val.clone()
            } else {
                shell_quote(val)
            };
            println!("{k}={shown}");
        }
        sh.last_status = 0;
        return Flow::Normal;
    }
    // fish style: set [-x|-e|-g|-l|-q] NAME [values...]
    let mut i = 0;
    let mut fish_flags = String::new();
    while i < args.len()
        && args[i].starts_with('-')
        && args[i].len() > 1
        && args[i] != "--"
        && args[i][1..].chars().all(|c| "xeglqUu".contains(c))
    {
        fish_flags.push_str(&args[i][1..]);
        i += 1;
    }
    if i < args.len()
        && parser::is_name(&args[i])
        && !(fish_flags.is_empty() && args.len() == 1 && false)
    {
        let looks_fish = !fish_flags.is_empty() || !args.is_empty();
        if looks_fish {
            let name = &args[i];
            let values = &args[i + 1..];
            let joined = values.join(" ");
            if fish_flags.contains('q') {
                sh.last_status = i32::from(!sh.vars.contains_key(name));
                return Flow::Normal;
            }
            if fish_flags.contains('e') {
                sh.unset_var(name);
            } else {
                if fish_flags.contains('l') && sh.func_depth > 0 {
                    sh.make_local(name);
                }
                if fish_flags.contains('x') {
                    sh.export_var(name, Some(&joined));
                } else {
                    sh.set_var(name, &joined);
                }
                if fish_flags.contains('u')
                    && let Some(v) = sh.vars.get_mut(name)
                {
                    v.exported = false;
                }
            }
            sh.last_status = 0;
            return Flow::Normal;
        }
    }
    // bash style options
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            sh.positional = args[i + 1..].to_vec();
            sh.last_status = 0;
            return Flow::Normal;
        }
        let (on, rest) = match a.chars().next() {
            Some('-') => (true, &a[1..]),
            Some('+') => (false, &a[1..]),
            _ => {
                sh.positional = args[i..].to_vec();
                sh.last_status = 0;
                return Flow::Normal;
            }
        };
        for c in rest.chars() {
            match c {
                'e' => sh.errexit = on,
                'x' => sh.xtrace = on,
                'u' => sh.nounset = on,
                'f' => sh.noglob = on,
                'o' => {
                    i += 1;
                    match args.get(i).map(|s| s.as_str()) {
                        Some("errexit") => sh.errexit = on,
                        Some("xtrace") => sh.xtrace = on,
                        Some("nounset") => sh.nounset = on,
                        Some("noglob") => sh.noglob = on,
                        Some("pipefail") => sh.pipefail = on,
                        Some(
                            "vi"
                            | "emacs"
                            | "posix"
                            | "monitor"
                            | "histexpand"
                            | "history"
                            | "braceexpand"
                            | "interactive-comments"
                            | "hashall",
                        ) => {}
                        Some(other) => {
                            return err(sh, format!("set: {other}: invalid option name"), 2);
                        }
                        None => {
                            for (n, v) in [
                                ("errexit", sh.errexit),
                                ("noglob", sh.noglob),
                                ("nounset", sh.nounset),
                                ("pipefail", sh.pipefail),
                                ("xtrace", sh.xtrace),
                            ] {
                                println!("{n:<15}\t{}", if v { "on" } else { "off" });
                            }
                        }
                    }
                }
                'v' | 'h' | 'B' | 'H' | 'm' | 'b' | 'C' | 'a' | 'T' | 'E' | 'P' => {}
                _ => return err(sh, format!("set: -{c}: invalid option"), 2),
            }
        }
        i += 1;
    }
    sh.last_status = 0;
    Flow::Normal
}

fn eval(sh: &mut Shell, src: &str) -> Flow {
    match parser::parse(src) {
        Ok(list) => sh.run_list(&list),
        Err(parser::ParseError::Incomplete) => {
            err(sh, "eval: syntax error: unexpected end of file".into(), 2)
        }
        Err(parser::ParseError::Syntax(m)) => err(sh, format!("eval: {m}"), 2),
    }
}

fn source(sh: &mut Shell, args: &[String]) -> Flow {
    let Some(file) = args.first() else {
        return err(sh, "source: filename argument required".into(), 2);
    };
    let path = if file.contains('/') {
        Some(file.clone())
    } else {
        sh.find_in_path(file)
            .filter(|p| std::path::Path::new(p).is_file())
            .or(Some(file.clone()))
    };
    let text = match path.and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(t) => t,
        None => return err(sh, format!("{file}: No such file or directory"), 1),
    };
    let saved = if args.len() > 1 {
        Some(std::mem::replace(&mut sh.positional, args[1..].to_vec()))
    } else {
        None
    };
    sh.func_depth += 1; // lets `return` work inside sourced files
    let f = eval(sh, &text);
    sh.func_depth -= 1;
    if let Some(p) = saved {
        sh.positional = p;
    }
    match f {
        Flow::Return => Flow::Normal,
        other => other,
    }
}

fn read_byte() -> Option<u8> {
    let mut b = [0u8; 1];
    loop {
        // SAFETY: reading one byte into a valid buffer
        let n = unsafe { libc::read(0, b.as_mut_ptr() as *mut libc::c_void, 1) };
        if n == 1 {
            return Some(b[0]);
        }
        // SAFETY: errno is thread-local and always valid
        if n < 0 && unsafe { *libc::__errno_location() } == libc::EINTR {
            continue;
        }
        return None;
    }
}

fn read(sh: &mut Shell, args: &[String]) -> i32 {
    let mut raw = false;
    let mut prompt: Option<String> = None;
    let mut delim = b'\n';
    let mut names = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-r" => raw = true,
            "-p" => {
                i += 1;
                prompt = args.get(i).cloned();
            }
            "-d" => {
                i += 1;
                delim = args.get(i).and_then(|d| d.bytes().next()).unwrap_or(0);
            }
            "-s" => {}
            a if a.starts_with('-') && a.len() > 1 => {
                for c in a[1..].chars() {
                    if c == 'r' {
                        raw = true;
                    }
                }
            }
            n => names.push(n.to_string()),
        }
        i += 1;
    }
    if let Some(p) = prompt {
        eprint!("{p}");
    }
    let mut buf: Vec<u8> = Vec::new();
    let mut escaped_pos: Vec<bool> = Vec::new();
    let mut got_delim = false;
    while let Some(b) = read_byte() {
        if !raw && b == b'\\' {
            match read_byte() {
                Some(b'\n') => continue,
                Some(n) => {
                    buf.push(n);
                    escaped_pos.push(true);
                    continue;
                }
                None => break,
            }
        }
        if b == delim {
            got_delim = true;
            break;
        }
        buf.push(b);
        escaped_pos.push(false);
    }
    let line = String::from_utf8_lossy(&buf).into_owned();
    if names.is_empty() {
        sh.set_var("REPLY", &line);
        return i32::from(!got_delim);
    }
    let ifs: Vec<char> = sh
        .get_var("IFS")
        .unwrap_or(" \t\n".into())
        .chars()
        .collect();
    let is_ws = |c: char| ifs.contains(&c) && c.is_whitespace();
    let chars: Vec<char> = line.chars().collect();
    let mut pos = 0;
    while pos < chars.len() && is_ws(chars[pos]) {
        pos += 1;
    }
    for (k, name) in names.iter().enumerate() {
        if k + 1 == names.len() {
            let mut rest: String = chars[pos.min(chars.len())..].iter().collect();
            while rest.chars().last().is_some_and(is_ws) {
                rest.pop();
            }
            sh.set_var(name, &rest);
            break;
        }
        let st = pos;
        while pos < chars.len() && !ifs.contains(&chars[pos]) {
            pos += 1;
        }
        let field: String = chars[st..pos].iter().collect();
        sh.set_var(name, &field);
        // skip one separator (and surrounding whitespace)
        while pos < chars.len() && is_ws(chars[pos]) {
            pos += 1;
        }
        if pos < chars.len() && ifs.contains(&chars[pos]) && !chars[pos].is_whitespace() {
            pos += 1;
            while pos < chars.len() && is_ws(chars[pos]) {
                pos += 1;
            }
        }
    }
    i32::from(!got_delim)
}

fn alias(sh: &mut Shell, args: &[String]) -> i32 {
    if args.is_empty() {
        for (k, v) in &sh.aliases {
            println!("alias {k}={}", shell_quote(v));
        }
        return 0;
    }
    let mut code = 0;
    for a in args {
        match a.split_once('=') {
            Some((k, v)) => {
                sh.aliases.insert(k.to_string(), v.to_string());
            }
            None => match sh.aliases.get(a) {
                Some(v) => println!("alias {a}={}", shell_quote(v)),
                None => {
                    eprintln!("{}: alias: {a}: not found", sh.source_name);
                    code = 1;
                }
            },
        }
    }
    code
}

fn type_cmd(sh: &mut Shell, args: &[String]) -> i32 {
    let short = args.iter().any(|a| a == "-t");
    let mut code = 0;
    for n in args.iter().filter(|a| !a.starts_with('-')) {
        let (kind, long) = if let Some(a) = sh.aliases.get(n) {
            ("alias", format!("{n} is aliased to `{a}'"))
        } else if KEYWORDS.contains(&n.as_str()) {
            ("keyword", format!("{n} is a shell keyword"))
        } else if sh.functions.contains_key(n) {
            ("function", format!("{n} is a function"))
        } else if is_builtin(n) {
            ("builtin", format!("{n} is a shell builtin"))
        } else if let Some(p) = sh
            .find_in_path(n)
            .filter(|p| std::path::Path::new(p).is_file())
        {
            ("file", format!("{n} is {p}"))
        } else {
            if !short {
                eprintln!("{}: type: {n}: not found", sh.source_name);
            }
            code = 1;
            continue;
        };
        println!("{}", if short { kind.to_string() } else { long });
    }
    code
}

// ---------- test / [ / [[ ----------

fn file_test(op: &str, path: &str) -> Option<bool> {
    let meta = std::fs::metadata(path);
    let lmeta = std::fs::symlink_metadata(path);
    let access = |mode: i32| -> bool {
        let Ok(c) = std::ffi::CString::new(path) else {
            return false;
        };
        // SAFETY: valid C string
        unsafe { libc::access(c.as_ptr(), mode) == 0 }
    };
    Some(match op {
        "-e" | "-a" => meta.is_ok(),
        "-f" => meta.map(|m| m.is_file()).unwrap_or(false),
        "-d" => meta.map(|m| m.is_dir()).unwrap_or(false),
        "-L" | "-h" => lmeta.map(|m| m.file_type().is_symlink()).unwrap_or(false),
        "-s" => meta.map(|m| m.len() > 0).unwrap_or(false),
        "-r" => access(libc::R_OK),
        "-w" => access(libc::W_OK),
        "-x" => access(libc::X_OK),
        "-b" => meta
            .map(|m| m.file_type().is_block_device())
            .unwrap_or(false),
        "-c" => meta
            .map(|m| m.file_type().is_char_device())
            .unwrap_or(false),
        "-p" => meta.map(|m| m.file_type().is_fifo()).unwrap_or(false),
        "-S" => meta.map(|m| m.file_type().is_socket()).unwrap_or(false),
        "-u" => meta
            .map(|m| m.permissions().mode() & 0o4000 != 0)
            .unwrap_or(false),
        "-g" => meta
            .map(|m| m.permissions().mode() & 0o2000 != 0)
            .unwrap_or(false),
        "-k" => meta
            .map(|m| m.permissions().mode() & 0o1000 != 0)
            .unwrap_or(false),
        "-O" => meta
            .map(|m| m.uid() == unsafe { libc::geteuid() })
            .unwrap_or(false),
        "-G" => meta
            .map(|m| m.gid() == unsafe { libc::getegid() })
            .unwrap_or(false),
        "-N" => meta.map(|m| m.mtime() > m.atime()).unwrap_or(false),
        "-t" => {
            let fd: i32 = path.parse().unwrap_or(-1);
            // SAFETY: isatty on any int is fine
            unsafe { libc::isatty(fd) == 1 }
        }
        _ => return None,
    })
}

struct TestErr(String);

fn int_arg(s: &str) -> Result<i64, TestErr> {
    s.trim()
        .parse::<i64>()
        .map_err(|_| TestErr(format!("{s}: integer expression expected")))
}

fn binary(a: &str, op: &str, b: &str) -> Option<Result<bool, TestErr>> {
    let cmp =
        |f: fn(i64, i64) -> bool| -> Result<bool, TestErr> { Ok(f(int_arg(a)?, int_arg(b)?)) };
    let mtime = |p: &str| {
        std::fs::metadata(p)
            .ok()
            .map(|m| (m.mtime(), m.mtime_nsec()))
    };
    Some(match op {
        "=" | "==" => Ok(a == b),
        "!=" => Ok(a != b),
        "<" => Ok(a < b),
        ">" => Ok(a > b),
        "-eq" => cmp(|x, y| x == y),
        "-ne" => cmp(|x, y| x != y),
        "-lt" => cmp(|x, y| x < y),
        "-le" => cmp(|x, y| x <= y),
        "-gt" => cmp(|x, y| x > y),
        "-ge" => cmp(|x, y| x >= y),
        "-nt" => Ok(match (mtime(a), mtime(b)) {
            (Some(x), Some(y)) => x > y,
            (Some(_), None) => true,
            _ => false,
        }),
        "-ot" => Ok(match (mtime(a), mtime(b)) {
            (Some(x), Some(y)) => x < y,
            (None, Some(_)) => true,
            _ => false,
        }),
        "-ef" => Ok(match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }),
        _ => return None,
    })
}

fn is_binop(op: &str) -> bool {
    binary("0", op, "0").is_some()
}

fn unary(sh: &Shell, op: &str, arg: &str) -> Option<bool> {
    match op {
        "-z" => Some(arg.is_empty()),
        "-n" => Some(!arg.is_empty()),
        "-v" => Some(sh.vars.contains_key(arg)),
        _ => file_test(op, arg),
    }
}

/// POSIX test expression parser (with -a, -o, !, parentheses).
struct TestP<'a> {
    a: &'a [String],
    i: usize,
    sh: &'a Shell,
}

impl TestP<'_> {
    fn or(&mut self) -> Result<bool, TestErr> {
        let mut v = self.and()?;
        while self.a.get(self.i).map(|s| s == "-o").unwrap_or(false) {
            self.i += 1;
            let r = self.and()?;
            v = v || r;
        }
        Ok(v)
    }
    fn and(&mut self) -> Result<bool, TestErr> {
        let mut v = self.not()?;
        while self.a.get(self.i).map(|s| s == "-a").unwrap_or(false) {
            self.i += 1;
            let r = self.not()?;
            v = v && r;
        }
        Ok(v)
    }
    fn not(&mut self) -> Result<bool, TestErr> {
        if self.a.get(self.i).map(|s| s == "!").unwrap_or(false) && self.i + 1 < self.a.len() {
            self.i += 1;
            return Ok(!self.not()?);
        }
        self.primary()
    }
    fn primary(&mut self) -> Result<bool, TestErr> {
        let a = self.a;
        let Some(t) = a.get(self.i) else {
            return Err(TestErr("argument expected".into()));
        };
        if t == "(" && a.len() > self.i + 2 {
            self.i += 1;
            let v = self.or()?;
            if a.get(self.i).map(|s| s != ")").unwrap_or(true) {
                return Err(TestErr("')' expected".into()));
            }
            self.i += 1;
            return Ok(v);
        }
        if let (Some(op), Some(b)) = (a.get(self.i + 1), a.get(self.i + 2))
            && is_binop(op)
        {
            self.i += 3;
            return binary(t, op, b).unwrap();
        }
        if t.starts_with('-')
            && t.len() == 2
            && a.get(self.i + 1).is_some()
            && let Some(v) = unary(self.sh, t, &a[self.i + 1])
        {
            self.i += 2;
            return Ok(v);
        }
        self.i += 1;
        Ok(!t.is_empty())
    }
}

fn eval_test(sh: &Shell, a: &[String]) -> Result<bool, TestErr> {
    match a.len() {
        0 => Ok(false),
        1 => Ok(!a[0].is_empty()),
        2 => {
            if a[0] == "!" {
                return Ok(a[1].is_empty());
            }
            match unary(sh, &a[0], &a[1]) {
                Some(v) => Ok(v),
                None => Err(TestErr(format!("{}: unary operator expected", a[0]))),
            }
        }
        3 => {
            if let Some(r) = binary(&a[0], &a[1], &a[2]) {
                return r;
            }
            if a[0] == "!" {
                return Ok(!eval_test(sh, &a[1..])?);
            }
            if a[0] == "(" && a[2] == ")" {
                return Ok(!a[1].is_empty());
            }
            if a[1] == "-a" || a[1] == "-o" {
                let mut p = TestP { a, i: 0, sh };
                return p.or();
            }
            Err(TestErr(format!("{}: binary operator expected", a[1])))
        }
        4 if a[0] == "!" => Ok(!eval_test(sh, &a[1..])?),
        _ => {
            let mut p = TestP { a, i: 0, sh };
            let v = p.or()?;
            if p.i != a.len() {
                return Err(TestErr("too many arguments".into()));
            }
            Ok(v)
        }
    }
}

fn test_cmd(sh: &mut Shell, args: &[String], bracket: bool) -> i32 {
    let a: &[String] = if bracket {
        match args.last() {
            Some(l) if l == "]" => &args[..args.len() - 1],
            _ => {
                eprintln!("{}: [: missing `]'", sh.source_name);
                return 2;
            }
        }
    } else {
        args
    };
    match eval_test(sh, a) {
        Ok(v) => i32::from(!v),
        Err(TestErr(m)) => {
            eprintln!(
                "{}: {}: {m}",
                sh.source_name,
                if bracket { "[" } else { "test" }
            );
            2
        }
    }
}

impl Shell {
    /// [[ ... ]]: no splitting/globbing, && || ! ( ), pattern matching on ==.
    pub fn run_double_bracket(&mut self, words: &[String]) -> Flow {
        let mut i = 0;
        let r = self.dbl_or(words, &mut i);
        self.last_status = match r {
            Ok(v) if i == words.len() => i32::from(!v),
            Ok(_) => {
                eprintln!(
                    "{}: syntax error in conditional expression",
                    self.source_name
                );
                2
            }
            Err(TestErr(m)) => {
                eprintln!("{}: {m}", self.source_name);
                2
            }
        };
        Flow::Normal
    }

    fn dbl_or(&mut self, w: &[String], i: &mut usize) -> Result<bool, TestErr> {
        let mut v = self.dbl_and(w, i)?;
        while w.get(*i).map(|s| s == "||").unwrap_or(false) {
            *i += 1;
            let r = self.dbl_and(w, i)?;
            v = v || r;
        }
        Ok(v)
    }

    fn dbl_and(&mut self, w: &[String], i: &mut usize) -> Result<bool, TestErr> {
        let mut v = self.dbl_not(w, i)?;
        while w.get(*i).map(|s| s == "&&").unwrap_or(false) {
            *i += 1;
            let r = self.dbl_not(w, i)?;
            v = v && r;
        }
        Ok(v)
    }

    fn dbl_not(&mut self, w: &[String], i: &mut usize) -> Result<bool, TestErr> {
        if w.get(*i).map(|s| s == "!").unwrap_or(false) {
            *i += 1;
            return Ok(!self.dbl_not(w, i)?);
        }
        self.dbl_primary(w, i)
    }

    fn dbl_word(&mut self, raw: &str) -> Result<String, TestErr> {
        self.expand_string(raw).map_err(|e| TestErr(e.0))
    }

    fn dbl_primary(&mut self, w: &[String], i: &mut usize) -> Result<bool, TestErr> {
        let Some(t) = w.get(*i).cloned() else {
            return Err(TestErr("unexpected end of conditional expression".into()));
        };
        if t == "(" {
            *i += 1;
            let v = self.dbl_or(w, i)?;
            if w.get(*i).map(|s| s != ")").unwrap_or(true) {
                return Err(TestErr("expected `)'".into()));
            }
            *i += 1;
            return Ok(v);
        }
        // binary?
        if let (Some(op), Some(rhs)) = (w.get(*i + 1).cloned(), w.get(*i + 2).cloned()) {
            let is_op = matches!(op.as_str(), "==" | "=" | "!=" | "<" | ">" | "=~")
                || (op.starts_with('-') && is_binop(&op));
            if is_op {
                *i += 3;
                let l = self.dbl_word(&t)?;
                return match op.as_str() {
                    "==" | "=" | "!=" => {
                        let p = self.expand_pattern(&rhs).map_err(|e| TestErr(e.0))?;
                        let m = crate::pattern::matches(&p, &l.chars().collect::<Vec<_>>());
                        Ok(if op == "!=" { !m } else { m })
                    }
                    "=~" => Err(TestErr(
                        "=~: regular expressions aren't supported yet".into(),
                    )),
                    "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" => {
                        let r = self.dbl_word(&rhs)?;
                        let lv = crate::arith::eval_str(self, &l).map_err(TestErr)?;
                        let rv = crate::arith::eval_str(self, &r).map_err(TestErr)?;
                        binary(&lv.to_string(), &op, &rv.to_string()).unwrap()
                    }
                    _ => {
                        let r = self.dbl_word(&rhs)?;
                        binary(&l, &op, &r).unwrap()
                    }
                };
            }
        }
        if t.starts_with('-')
            && t.len() == 2
            && let Some(arg) = w.get(*i + 1).cloned()
            && !matches!(arg.as_str(), "&&" | "||" | ")")
        {
            let a = self.dbl_word(&arg)?;
            if let Some(v) = unary(self, &t, &a) {
                *i += 2;
                return Ok(v);
            }
        }
        *i += 1;
        Ok(!self.dbl_word(&t)?.is_empty())
    }
}

fn getopts(sh: &mut Shell, args: &[String]) -> i32 {
    if args.len() < 2 {
        eprintln!(
            "{}: getopts: usage: getopts optstring name [arg ...]",
            sh.source_name
        );
        return 2;
    }
    let spec = args[0].as_str();
    let var = args[1].clone();
    let list: Vec<String> = if args.len() > 2 {
        args[2..].to_vec()
    } else {
        sh.positional.clone()
    };
    let silent = spec.starts_with(':');
    let spec = spec.trim_start_matches(':');
    let mut optind: usize = sh
        .get_var("OPTIND")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .max(1);
    if optind == 1 && sh.getopts_pos == 0 {
        sh.getopts_pos = 1;
    }
    let done = |sh: &mut Shell, optind: usize| {
        sh.set_var(&var, "?");
        sh.set_var("OPTIND", &optind.to_string());
        sh.getopts_pos = 1;
        1
    };
    let Some(arg) = list.get(optind - 1).cloned() else {
        return done(sh, optind);
    };
    if sh.getopts_pos == 1 {
        if arg == "--" {
            return done(sh, optind + 1);
        }
        if !arg.starts_with('-') || arg == "-" {
            return done(sh, optind);
        }
    }
    let chars: Vec<char> = arg.chars().collect();
    let pos = sh.getopts_pos.min(chars.len().saturating_sub(1)).max(1);
    let c = chars[pos];
    let mut next_pos = pos + 1;
    let advance = |optind: &mut usize, next_pos: &mut usize| {
        if *next_pos >= chars.len() {
            *optind += 1;
            *next_pos = 1;
        }
    };
    sh.vars.remove("OPTARG");
    match spec.find(c) {
        Some(i) if c != ':' => {
            let wants_arg = spec[i + 1..].starts_with(':');
            sh.set_var(&var, &c.to_string());
            if wants_arg {
                if next_pos < chars.len() {
                    let v: String = chars[next_pos..].iter().collect();
                    sh.set_var("OPTARG", &v);
                    optind += 1;
                    next_pos = 1;
                } else if let Some(v) = list.get(optind) {
                    sh.set_var("OPTARG", v);
                    optind += 2;
                    next_pos = 1;
                } else {
                    optind += 1;
                    next_pos = 1;
                    if silent {
                        sh.set_var(&var, ":");
                        sh.set_var("OPTARG", &c.to_string());
                    } else {
                        eprintln!("{}: option requires an argument -- {c}", sh.source_name);
                        sh.set_var(&var, "?");
                    }
                }
            } else {
                advance(&mut optind, &mut next_pos);
            }
        }
        _ => {
            advance(&mut optind, &mut next_pos);
            sh.set_var(&var, "?");
            if silent {
                sh.set_var("OPTARG", &c.to_string());
            } else {
                eprintln!("{}: illegal option -- {c}", sh.source_name);
            }
        }
    }
    sh.getopts_pos = next_pos;
    sh.set_var("OPTIND", &optind.to_string());
    0
}
