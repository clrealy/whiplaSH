//! $(( )) and (( )) — integer arithmetic like bash (64-bit, wrapping).

use crate::shell::Shell;

#[derive(Debug, Clone, PartialEq)]
enum T {
    Num(i64),
    Name(String),
    Op(&'static str),
    LParen,
    RParen,
}

const OPS: [&str; 34] = [
    "<<=", ">>=", "**", "++", "--", "<<", ">>", "<=", ">=", "==", "!=", "&&", "||", "+=", "-=",
    "*=", "/=", "%=", "&=", "|=", "^=", "+", "-", "*", "/", "%", "<", ">", "&", "|", "^", "!", "~",
    "=",
];

fn lex(s: &str) -> Result<Vec<T>, String> {
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() {
            let st = i;
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || b[i] == '#' || b[i] == '_' || b[i] == '@')
            {
                i += 1;
            }
            let txt: String = b[st..i].iter().collect();
            out.push(T::Num(parse_num(&txt)?));
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let st = i;
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_') {
                i += 1;
            }
            // array element: name[expr]
            if i < b.len() && b[i] == '[' {
                let mut depth = 0;
                while i < b.len() {
                    if b[i] == '[' {
                        depth += 1;
                    } else if b[i] == ']' {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    i += 1;
                }
            }
            out.push(T::Name(b[st..i].iter().collect()));
            continue;
        }
        match c {
            '(' => {
                out.push(T::LParen);
                i += 1;
                continue;
            }
            ')' => {
                out.push(T::RParen);
                i += 1;
                continue;
            }
            '?' => {
                out.push(T::Op("?"));
                i += 1;
                continue;
            }
            ':' => {
                out.push(T::Op(":"));
                i += 1;
                continue;
            }
            ',' => {
                out.push(T::Op(","));
                i += 1;
                continue;
            }
            _ => {}
        }
        let rest: String = b[i..].iter().take(3).collect();
        match OPS.iter().find(|o| rest.starts_with(**o)) {
            Some(o) => {
                out.push(T::Op(o));
                i += o.len();
            }
            None => {
                return Err(format!(
                    "syntax error: operand expected (error token is \"{}\")",
                    b[i..].iter().collect::<String>()
                ));
            }
        }
    }
    Ok(out)
}

fn parse_num(t: &str) -> Result<i64, String> {
    let bad = || format!("{t}: value too great for base (error token is \"{t}\")");
    if let Some((base, digits)) = t.split_once('#') {
        let base: u32 = base.parse().map_err(|_| bad())?;
        return i64::from_str_radix(digits, base).map_err(|_| bad());
    }
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return i64::from_str_radix(h, 16).map_err(|_| bad());
    }
    if t.len() > 1 && t.starts_with('0') {
        return i64::from_str_radix(&t[1..], 8).map_err(|_| bad());
    }
    t.parse().map_err(|_| bad())
}

struct P<'a> {
    t: Vec<T>,
    i: usize,
    sh: &'a mut Shell,
    /// when false we're in a short-circuited branch: don't assign
    live: bool,
}

