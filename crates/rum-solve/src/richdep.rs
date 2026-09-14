//! Parser for RPM rich/boolean dependencies.
//!
//! RPM encodes boolean deps as the *name* of a single `<rpm:entry>`, e.g.
//! `(mysql-selinux if selinux-policy-targeted)` or `(python3-foo or python3-bar)`.
//! This parses that mini-language into an AST; `sat.rs` maps it onto resolvo's
//! native conditional requirements and version-set unions.
//!
//! Grammar (whitespace-tokenized, grouping parens stuck to operands):
//!   expr    := operand (BOOLOP operand [else operand])*
//!   operand := '(' expr ')' | WORD [CMP WORD]
//!   BOOLOP  := and | or | if | unless | with | without
//!   CMP     := < | <= | = | >= | >
//! Capability names may contain balanced parens (`libc.so.6()(64bit)`), so only
//! *excess* leading/trailing parens are treated as grouping.

use crate::dep::DepFlag;
use crate::{Dep, Evr};

/// A parsed rich-dependency expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RichExpr {
    Term(Dep),
    And(Box<RichExpr>, Box<RichExpr>),
    Or(Box<RichExpr>, Box<RichExpr>),
    /// `(then if cond)`
    If(Box<RichExpr>, Box<RichExpr>),
    /// `(then if cond else els)`
    IfElse(Box<RichExpr>, Box<RichExpr>, Box<RichExpr>),
    /// `(body unless cond)`
    Unless(Box<RichExpr>, Box<RichExpr>),
    /// `(body unless cond else els)`
    UnlessElse(Box<RichExpr>, Box<RichExpr>, Box<RichExpr>),
    /// `(a with b)` — one package providing both (approximated by callers).
    With(Box<RichExpr>, Box<RichExpr>),
    /// `(a without b)` — approximated by callers.
    Without(Box<RichExpr>, Box<RichExpr>),
}

/// Does this dependency name look like a rich/boolean expression?
pub fn is_rich(name: &str) -> bool {
    name.starts_with('(')
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Open,
    Close,
    And,
    Or,
    If,
    Else,
    Unless,
    With,
    Without,
    Cmp(DepFlag),
    Word(String),
}

fn classify(term: &str) -> Tok {
    match term {
        "and" => Tok::And,
        "or" => Tok::Or,
        "if" => Tok::If,
        "else" => Tok::Else,
        "unless" => Tok::Unless,
        "with" => Tok::With,
        "without" => Tok::Without,
        "<" => Tok::Cmp(DepFlag::Lt),
        "<=" => Tok::Cmp(DepFlag::Le),
        "=" => Tok::Cmp(DepFlag::Eq),
        ">=" => Tok::Cmp(DepFlag::Ge),
        ">" => Tok::Cmp(DepFlag::Gt),
        _ => Tok::Word(term.to_string()),
    }
}

