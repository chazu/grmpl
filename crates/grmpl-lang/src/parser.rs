//! Recursive-descent parser for the surface grammar.
//!
//! ```text
//! program := decl*
//! decl    := "rel"  Ident "(" collist ")"
//!          | "view" Ident "(" identlist? ")" "{" atom* "yield" yieldlist "}"
//!          | "form" Ident "{" rule* "}"
//! collist := col ("," col)*
//! col     := Ident (":" Ident)?          // column name and optional type
//! atom    := Ident "(" arg ("," arg)* ")"
//! arg     := Ident | Str | Int
//! yieldlist := yielditem ("," yielditem)*
//! yielditem := Ident                     // a grouping / projection column
//!            | Ident "(" Ident? ")"      // an aggregate: sum(col) / count()
//!                                         // (at most one aggregate per view)
//! rule    := patom+ "->" Ident "(" identlist? ")"
//! patom   := Str | Ident
//! ```

use grmpl_core::Value;

use crate::ast::{
    AggFunc, AggYield, Arg, Arm, Atom, ColDecl, Decl, FormRule, MatchOp, PAtom, SArg, Stmt,
};
use crate::concat::{ConcatArm, Word};
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
        let (yields, agg) = self.yield_clause()?;
        self.expect(&Token::RBrace)?;
        Ok(Decl::View { name, params, atoms, yields, agg })
    }

    /// `yieldlist := yielditem ("," yielditem)*`, splitting the items into the
    /// plain grouping columns and the (at most one) aggregate. A `yielditem`
    /// spelled `Ident "(" Ident? ")"` is an aggregate call; a bare `Ident` is a
    /// grouping column. A second aggregate, an unknown aggregate name, or a
    /// wrong aggregate arity (`count` takes no column; `sum`/`min`/`max` take
    /// one) is a parse error.
    fn yield_clause(&mut self) -> Result<(Vec<String>, Option<AggYield>), String> {
        let mut yields = Vec::new();
        let mut agg: Option<AggYield> = None;
        loop {
            let name = self.ident()?;
            if matches!(self.peek(), Some(Token::LParen)) {
                self.next(); // (
                let col = if matches!(self.peek(), Some(Token::RParen)) {
                    None
                } else {
                    Some(self.ident()?)
                };
                self.expect(&Token::RParen)?;
                let func = match name.as_str() {
                    "count" => AggFunc::Count,
                    "sum" => AggFunc::Sum,
                    "min" => AggFunc::Min,
                    "max" => AggFunc::Max,
                    other => {
                        return Err(format!(
                            "unknown aggregate `{other}` (expected count, sum, min, or max)"
                        ))
                    }
                };
                match (func, &col) {
                    (AggFunc::Count, Some(c)) => {
                        return Err(format!("aggregate `count` takes no column, found `{c}`"))
                    }
                    (AggFunc::Sum | AggFunc::Min | AggFunc::Max, None) => {
                        return Err(format!("aggregate `{name}` needs a column"))
                    }
                    _ => {}
                }
                if agg.is_some() {
                    return Err("a view `yield` may contain at most one aggregate".into());
                }
                agg = Some(AggYield { func, col });
            } else {
                yields.push(name);
            }
            if matches!(self.peek(), Some(Token::Comma)) {
                self.next();
            } else {
                break;
            }
        }
        Ok((yields, agg))
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
        let mut stmt_arms = Vec::new();
        let mut word_arms = Vec::new();
        while !matches!(self.peek(), Some(Token::RBrace)) {
            if self.peek().is_none() {
                return Err("unterminated on-handler".into());
            }
            // Each arm is `match Tag(vars)` followed by either a `{ stmt* }`
            // statement body (v1) or a `[ word* ]` concatenative body (P11);
            // the two surfaces coexist in one handler.
            let (tag, vars) = self.arm_header()?;
            match self.peek() {
                Some(Token::LBrace) => stmt_arms.push(self.stmt_arm(tag, vars)?),
                Some(Token::LBracket) => word_arms.push(self.word_arm(tag, vars)?),
                other => {
                    return Err(format!(
                        "expected `{{` (statement arm) or `[` (concatenative arm), found {other:?}"
                    ))
                }
            }
        }
        self.next(); // }
        Ok(Decl::On {
            inbox,
            form,
            stmt_arms,
            word_arms,
        })
    }

    /// `match Tag ( identlist? )` — the shared head of both arm surfaces.
    fn arm_header(&mut self) -> Result<(String, Vec<String>), String> {
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
        Ok((tag, vars))
    }

    fn stmt_arm(&mut self, tag: String, vars: Vec<String>) -> Result<Arm, String> {
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

    /// `[ word* ]` — a point-free concatenative arm body.
    fn word_arm(&mut self, tag: String, vars: Vec<String>) -> Result<ConcatArm, String> {
        self.expect(&Token::LBracket)?;
        let mut words = Vec::new();
        while !matches!(self.peek(), Some(Token::RBracket)) {
            if self.peek().is_none() {
                return Err("unterminated concatenative arm".into());
            }
            words.push(self.word()?);
        }
        self.next(); // ]
        Ok(ConcatArm { tag, vars, words })
    }

    /// Parse one concatenative [`Word`]. Keyword words (`self`, the shufflers,
    /// and the effect seam) are recognized by name; a bare string/int is a
    /// literal push. The seam words consume a fixed number of *immediate*
    /// operands from the token stream (a view/relation name, a column, a match
    /// op, a key count) — their stack operands come at runtime, not here.
    fn word(&mut self) -> Result<Word, String> {
        match self.next() {
            Some(Token::Str(s)) => Ok(Word::Lit(Value::text(&s))),
            Some(Token::Int(n)) => Ok(Word::Lit(Value::Int(n))),
            Some(Token::Ident(kw)) => match kw.as_str() {
                "self" => Ok(Word::SelfEntity),
                "dup" => Ok(Word::Dup),
                "drop" => Ok(Word::Drop),
                "swap" => Ok(Word::Swap),
                "over" => Ok(Word::Over),
                "rot" => Ok(Word::Rot),
                "nip" => Ok(Word::Nip),
                "tuck" => Ok(Word::Tuck),
                "dup2" => Ok(Word::TwoDup),
                "drop2" => Ok(Word::TwoDrop),
                "resolve" => {
                    let view = self.ident()?;
                    let col = self.ident()?;
                    let op = match self.next() {
                        Some(Token::Eq) => MatchOp::Exact,
                        Some(Token::Tilde) => MatchOp::Word,
                        other => return Err(format!("expected `=` or `~`, found {other:?}")),
                    };
                    Ok(Word::Resolve { view, col, op })
                }
                "find" => {
                    let rel = self.ident()?;
                    let keyn = match self.next() {
                        Some(Token::Int(n)) if n >= 0 => n as usize,
                        other => {
                            return Err(format!("`find` needs a key count, found {other:?}"))
                        }
                    };
                    Ok(Word::Find { rel, keyn })
                }
                "expect" => Ok(Word::Expect(self.ident()?)),
                "assert" => Ok(Word::Assert(self.ident()?)),
                "retract" => Ok(Word::Retract(self.ident()?)),
                "emit" => Ok(Word::Emit(self.ident()?)),
                other => Err(format!("unknown word `{other}`")),
            },
            other => Err(format!("expected a word, found {other:?}")),
        }
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
