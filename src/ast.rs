//! The syntax tree. Words stay as raw source text; they're expanded at run
//! time (that's how quoting, $vars and globs work in a POSIX shell).

#[derive(Debug, Clone)]
pub struct Redir {
    /// fd being redirected (default 0 for <, 1 for >)
    pub fd: i32,
    pub kind: RedirKind,
    /// target word (raw), or heredoc body
    pub target: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RedirKind {
    In,                       // <
    Out,                      // >
    Append,                   // >>
    InOut,                    // <>
    DupOut,                   // >&N  / N>&M
    DupIn,                    // <&N
    Both,                     // &>  (stdout+stderr)
    BothAppend,               // &>>
    HereDoc { expand: bool }, // << / <<-
    HereString,               // <<<
}

#[derive(Debug, Clone)]
pub enum Command {
    Simple {
        assigns: Vec<(String, String)>,
        words: Vec<String>,
        redirs: Vec<Redir>,
    },
    If {
        branches: Vec<(List, List)>, // (condition, body)
        otherwise: Option<List>,
        redirs: Vec<Redir>,
    },
    While {
        until: bool,
        cond: List,
        body: List,
        redirs: Vec<Redir>,
    },
    For {
        var: String,
        items: Option<Vec<String>>, // None = "$@"
        body: List,
        redirs: Vec<Redir>,
    },
    Case {
        word: String,
        arms: Vec<(Vec<String>, List)>,
        redirs: Vec<Redir>,
    },
    Group(List, Vec<Redir>),
    Subshell(List, Vec<Redir>),
    Function {
        name: String,
        body: Box<Command>,
    },
    Arith(String, Vec<Redir>),
}

#[derive(Debug, Clone)]
pub struct Pipeline {
    pub negate: bool,
    pub cmds: Vec<Command>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Connector {
    And,
    Or,
}

#[derive(Debug, Clone)]
pub struct AndOr {
    pub first: Pipeline,
    pub rest: Vec<(Connector, Pipeline)>,
    pub background: bool,
}

pub type List = Vec<AndOr>;
