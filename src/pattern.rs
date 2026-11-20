//! Glob patterns (`*`, `?`, `[a-z]`, `[!x]`, `[[:alpha:]]`) for globbing,
//! `case`, `[[ == ]]` and `${var#pattern}`.

#[derive(Debug, Clone, PartialEq)]
pub enum P {
    Lit(char),
    Any,  // ?
    Star, // *
    Class { neg: bool, items: Vec<ClassItem> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClassItem {
    Ch(char),
    Range(char, char),
    Named(String),
}

/// Compile a pattern. `quoted[i]` marks characters that must match literally.
pub fn compile(chars: &[char], quoted: &[bool]) -> Vec<P> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let q = quoted.get(i).copied().unwrap_or(false);
        if q {
            out.push(P::Lit(c));
            i += 1;
            continue;
        }
        match c {
            '*' => {
                if out.last() != Some(&P::Star) {
                    out.push(P::Star);
                }
            }
            '?' => out.push(P::Any),
            '\\' if i + 1 < chars.len() => {
                i += 1;
                out.push(P::Lit(chars[i]));
            }
            '[' => {
                if let Some((p, next)) = class(chars, i) {
                    out.push(p);
                    i = next;
                    continue;
                }
                out.push(P::Lit('['));
            }
            _ => out.push(P::Lit(c)),
        }
        i += 1;
    }
    out
}

fn class(chars: &[char], start: usize) -> Option<(P, usize)> {
    let mut i = start + 1;
    let mut neg = false;
    if i < chars.len() && (chars[i] == '!' || chars[i] == '^') {
        neg = true;
        i += 1;
    }
    let mut items = Vec::new();
    let mut first = true;
    while i < chars.len() {
        let c = chars[i];
        if c == ']' && !first {
            return Some((P::Class { neg, items }, i + 1));
        }
        first = false;
        if c == '[' && chars.get(i + 1) == Some(&':') {
            let rest: String = chars[i + 2..].iter().collect();
            if let Some(end) = rest.find(":]") {
                items.push(ClassItem::Named(rest[..end].to_string()));
                i += 2 + end + 2;
                continue;
            }
        }
        let c = if c == '\\' && i + 1 < chars.len() {
            i += 1;
            chars[i]
        } else {
            c
        };
        if chars.get(i + 1) == Some(&'-') && chars.get(i + 2).is_some_and(|&x| x != ']') {
            items.push(ClassItem::Range(c, chars[i + 2]));
            i += 3;
        } else {
            items.push(ClassItem::Ch(c));
            i += 1;
        }
    }
    None
}

fn named(name: &str, c: char) -> bool {
    match name {
        "alpha" => c.is_alphabetic(),
        "digit" => c.is_ascii_digit(),
        "alnum" => c.is_alphanumeric(),
        "upper" => c.is_uppercase(),
        "lower" => c.is_lowercase(),
        "space" => c.is_whitespace(),
        "blank" => c == ' ' || c == '\t',
        "punct" => c.is_ascii_punctuation(),
        "xdigit" => c.is_ascii_hexdigit(),
        "cntrl" => c.is_control(),
        "print" => !c.is_control(),
        "graph" => !c.is_control() && c != ' ',
        _ => false,
    }
}

fn class_match(neg: bool, items: &[ClassItem], c: char) -> bool {
    let hit = items.iter().any(|it| match it {
        ClassItem::Ch(x) => *x == c,
        ClassItem::Range(a, b) => *a <= c && c <= *b,
        ClassItem::Named(n) => named(n, c),
    });
    hit != neg
}

/// Does the whole of `s` match?
pub fn matches(p: &[P], s: &[char]) -> bool {
    // iterative wildcard matching with backtracking on the last '*'
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() {
            match &p[pi] {
                P::Star => {
                    star = Some((pi, si));
                    pi += 1;
                    continue;
                }
                P::Any => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                P::Lit(c) if *c == s[si] => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                P::Class { neg, items } if class_match(*neg, items, s[si]) => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                _ => {}
            }
        }
        match star {
            Some((sp, ss)) => {
                pi = sp + 1;
                si = ss + 1;
                star = Some((sp, ss + 1));
            }
            None => return false,
        }
    }
    while pi < p.len() && p[pi] == P::Star {
        pi += 1;
    }
    pi == p.len()
}

pub fn has_magic(p: &[P]) -> bool {
    p.iter().any(|x| !matches!(x, P::Lit(_)))
}

/// Simple helper: compile an unquoted pattern string.
#[cfg(test)]
pub fn compile_str(s: &str) -> Vec<P> {
    let chars: Vec<char> = s.chars().collect();
    compile(&chars, &[])
}

#[cfg(test)]
pub fn match_str(pattern: &str, s: &str) -> bool {
    let p = compile_str(pattern);
    let c: Vec<char> = s.chars().collect();
    matches(&p, &c)
}

#[cfg(test)]
mod tests {
    use super::match_str;

    #[test]
    fn globs() {
        assert!(match_str("*.rs", "main.rs"));
        assert!(!match_str("*.rs", "main.rsx"));
        assert!(match_str("a?c", "abc"));
        assert!(match_str("[a-c]x", "bx"));
        assert!(!match_str("[!a-c]x", "bx"));
        assert!(match_str("[[:digit:]]*", "9lives"));
        assert!(match_str("*a*b*", "xxaxxbxx"));
        assert!(match_str("\\*", "*"));
        assert!(!match_str("\\*", "x"));
    }
}
