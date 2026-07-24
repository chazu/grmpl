//! Recursive-descent parser for the surface grammar.
//!
//! ```text
//! program := decl*
//! decl    := "rel"  Ident "(" collist ")"
//!          | "view" Ident "(" identlist? ")" "{" atom* "yield" identlist "}"
//!          | "form" Ident "{" rule* "}"
//! collist := col ("," col)*
//! col     := Ident (":" Ident)?          // column name and optional type
//! atom    := Ident "(" arg ("," arg)* ")"
//! arg     := Ident | Str | Int
//! rule    := patom+ "->" Ident "(" identlist? ")"
//! patom   := Str | Ident
//! ```

use crate::ast::{Arg, Arm, Atom, ColDecl, Decl, FormRule, MatchOp, PAtom, SArg, Stmt};
use crate::lexer::{lex, Token};

pub fn parse(src: &str) -> Result<Vec<Decl>, String> {
    let tokens = lex(src)?;
    let mut p = Parser { tokens, pos: 0 };
    let mut decls = Vec::new();
    while p.peek().is_some() {
        decls.push(p.decl()?);
    }
    Ok(decls)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }
    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }
    fn expect(&mut self, want: &Token) -> Result<(), String> {
        match self.next() {
            Some(ref t) if t == want => Ok(()),
            other => Err(format!("expected {want:?}, found {other:?}")),
        }
    }
    fn ident(&mut self) -> Result<String, String> {
        match self.next() {
            Some(Token::Ident(s)) => Ok(s),
            other => Err(format!("expected identifier, found {other:?}")),
        }
    }
    fn is_ident(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Token::Ident(s)) if s == kw)
    }

    fn decl(&mut self) -> Result<Decl, String> {
        match self.peek() {
            Some(Token::Ident(k)) if k == "rel" => self.rel_decl(),
            Some(Token::Ident(k)) if k == "view" => self.view_decl(),
            Some(Token::Ident(k)) if k == "form" => self.form_decl(),
            Some(Token::Ident(k)) if k == "on" => self.on_decl(),
            other => Err(format!("expected a declaration (rel/view/form/on), found {other:?}")),
        }
    }

    fn identlist(&mut self) -> Result<Vec<String>, String> {
        let mut out = vec![self.ident()?];
        while matches!(self.peek(), Some(Token::Comma)) {
            self.next();
            out.push(self.ident()?);
        }
        Ok(out)
    }

    fn rel_decl(&mut self) -> Result<Decl, String> {
        self.next(); // rel
        let name = self.ident()?;
        self.expect(&Token::LParen)?;
        let cols = self.collist()?;
        self.expect(&Token::RParen)?;
        Ok(Decl::Rel { name, cols })
    }

    /// `collist := col ("," col)*` where `col := Ident (":" Ident)?`.
    fn collist(&mut self) -> Result<Vec<ColDecl>, String> {
        let mut out = vec![self.col_decl()?];
        while matches!(self.peek(), Some(Token::Comma)) {
            self.next();
            out.push(self.col_decl()?);
        }
        Ok(out)
    }

    fn col_decl(&mut self) -> Result<ColDecl, String> {
        let name = self.ident()?;
        let ty = if matches!(self.peek(), Some(Token::Colon)) {
            self.next(); // :
            Some(self.ident()?)
        } else {
            None
        };
        Ok(ColDecl { name, ty })
    }

    fn view_decl(&mut self) -> Result<Decl, String> {
        self.next(); // view
        let name = self.ident()?;
        self.expect(&Token::LParen)?;
        let params = if matches!(self.peek(), Some(Token::RParen)) {
            Vec::new()
        } else {
            self.identlist()?
        };
        self.expect(&Token::RParen)?;
        self.expect(&Token::LBrace)?;

        let mut atoms = Vec::new();
        while !self.is_ident("yield") {
            if matches!(self.peek(), Some(Token::RBrace)) | self.peek().is_none() {
                return Err("view body must end with a `yield`".into());
            }
            atoms.push(self.atom()?);
        }
        self.next(); // yield
        let yields = self.identlist()?;
        self.expect(&Token::RBrace)?;
        Ok(Decl::View { name, params, atoms, yields })
    }

    fn atom(&mut self) -> Result<Atom, String> {
        let rel = self.ident()?;
        self.expect(&Token::LParen)?;
        let mut args = vec![self.arg()?];
        while matches!(self.peek(), Some(Token::Comma)) {
            self.next();
            args.push(self.arg()?);
        }
        self.expect(&Token::RParen)?;
        Ok(Atom { rel, args })
    }

    fn arg(&mut self) -> Result<Arg, String> {
        match self.next() {
            Some(Token::Ident(s)) => Ok(Arg::Var(s)),
            Some(Token::Str(s)) => Ok(Arg::Str(s)),
            Some(Token::Int(n)) => Ok(Arg::Int(n)),
            other => Err(format!("expected an argument, found {other:?}")),
        }
    }

    fn form_decl(&mut self) -> Result<Decl, String> {
        self.next(); // form
        let name = self.ident()?;
        self.expect(&Token::LBrace)?;
        let mut rules = Vec::new();
        while !matches!(self.peek(), Some(Token::RBrace)) {
            if self.peek().is_none() {
                return Err("unterminated form body".into());
            }
            rules.push(self.rule()?);
        }
        self.next(); // }
        Ok(Decl::Form { name, rules })
    }

    fn rule(&mut self) -> Result<FormRule, String> {
        let mut seq = Vec::new();
        while !matches!(self.peek(), Some(Token::Arrow)) {
            match self.next() {
                Some(Token::Str(s)) => seq.push(PAtom::Lit(s)),
                Some(Token::Ident(s)) => seq.push(PAtom::Bind(s)),
                other => return Err(format!("expected a pattern atom or `->`, found {other:?}")),
            }
        }
        if seq.is_empty() {
            return Err("form rule has an empty pattern".into());
        }
        self.expect(&Token::Arrow)?;
        let tag = self.ident()?;
        self.expect(&Token::LParen)?;
        let ctor_args = if matches!(self.peek(), Some(Token::RParen)) {
            Vec::new()
        } else {
            self.identlist()?
        };
        self.expect(&Token::RParen)?;
        Ok(FormRule { seq, tag, ctor_args })
    }

    fn on_decl(&mut self) -> Result<Decl, String> {
        self.next(); // on
        let inbox = self.ident()?;
        match self.ident()?.as_str() {
            "parse" => {}
            other => return Err(format!("expected `parse` in on-handler, found `{other}`")),
        }
        let form = self.ident()?;
        self.expect(&Token::LBrace)?;
        let mut arms = Vec::new();
        while !matches!(self.peek(), Some(Token::RBrace)) {
            if self.peek().is_none() {
                return Err("unterminated on-handler".into());
            }
            arms.push(self.arm()?);
        }
        self.next(); // }
        Ok(Decl::On { inbox, form, arms })
    }

    fn arm(&mut self) -> Result<Arm, String> {
        match self.ident()?.as_str() {
            "match" => {}
            other => return Err(format!("expected `match`, found `{other}`")),
        }
        let tag = self.ident()?;
        self.expect(&Token::LParen)?;
        let vars = if matches!(self.peek(), Some(Token::RParen)) {
            Vec::new()
        } else {
            self.identlist()?
        };
        self.expect(&Token::RParen)?;
        self.expect(&Token::LBrace)?;
        let mut stmts = Vec::new();
        while !matches!(self.peek(), Some(Token::RBrace)) {
            if self.peek().is_none() {
                return Err("unterminated match arm".into());
            }
            stmts.push(self.stmt()?);
        }
        self.next(); // }
        Ok(Arm { tag, vars, stmts })
    }

    fn stmt(&mut self) -> Result<Stmt, String> {
        let kw = self.ident()?;
        match kw.as_str() {
            "resolve" => {
                let view = self.ident()?;
                let args = self.paren_sargs()?;
                match self.ident()?.as_str() {
                    "where" => {}
                    other => return Err(format!("expected `where`, found `{other}`")),
                }
                let col = self.ident()?;
                let op = match self.next() {
                    Some(Token::Eq) => MatchOp::Exact,
                    Some(Token::Tilde) => MatchOp::Word,
                    other => return Err(format!("expected `=` or `~`, found {other:?}")),
                };
                let rhs = self.sarg()?;
                Ok(Stmt::Resolve { view, args, col, op, rhs })
            }
            "find" => Ok(Stmt::Find { rel: self.ident()?, args: self.paren_sargs()? }),
            "expect" => Ok(Stmt::Expect { rel: self.ident()?, args: self.paren_sargs()? }),
            "assert" => Ok(Stmt::Assert { rel: self.ident()?, args: self.paren_sargs()? }),
            "retract" => Ok(Stmt::Retract { rel: self.ident()?, args: self.paren_sargs()? }),
            "emit" => Ok(Stmt::Emit { rel: self.ident()?, args: self.paren_sargs()? }),
            other => Err(format!("unknown statement `{other}`")),
        }
    }

    fn paren_sargs(&mut self) -> Result<Vec<SArg>, String> {
        self.expect(&Token::LParen)?;
        if matches!(self.peek(), Some(Token::RParen)) {
            self.next();
            return Ok(Vec::new());
        }
        let mut out = vec![self.sarg()?];
        while matches!(self.peek(), Some(Token::Comma)) {
            self.next();
            out.push(self.sarg()?);
        }
        self.expect(&Token::RParen)?;
        Ok(out)
    }

    fn sarg(&mut self) -> Result<SArg, String> {
        match self.next() {
            Some(Token::Ident(s)) => Ok(SArg::Var(s)),
            Some(Token::Str(s)) => Ok(SArg::Str(s)),
            Some(Token::Int(n)) => Ok(SArg::Int(n)),
            other => Err(format!("expected an argument, found {other:?}")),
        }
    }
}