fn tokenize(s: &str) -> Vec<Tok> {
    let mut toks = Vec::new();
    for raw in s.split_whitespace() {
        let mut rest = raw;
        // Leading '(' are always grouping (capability names never start with '(').
        while let Some(r) = rest.strip_prefix('(') {
            toks.push(Tok::Open);
            rest = r;
        }
        // Trailing grouping ')' = the excess over the token's own balanced parens.
        let excess = rest
            .matches(')')
            .count()
            .saturating_sub(rest.matches('(').count());
        let term = &rest[..rest.len() - excess];
        if !term.is_empty() {
            toks.push(classify(term));
        }
        for _ in 0..excess {
            toks.push(Tok::Close);
        }
    }
    toks
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn operand(&mut self) -> Option<RichExpr> {
        match self.next()? {
            Tok::Open => {
                let e = self.expr()?;
                if !matches!(self.next(), Some(Tok::Close)) {
                    return None;
                }
                Some(e)
            }
            Tok::Word(name) => {
                if let Some(Tok::Cmp(flag)) = self.peek().cloned() {
                    self.pos += 1;
                    let ver = match self.next()? {
                        Tok::Word(v) => v,
                        _ => return None,
                    };
                    Some(RichExpr::Term(Dep {
                        name,
                        flag,
                        evr: Some(Evr::parse(&ver)),
                    }))
                } else {
                    Some(RichExpr::Term(Dep::unversioned(name)))
                }
            }
            _ => None,
        }
    }

    fn expr(&mut self) -> Option<RichExpr> {
        let mut left = self.operand()?;
        loop {
            match self.peek() {
                Some(Tok::And) => {
                    self.pos += 1;
                    left = RichExpr::And(Box::new(left), Box::new(self.operand()?));
                }
                Some(Tok::Or) => {
                    self.pos += 1;
                    left = RichExpr::Or(Box::new(left), Box::new(self.operand()?));
                }
                Some(Tok::If) => {
                    self.pos += 1;
                    let cond = self.operand()?;
                    if matches!(self.peek(), Some(Tok::Else)) {
                        self.pos += 1;
                        let els = self.operand()?;
                        left = RichExpr::IfElse(Box::new(left), Box::new(cond), Box::new(els));
                    } else {
                        left = RichExpr::If(Box::new(left), Box::new(cond));
                    }
                }
                Some(Tok::Unless) => {
                    self.pos += 1;
                    let cond = self.operand()?;
                    if matches!(self.peek(), Some(Tok::Else)) {
                        self.pos += 1;
                        let els = self.operand()?;
                        left = RichExpr::UnlessElse(Box::new(left), Box::new(cond), Box::new(els));
                    } else {
                        left = RichExpr::Unless(Box::new(left), Box::new(cond));
                    }
                }
                Some(Tok::With) => {
                    self.pos += 1;
                    left = RichExpr::With(Box::new(left), Box::new(self.operand()?));
                }
                Some(Tok::Without) => {
                    self.pos += 1;
                    left = RichExpr::Without(Box::new(left), Box::new(self.operand()?));
                }
                _ => break,
            }
        }
        Some(left)
    }
}

/// Parse a rich-dependency string into an AST. Returns `None` if it doesn't
/// parse (caller should then skip it rather than fail resolution).
pub fn parse_rich(s: &str) -> Option<RichExpr> {
    let mut p = Parser {
        toks: tokenize(s),
        pos: 0,
    };
    let e = p.expr()?;
    (p.pos == p.toks.len()).then_some(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(n: &str) -> RichExpr {
        RichExpr::Term(Dep::unversioned(n))
    }

    #[test]
    fn parses_if() {
        let e = parse_rich("(mysql-selinux if selinux-policy-targeted)").unwrap();
        assert_eq!(
            e,
            RichExpr::If(
                Box::new(term("mysql-selinux")),
                Box::new(term("selinux-policy-targeted"))
            )
        );
    }

    #[test]
    fn parses_or() {
        let e = parse_rich("(python3-foo or python3-bar)").unwrap();
        assert_eq!(
            e,
            RichExpr::Or(Box::new(term("python3-foo")), Box::new(term("python3-bar")))
        );
    }

    #[test]
    fn parses_versioned_and_nested() {
        // (foo >= 1.2 and (bar or baz))
        let e = parse_rich("(foo >= 1.2 and (bar or baz))").unwrap();
        match e {
            RichExpr::And(l, r) => {
                match *l {
                    RichExpr::Term(d) => {
                        assert_eq!(d.name, "foo");
                        assert_eq!(d.flag, DepFlag::Ge);
                        assert_eq!(d.evr.unwrap().version, "1.2");
                    }
                    _ => panic!("lhs not term"),
                }
                assert!(matches!(*r, RichExpr::Or(..)));
            }
            _ => panic!("not And"),
        }
    }

    #[test]
    fn parses_soname_with_internal_parens() {
        // grouping parens vs the soname's own parens
        let e = parse_rich("(libc.so.6()(64bit) or musl)").unwrap();
        match e {
            RichExpr::Or(l, _) => match *l {
                RichExpr::Term(d) => assert_eq!(d.name, "libc.so.6()(64bit)"),
                _ => panic!(),
            },
            _ => panic!("not Or"),
        }
    }

    #[test]
    fn if_else() {
        let e = parse_rich("(a if b else c)").unwrap();
        assert!(matches!(e, RichExpr::IfElse(..)));
    }

    #[test]
    fn garbage_returns_none() {
        assert!(parse_rich("(a and)").is_none());
    }
}
