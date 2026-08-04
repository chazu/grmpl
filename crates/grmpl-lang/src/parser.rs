//! Recursive-descent parser for the surface grammar.
//!
//! ```text
//! program := decl*
//! decl    := "rel"  Ident "(" collist ")"
//!          | "view" Ident "(" identlist? ")" "{" atom* "yield" yieldlist "}"
//!          | "form" Ident "{" rule* "}"
//!          | "on" "watch" Ident ("including" "current")? "{" watchbind* "}"
//! watchbind := ("inbox" | "cursor" | "seqs") Ident
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
    AggFunc, AggYield, Arg, Arm, Atom, BinaryOp, BootstrapFact, BootstrapValue, ColDecl, Decl,
    Expr, FormRule, MatchOp, PAtom, SArg, Stmt, UnaryOp,
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
            Some(Token::Ident(k)) if k == "package" => self.package_decl(),
            Some(Token::Ident(k)) if k == "entity" => self.entity_decl(),
            Some(Token::Ident(k)) if k == "requires" => self.requires_decl(),
            Some(Token::Ident(k)) if k == "authority" => self.authority_decl(),
            Some(Token::Ident(k)) if k == "actor" => self.actor_decl(),
            Some(Token::Ident(k)) if k == "bootstrap" => self.bootstrap_decl(),
            Some(Token::Ident(k)) if k == "rel" => self.rel_decl(),
            Some(Token::Ident(k)) if k == "view" => self.view_decl(),
            Some(Token::Ident(k)) if k == "form" => self.form_decl(),
            Some(Token::Ident(k)) if k == "on" => self.on_decl(),
            other => Err(format!(
                "expected a declaration (package/entity/requires/authority/actor/bootstrap/rel/view/form/on), \
                 found {other:?}"
            )),
        }
    }

    fn keyword(&mut self, expected: &str) -> Result<(), String> {
        match self.ident()?.as_str() {
            actual if actual == expected => Ok(()),
            actual => Err(format!("expected `{expected}`, found `{actual}`")),
        }
    }

    fn package_decl(&mut self) -> Result<Decl, String> {
        self.next(); // package
        let id = self.ident()?;
        self.keyword("bootstrap")?;
        let version = match self.next() {
            Some(Token::Int(n)) if (0..=u32::MAX as i64).contains(&n) => n as u32,
            other => {
                return Err(format!(
                    "package bootstrap version must be a u32, found {other:?}"
                ))
            }
        };
        Ok(Decl::Package {
            id,
            bootstrap_version: version,
        })
    }

    fn signed_int(&mut self, what: &str) -> Result<i64, String> {
        match self.next() {
            Some(Token::Int(n)) => Ok(n),
            Some(Token::Minus) => match self.next() {
                Some(Token::Int(n)) => n
                    .checked_neg()
                    .ok_or_else(|| format!("{what} is below i64::MIN")),
                other => Err(format!(
                    "expected integer after `-` for {what}, found {other:?}"
                )),
            },
            other => Err(format!("expected integer for {what}, found {other:?}")),
        }
    }

    fn entity_decl(&mut self) -> Result<Decl, String> {
        self.next(); // entity
        let name = self.ident()?;
        self.expect(&Token::Eq)?;
        let id = self.signed_int("entity id")?;
        Ok(Decl::Entity { name, id })
    }

    fn requires_decl(&mut self) -> Result<Decl, String> {
        self.next(); // requires
        let kind = self.ident()?;
        let name = self.ident()?;
        self.expect(&Token::LParen)?;
        let decl = match kind.as_str() {
            "allocate" => {
                self.keyword("counter")?;
                self.expect(&Token::Colon)?;
                let counter = self.ident()?;
                self.expect(&Token::Comma)?;
                self.keyword("first")?;
                self.expect(&Token::Colon)?;
                let first = self.signed_int("allocation first")?;
                self.expect(&Token::Comma)?;
                self.keyword("last")?;
                self.expect(&Token::Colon)?;
                let last = self.signed_int("allocation last")?;
                Decl::RequireAllocate {
                    name,
                    counter,
                    first,
                    last,
                }
            }
            "random" => {
                self.keyword("state")?;
                self.expect(&Token::Colon)?;
                let state = self.ident()?;
                self.expect(&Token::Comma)?;
                self.keyword("owner")?;
                self.expect(&Token::Colon)?;
                let owner = self.ident()?;
                self.expect(&Token::Comma)?;
                self.keyword("algorithm")?;
                self.expect(&Token::Colon)?;
                let algorithm = self.ident()?;
                Decl::RequireRandom {
                    name,
                    state,
                    owner,
                    algorithm,
                }
            }
            "schedule" => {
                self.keyword("clock")?;
                self.expect(&Token::Colon)?;
                let clock = self.ident()?;
                self.expect(&Token::Comma)?;
                self.keyword("timers")?;
                self.expect(&Token::Colon)?;
                let timers = self.ident()?;
                self.expect(&Token::Comma)?;
                self.keyword("sequences")?;
                self.expect(&Token::Colon)?;
                let sequences = self.ident()?;
                Decl::RequireSchedule {
                    name,
                    clock,
                    timers,
                    sequences,
                }
            }
            other => {
                return Err(format!(
                "unknown capability requirement `{other}` (expected allocate, random, or schedule)"
            ))
            }
        };
        self.expect(&Token::RParen)?;
        Ok(decl)
    }

    fn authority_decl(&mut self) -> Result<Decl, String> {
        self.next(); // authority
        let name = self.ident()?;
        self.expect(&Token::LBrace)?;
        let mut writes = Vec::new();
        while !matches!(self.peek(), Some(Token::RBrace)) {
            if self.peek().is_none() {
                return Err("unterminated authority block".into());
            }
            self.keyword("write")?;
            writes.push(self.ident()?);
        }
        self.next();
        Ok(Decl::Authority { name, writes })
    }

    fn actor_decl(&mut self) -> Result<Decl, String> {
        self.next(); // actor
        let entity = self.ident()?;
        self.expect(&Token::LBrace)?;
        let mut inbox = None;
        let mut cursor = None;
        let mut authority = None;
        while !matches!(self.peek(), Some(Token::RBrace)) {
            if self.peek().is_none() {
                return Err("unterminated actor block".into());
            }
            let field = self.ident()?;
            let value = self.ident()?;
            let slot = match field.as_str() {
                "inbox" => &mut inbox,
                "cursor" => &mut cursor,
                "authority" => &mut authority,
                _ => return Err(format!("unknown actor field `{field}`")),
            };
            if slot.replace(value).is_some() {
                return Err(format!("actor field `{field}` declared twice"));
            }
        }
        self.next();
        Ok(Decl::Actor {
            entity,
            inbox: inbox.ok_or_else(|| "actor needs `inbox`".to_string())?,
            cursor: cursor.ok_or_else(|| "actor needs `cursor`".to_string())?,
            authority: authority.ok_or_else(|| "actor needs `authority`".to_string())?,
        })
    }

    fn bootstrap_decl(&mut self) -> Result<Decl, String> {
        self.next(); // bootstrap
        self.expect(&Token::LBrace)?;
        let mut facts = Vec::new();
        while !matches!(self.peek(), Some(Token::RBrace)) {
            if self.peek().is_none() {
                return Err("unterminated bootstrap block".into());
            }
            let rel = self.ident()?;
            self.expect(&Token::LParen)?;
            let values = if matches!(self.peek(), Some(Token::RParen)) {
                Vec::new()
            } else {
                self.bootstrap_values()?
            };
            self.expect(&Token::RParen)?;
            facts.push(BootstrapFact { rel, values });
        }
        self.next(); // }
        Ok(Decl::Bootstrap { facts })
    }

    fn bootstrap_values(&mut self) -> Result<Vec<BootstrapValue>, String> {
        let mut values = vec![self.bootstrap_value()?];
        while matches!(self.peek(), Some(Token::Comma)) {
            self.next();
            values.push(self.bootstrap_value()?);
        }
        Ok(values)
    }

    fn bootstrap_value(&mut self) -> Result<BootstrapValue, String> {
        match self.next() {
            Some(Token::Ident(name)) if name == "true" => Ok(BootstrapValue::Bool(true)),
            Some(Token::Ident(name)) if name == "false" => Ok(BootstrapValue::Bool(false)),
            Some(Token::Ident(name)) => Ok(BootstrapValue::Entity(name)),
            Some(Token::Str(text)) => Ok(BootstrapValue::Text(text)),
            Some(Token::Int(value)) => Ok(BootstrapValue::Int(value)),
            Some(Token::Float(value)) => Ok(BootstrapValue::Float(value)),
            Some(Token::Minus) => match self.next() {
                Some(Token::Int(value)) => value
                    .checked_neg()
                    .map(BootstrapValue::Int)
                    .ok_or_else(|| "bootstrap integer is below i64::MIN".into()),
                Some(Token::Float(value)) => Ok(BootstrapValue::Float(
                    grmpl_core::FiniteF64::new(-value.get()).expect("finite negation stays finite"),
                )),
                other => Err(format!("expected a number after `-`, found {other:?}")),
            },
            Some(Token::LParen) => {
                let values = if matches!(self.peek(), Some(Token::RParen)) {
                    Vec::new()
                } else {
                    self.bootstrap_values()?
                };
                self.expect(&Token::RParen)?;
                Ok(BootstrapValue::Tuple(values))
            }
            other => Err(format!("expected bootstrap literal, found {other:?}")),
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
        Ok(Decl::View {
            name,
            params,
            atoms,
            yields,
            agg,
        })
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
            Some(Token::Ident(s)) if s == "true" => Ok(Arg::Bool(true)),
            Some(Token::Ident(s)) if s == "false" => Ok(Arg::Bool(false)),
            Some(Token::Ident(s)) => Ok(Arg::Var(s)),
            Some(Token::Str(s)) => Ok(Arg::Str(s)),
            Some(Token::Int(n)) => Ok(Arg::Int(n)),
            Some(Token::Float(n)) => Ok(Arg::Float(n)),
            Some(Token::Minus) => match self.next() {
                Some(Token::Int(n)) => n
                    .checked_neg()
                    .map(Arg::Int)
                    .ok_or_else(|| "integer literal is below i64::MIN".into()),
                Some(Token::Float(n)) => Ok(Arg::Float(
                    grmpl_core::FiniteF64::new(-n.get()).expect("finite negation stays finite"),
                )),
                other => Err(format!("expected a number after `-`, found {other:?}")),
            },
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
        Ok(FormRule {
            seq,
            tag,
            ctor_args,
        })
    }

    fn on_decl(&mut self) -> Result<Decl, String> {
        self.next(); // on
                     // `on watch <view> { … }` — the reactive-handler surface — shares the
                     // `on` keyword with the message-handler `on <inbox> parse <form> { … }`,
                     // disambiguated by the `watch` keyword immediately after `on`.
        if self.is_ident("watch") {
            return self.on_watch_decl();
        }
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

    /// `on watch <view> ("including" "current")? "{" ("inbox"|"cursor"|"seqs")
    /// Ident … "}"` — a reactive handler over a maintained view. `on` and the
    /// `watch` keyword are already consumed. Each of the three relation bindings
    /// must appear exactly once; order is free.
    fn on_watch_decl(&mut self) -> Result<Decl, String> {
        self.next(); // watch
        let view = self.ident()?;
        let including_current = if self.is_ident("including") {
            self.next(); // including
            match self.ident()?.as_str() {
                "current" => {}
                other => {
                    return Err(format!(
                        "expected `current` after `including`, found `{other}`"
                    ))
                }
            }
            true
        } else {
            false
        };
        self.expect(&Token::LBrace)?;
        let mut inbox: Option<String> = None;
        let mut cursor: Option<String> = None;
        let mut seqs: Option<String> = None;
        let set = |slot: &mut Option<String>, rel: String, key: &str| -> Result<(), String> {
            if slot.is_some() {
                return Err(format!("on-watch binding `{key}` set twice"));
            }
            *slot = Some(rel);
            Ok(())
        };
        while !matches!(self.peek(), Some(Token::RBrace)) {
            if self.peek().is_none() {
                return Err("unterminated on-watch body".into());
            }
            let key = self.ident()?;
            let rel = self.ident()?;
            match key.as_str() {
                "inbox" => set(&mut inbox, rel, "inbox")?,
                "cursor" => set(&mut cursor, rel, "cursor")?,
                "seqs" => set(&mut seqs, rel, "seqs")?,
                other => {
                    return Err(format!(
                        "unknown on-watch binding `{other}` (expected inbox, cursor, or seqs)"
                    ))
                }
            }
        }
        self.next(); // }
        let inbox = inbox.ok_or("on-watch missing `inbox` binding")?;
        let cursor = cursor.ok_or("on-watch missing `cursor` binding")?;
        let seqs = seqs.ok_or("on-watch missing `seqs` binding")?;
        Ok(Decl::OnWatch {
            view,
            inbox,
            cursor,
            seqs,
            including_current,
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
            Some(Token::Float(n)) => Ok(Word::Lit(Value::Float(n))),
            Some(Token::Ident(ref kw)) if kw == "true" => Ok(Word::Lit(Value::Bool(true))),
            Some(Token::Ident(ref kw)) if kw == "false" => Ok(Word::Lit(Value::Bool(false))),
            Some(Token::Minus) => match self.next() {
                Some(Token::Int(n)) => n
                    .checked_neg()
                    .map(|n| Word::Lit(Value::Int(n)))
                    .ok_or_else(|| "integer literal is below i64::MIN".into()),
                Some(Token::Float(n)) => Ok(Word::Lit(Value::Float(
                    grmpl_core::FiniteF64::new(-n.get()).expect("finite negation stays finite"),
                ))),
                other => Err(format!("expected a number after `-`, found {other:?}")),
            },
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
                "add" => Ok(Word::Add),
                "sub" => Ok(Word::Sub),
                "mul" => Ok(Word::Mul),
                "div" => Ok(Word::Div),
                "rem" => Ok(Word::Rem),
                "neg" => Ok(Word::Neg),
                "min" => Ok(Word::Min),
                "max" => Ok(Word::Max),
                "to_float" => Ok(Word::ToFloat),
                "eq" => Ok(Word::Eq),
                "ne" => Ok(Word::Ne),
                "lt" => Ok(Word::Lt),
                "le" => Ok(Word::Le),
                "gt" => Ok(Word::Gt),
                "ge" => Ok(Word::Ge),
                "not" => Ok(Word::Not),
                "and" => Ok(Word::And),
                "or" => Ok(Word::Or),
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
                        other => return Err(format!("`find` needs a key count, found {other:?}")),
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
            "let" => {
                let name = self.ident()?;
                self.expect(&Token::Eq)?;
                Ok(Stmt::Let {
                    name,
                    value: self.expr()?,
                })
            }
            "if" => {
                let condition = self.expr()?;
                let then_stmts = self.stmt_block()?;
                let else_stmts = if self.is_ident("else") {
                    self.next();
                    self.stmt_block()?
                } else {
                    Vec::new()
                };
                Ok(Stmt::If {
                    condition,
                    then_stmts,
                    else_stmts,
                })
            }
            "fresh" => {
                let capability = self.ident()?;
                match self.ident()?.as_str() {
                    "as" => {}
                    other => {
                        return Err(format!(
                            "expected `as` after fresh capability, found `{other}`"
                        ))
                    }
                }
                Ok(Stmt::Fresh {
                    capability,
                    local: self.ident()?,
                })
            }
            "random" => {
                let capability = self.ident()?;
                match self.ident()?.as_str() {
                    "below" => {}
                    other => {
                        return Err(format!(
                            "expected `below` after random capability, found `{other}`"
                        ))
                    }
                }
                let bound = self.expr()?;
                match self.ident()?.as_str() {
                    "as" => {}
                    other => {
                        return Err(format!("expected `as` after random bound, found `{other}`"))
                    }
                }
                Ok(Stmt::Random {
                    capability,
                    bound,
                    local: self.ident()?,
                })
            }
            "schedule" => {
                let capability = self.ident()?;
                self.keyword("at")?;
                let due = self.expr()?;
                self.keyword("send")?;
                let tag = self.ident()?;
                let arguments = self.paren_exprs()?;
                self.keyword("to")?;
                Ok(Stmt::Schedule {
                    capability,
                    due,
                    tag,
                    arguments,
                    target: self.ident()?,
                })
            }
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
                Ok(Stmt::Resolve {
                    view,
                    args,
                    col,
                    op,
                    rhs,
                })
            }
            "find" => Ok(Stmt::Find {
                rel: self.ident()?,
                args: self.paren_sargs()?,
            }),
            "expect" => Ok(Stmt::Expect {
                rel: self.ident()?,
                args: self.paren_sargs()?,
            }),
            "assert" => Ok(Stmt::Assert {
                rel: self.ident()?,
                args: self.paren_sargs()?,
            }),
            "retract" => Ok(Stmt::Retract {
                rel: self.ident()?,
                args: self.paren_sargs()?,
            }),
            "emit" => Ok(Stmt::Emit {
                rel: self.ident()?,
                args: self.paren_sargs()?,
            }),
            other => Err(format!("unknown statement `{other}`")),
        }
    }

    fn stmt_block(&mut self) -> Result<Vec<Stmt>, String> {
        self.expect(&Token::LBrace)?;
        let mut statements = Vec::new();
        while !matches!(self.peek(), Some(Token::RBrace)) {
            if self.peek().is_none() {
                return Err("unterminated statement block".into());
            }
            statements.push(self.stmt()?);
        }
        self.next();
        Ok(statements)
    }

    fn expr(&mut self) -> Result<Expr, String> {
        self.expr_or()
    }

    fn expr_or(&mut self) -> Result<Expr, String> {
        let mut expression = self.expr_and()?;
        while matches!(self.peek(), Some(Token::OrOr)) {
            self.next();
            expression = Expr::Binary {
                op: BinaryOp::Or,
                left: Box::new(expression),
                right: Box::new(self.expr_and()?),
            };
        }
        Ok(expression)
    }

    fn expr_and(&mut self) -> Result<Expr, String> {
        let mut expression = self.expr_equality()?;
        while matches!(self.peek(), Some(Token::AndAnd)) {
            self.next();
            expression = Expr::Binary {
                op: BinaryOp::And,
                left: Box::new(expression),
                right: Box::new(self.expr_equality()?),
            };
        }
        Ok(expression)
    }

    fn expr_equality(&mut self) -> Result<Expr, String> {
        let mut expression = self.expr_comparison()?;
        loop {
            let op = match self.peek() {
                Some(Token::EqEq) => BinaryOp::Eq,
                Some(Token::Ne) => BinaryOp::Ne,
                _ => break,
            };
            self.next();
            expression = Expr::Binary {
                op,
                left: Box::new(expression),
                right: Box::new(self.expr_comparison()?),
            };
        }
        Ok(expression)
    }

    fn expr_comparison(&mut self) -> Result<Expr, String> {
        let mut expression = self.expr_additive()?;
        loop {
            let op = match self.peek() {
                Some(Token::Lt) => BinaryOp::Lt,
                Some(Token::Le) => BinaryOp::Le,
                Some(Token::Gt) => BinaryOp::Gt,
                Some(Token::Ge) => BinaryOp::Ge,
                _ => break,
            };
            self.next();
            expression = Expr::Binary {
                op,
                left: Box::new(expression),
                right: Box::new(self.expr_additive()?),
            };
        }
        Ok(expression)
    }

    fn expr_additive(&mut self) -> Result<Expr, String> {
        let mut expression = self.expr_multiplicative()?;
        loop {
            let op = match self.peek() {
                Some(Token::Plus) => BinaryOp::Add,
                Some(Token::Minus) => BinaryOp::Sub,
                _ => break,
            };
            self.next();
            expression = Expr::Binary {
                op,
                left: Box::new(expression),
                right: Box::new(self.expr_multiplicative()?),
            };
        }
        Ok(expression)
    }

    fn expr_multiplicative(&mut self) -> Result<Expr, String> {
        let mut expression = self.expr_unary()?;
        loop {
            let op = match self.peek() {
                Some(Token::Star) => BinaryOp::Mul,
                Some(Token::Slash) => BinaryOp::Div,
                Some(Token::Percent) => BinaryOp::Rem,
                _ => break,
            };
            self.next();
            expression = Expr::Binary {
                op,
                left: Box::new(expression),
                right: Box::new(self.expr_unary()?),
            };
        }
        Ok(expression)
    }

    fn expr_unary(&mut self) -> Result<Expr, String> {
        let op = match self.peek() {
            Some(Token::Minus) => Some(UnaryOp::Neg),
            Some(Token::Bang) => Some(UnaryOp::Not),
            _ => None,
        };
        if let Some(op) = op {
            self.next();
            return Ok(Expr::Unary {
                op,
                value: Box::new(self.expr_unary()?),
            });
        }
        self.expr_primary()
    }

    fn expr_primary(&mut self) -> Result<Expr, String> {
        match self.next() {
            Some(Token::Ident(name)) if name == "true" => Ok(Expr::Lit(Value::Bool(true))),
            Some(Token::Ident(name)) if name == "false" => Ok(Expr::Lit(Value::Bool(false))),
            Some(Token::Ident(name)) => {
                if matches!(self.peek(), Some(Token::LParen)) {
                    self.next();
                    let mut args = Vec::new();
                    if !matches!(self.peek(), Some(Token::RParen)) {
                        args.push(self.expr()?);
                        while matches!(self.peek(), Some(Token::Comma)) {
                            self.next();
                            args.push(self.expr()?);
                        }
                    }
                    self.expect(&Token::RParen)?;
                    Ok(Expr::Call { name, args })
                } else {
                    Ok(Expr::Var(name))
                }
            }
            Some(Token::Str(value)) => Ok(Expr::Lit(Value::text(value))),
            Some(Token::Int(value)) => Ok(Expr::Lit(Value::Int(value))),
            Some(Token::Float(value)) => Ok(Expr::Lit(Value::Float(value))),
            Some(Token::LParen) => {
                let expression = self.expr()?;
                self.expect(&Token::RParen)?;
                Ok(expression)
            }
            other => Err(format!("expected expression, found {other:?}")),
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

    fn paren_exprs(&mut self) -> Result<Vec<Expr>, String> {
        self.expect(&Token::LParen)?;
        if matches!(self.peek(), Some(Token::RParen)) {
            self.next();
            return Ok(Vec::new());
        }
        let mut out = vec![self.expr()?];
        while matches!(self.peek(), Some(Token::Comma)) {
            self.next();
            out.push(self.expr()?);
        }
        self.expect(&Token::RParen)?;
        Ok(out)
    }

    fn sarg(&mut self) -> Result<SArg, String> {
        match self.next() {
            Some(Token::Ident(s)) if s == "true" => Ok(SArg::Bool(true)),
            Some(Token::Ident(s)) if s == "false" => Ok(SArg::Bool(false)),
            Some(Token::Ident(s)) => Ok(SArg::Var(s)),
            Some(Token::Str(s)) => Ok(SArg::Str(s)),
            Some(Token::Int(n)) => Ok(SArg::Int(n)),
            Some(Token::Float(n)) => Ok(SArg::Float(n)),
            Some(Token::Minus) => match self.next() {
                Some(Token::Int(n)) => n
                    .checked_neg()
                    .map(SArg::Int)
                    .ok_or_else(|| "integer literal is below i64::MIN".into()),
                Some(Token::Float(n)) => Ok(SArg::Float(
                    grmpl_core::FiniteF64::new(-n.get()).expect("finite negation stays finite"),
                )),
                other => Err(format!("expected a number after `-`, found {other:?}")),
            },
            other => Err(format!("expected an argument, found {other:?}")),
        }
    }
}