impl P<'_> {
    fn peek(&self) -> Option<&T> {
        self.t.get(self.i)
    }
    fn op(&self) -> Option<&'static str> {
        match self.peek() {
            Some(T::Op(o)) => Some(o),
            _ => None,
        }
    }
    fn var(&mut self, n: &str) -> Result<i64, String> {
        let v = match n.split_once('[') {
            Some((name, rest)) => self
                .sh
                .get_elem(name, rest.strip_suffix(']').unwrap_or(rest))?
                .unwrap_or_default(),
            None => self.sh.get_var(n).unwrap_or_default(),
        };
        let v = v.trim();
        if v.is_empty() {
            return Ok(0);
        }
        eval_str(self.sh, v)
    }
    fn set(&mut self, n: &str, v: i64) {
        if self.live {
            match n.split_once('[') {
                Some((name, rest)) => {
                    let _ = self.sh.set_elem(
                        name,
                        rest.strip_suffix(']').unwrap_or(rest),
                        &v.to_string(),
                    );
                }
                None => self.sh.set_var(n, &v.to_string()),
            }
        }
    }

    fn comma(&mut self) -> Result<i64, String> {
        let mut v = self.assign()?;
        while self.op() == Some(",") {
            self.i += 1;
            v = self.assign()?;
        }
        Ok(v)
    }

    fn assign(&mut self) -> Result<i64, String> {
        if let (Some(T::Name(n)), Some(T::Op(o))) =
            (self.t.get(self.i).cloned(), self.t.get(self.i + 1).cloned())
            && [
                "=", "+=", "-=", "*=", "/=", "%=", "<<=", ">>=", "&=", "|=", "^=",
            ]
            .contains(&o)
        {
            self.i += 2;
            let rhs = self.assign()?;
            let cur = if o == "=" { 0 } else { self.var(&n)? };
            let v = match o {
                "=" => rhs,
                "+=" => cur.wrapping_add(rhs),
                "-=" => cur.wrapping_sub(rhs),
                "*=" => cur.wrapping_mul(rhs),
                "/=" => div(cur, rhs)?,
                "%=" => rem(cur, rhs)?,
                "<<=" => cur.wrapping_shl(rhs as u32),
                ">>=" => cur.wrapping_shr(rhs as u32),
                "&=" => cur & rhs,
                "|=" => cur | rhs,
                _ => cur ^ rhs,
            };
            self.set(&n, v);
            return Ok(v);
        }
        self.ternary()
    }

    fn ternary(&mut self) -> Result<i64, String> {
        let c = self.binary(0)?;
        if self.op() == Some("?") {
            self.i += 1;
            let live = self.live;
            self.live = live && c != 0;
            let a = self.assign()?;
            if self.op() != Some(":") {
                return Err("`:' expected for conditional expression".into());
            }
            self.i += 1;
            self.live = live && c == 0;
            let b = self.assign()?;
            self.live = live;
            return Ok(if c != 0 { a } else { b });
        }
        Ok(c)
    }

    fn binary(&mut self, min: u8) -> Result<i64, String> {
        let mut lhs = self.unary()?;
        while let Some(o) = self.op() {
            let prec = match o {
                "||" => 1,
                "&&" => 2,
                "|" => 3,
                "^" => 4,
                "&" => 5,
                "==" | "!=" => 6,
                "<" | ">" | "<=" | ">=" => 7,
                "<<" | ">>" => 8,
                "+" | "-" => 9,
                "*" | "/" | "%" => 10,
                "**" => 11,
                _ => break,
            };
            if prec < min {
                break;
            }
            self.i += 1;
            let live = self.live;
            if o == "&&" {
                self.live = live && lhs != 0;
            } else if o == "||" {
                self.live = live && lhs == 0;
            }
            // ** is right-associative
            let rhs = if o == "**" {
                self.binary(prec)?
            } else {
                self.binary(prec + 1)?
            };
            self.live = live;
            lhs = match o {
                "||" => i64::from(lhs != 0 || rhs != 0),
                "&&" => i64::from(lhs != 0 && rhs != 0),
                "|" => lhs | rhs,
                "^" => lhs ^ rhs,
                "&" => lhs & rhs,
                "==" => i64::from(lhs == rhs),
                "!=" => i64::from(lhs != rhs),
                "<" => i64::from(lhs < rhs),
                ">" => i64::from(lhs > rhs),
                "<=" => i64::from(lhs <= rhs),
                ">=" => i64::from(lhs >= rhs),
                "<<" => lhs.wrapping_shl(rhs as u32),
                ">>" => lhs.wrapping_shr(rhs as u32),
                "+" => lhs.wrapping_add(rhs),
                "-" => lhs.wrapping_sub(rhs),
                "*" => lhs.wrapping_mul(rhs),
                "/" => {
                    if self.live {
                        div(lhs, rhs)?
                    } else {
                        0
                    }
                }
                "%" => {
                    if self.live {
                        rem(lhs, rhs)?
                    } else {
                        0
                    }
                }
                _ => {
                    if rhs < 0 {
                        return Err("exponent less than 0".into());
                    }
                    lhs.wrapping_pow(rhs as u32)
                }
            };
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Result<i64, String> {
        match self.op() {
            Some("-") => {
                self.i += 1;
                Ok(self.unary()?.wrapping_neg())
            }
            Some("+") => {
                self.i += 1;
                self.unary()
            }
            Some("!") => {
                self.i += 1;
                Ok(i64::from(self.unary()? == 0))
            }
            Some("~") => {
                self.i += 1;
                Ok(!self.unary()?)
            }
            Some(o @ ("++" | "--")) => {
                self.i += 1;
                let Some(T::Name(n)) = self.peek().cloned() else {
                    return Err("syntax error: operand expected".into());
                };
                self.i += 1;
                let v = self.var(&n)? + if o == "++" { 1 } else { -1 };
                self.set(&n, v);
                Ok(v)
            }
            _ => self.postfix(),
        }
    }

    fn postfix(&mut self) -> Result<i64, String> {
        match self.peek().cloned() {
            Some(T::Num(n)) => {
                self.i += 1;
                Ok(n)
            }
            Some(T::Name(n)) => {
                self.i += 1;
                let v = self.var(&n)?;
                if let Some(o @ ("++" | "--")) = self.op() {
                    self.i += 1;
                    self.set(&n, v + if o == "++" { 1 } else { -1 });
                }
                Ok(v)
            }
            Some(T::LParen) => {
                self.i += 1;
                let v = self.comma()?;
                if self.peek() != Some(&T::RParen) {
                    return Err("missing `)'".into());
                }
                self.i += 1;
                Ok(v)
            }
            _ => Err("syntax error: operand expected".into()),
        }
    }
}

fn div(a: i64, b: i64) -> Result<i64, String> {
    if b == 0 {
        Err("division by 0".into())
    } else {
        Ok(a.wrapping_div(b))
    }
}

fn rem(a: i64, b: i64) -> Result<i64, String> {
    if b == 0 {
        Err("division by 0".into())
    } else {
        Ok(a.wrapping_rem(b))
    }
}

/// Evaluate an (already $-expanded) arithmetic expression.
pub fn eval_str(sh: &mut Shell, s: &str) -> Result<i64, String> {
    if s.trim().is_empty() {
        return Ok(0);
    }
    let t = lex(s)?;
    let mut p = P {
        t,
        i: 0,
        sh,
        live: true,
    };
    let v = p.comma()?;
    if p.i < p.t.len() {
        let rest: Vec<String> = p.t[p.i..].iter().map(|t| format!("{t:?}")).collect();
        return Err(format!(
            "syntax error in expression (error token is \"{}\")",
            rest.join(" ")
        ));
    }
    Ok(v)
}
