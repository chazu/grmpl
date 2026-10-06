//! Recursive-descent parser for the surface grammar.
//!
//! ```text
//! program := decl*
//! decl    := "rel"  Ident "(" collist ")"
//!          | "materialized"? "view" Ident "(" identlist? ")" "{" atom* "yield" yieldlist "}"
//!          | "context" Ident                // a scope relation (first, last, key, value)
//!          | "form" Ident "{" rule* "}"
//!          | "on" "watch" Ident ("including" "current")? "{" watchbind* "}"
//! watchbind := ("inbox" | "cursor" | "seqs") Ident
//! collist := col ("," col)*
//! col     := Ident (":" Ident)?          // column name and optional type
//! atom    := "inherit"? Ident "(" arg ("," arg)* ")"
//! arg     := Ident | Str | Int
//! yieldlist := yielditem ("," yielditem)*
//! yielditem := Ident                     // a grouping / projection column
//!            | Ident "(" Ident? ")"      // an aggregate: sum(col) / count()
//!                                         // (at most one aggregate per view)
//! rule    := patom+ "->" Ident "(" identlist? ")"
//! patom   := Str | Ident
//! ```
//!
//! Every error is a [`Diagnostic`] at the offending token, or at the end of
//! input when the source stops short.

use grmpl_core::Value;

use crate::ast::{
    AggFunc, AggYield, Arg, Arm, Atom, BinaryOp, BootstrapFact, BootstrapValue, ColDecl, Decl,
    Expr, FormRule, MatchOp, PAtom, SArg, Stmt, UnaryOp,
};
use crate::concat::{ConcatArm, Word};
use crate::diagnostic::{Diagnostic, Pos, Spanned};
use crate::lexer::{lex, Token};

/// Parse a source into its top-level declarations, each with the position of
/// its first token.
pub fn parse(src: &str) -> Result<Vec<Spanned<Decl>>, Diagnostic> {
    let lexed = lex(src)?;
    let mut p = Parser {
        tokens: lexed.tokens,
        pos: 0,
        end: lexed.end,
    };
    let mut decls = Vec::new();
    while p.peek().is_some() {
        let pos = p.here();
        decls.push(Spanned {
            node: p.decl()?,
            pos,
        });
    }
    Ok(decls)
}

type PResult<T> = Result<T, Diagnostic>;

/// How a message names what was found: the token, or the end of input.
fn found(token: &Option<Token>) -> String {
    match token {
        Some(token) => token.to_string(),
        None => "end of input".into(),
    }
}

struct Parser {
    tokens: Vec<Spanned<Token>>,
    pos: usize,
    /// Where the input ends: the position an error at end of input points to.
    end: Pos,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|t| &t.node)
    }
    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).map(|t| t.node.clone());
        if t.is_some() {
            self.pos += 1;
        }
        t
    }
    /// Where the next token starts, or the end of input.
    fn here(&self) -> Pos {
        self.tokens.get(self.pos).map_or(self.end, |t| t.pos)
    }
    /// Where the last consumed token starts.
    fn last(&self) -> Pos {
        self.pos
            .checked_sub(1)
            .map_or(Pos::START, |i| self.tokens[i].pos)
    }
    /// An error about `token`, just returned by [`next`](Self::next): at it,
    /// or at the end of input if there was none.
    fn at_found(&self, token: &Option<Token>, msg: String) -> Diagnostic {
        let pos = if token.is_some() {
            self.last()
        } else {
            self.end
        };
        Diagnostic::new(pos, msg)
    }
    /// `expected {what}, found {token}`, at `token` (see [`at_found`](Self::at_found)).
    fn expected(&self, what: &str, token: Option<Token>) -> Diagnostic {
        let msg = format!("expected {what}, found {}", found(&token));
        self.at_found(&token, msg)
    }
    /// An error at the next token, or at the end of input.
    fn at_here(&self, msg: impl Into<String>) -> Diagnostic {
        Diagnostic::new(self.here(), msg)
    }
    /// An error at the last consumed token.
    fn at_last(&self, msg: impl Into<String>) -> Diagnostic {
        Diagnostic::new(self.last(), msg)
    }
    fn expect(&mut self, want: &Token) -> PResult<()> {
        match self.next() {
            Some(ref t) if t == want => Ok(()),
            other => Err(self.expected(&want.to_string(), other)),
        }
    }
    fn ident(&mut self) -> PResult<String> {
        match self.next() {
            Some(Token::Ident(s)) => Ok(s),
            other => Err(self.expected("identifier", other)),
        }
    }
    fn is_ident(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Token::Ident(s)) if s == kw)
    }

    fn decl(&mut self) -> PResult<Decl> {
        match self.peek() {
            Some(Token::Ident(k)) if k == "package" => self.package_decl(),
            Some(Token::Ident(k)) if k == "entity" => self.entity_decl(),
            Some(Token::Ident(k)) if k == "requires" => self.requires_decl(),
            Some(Token::Ident(k)) if k == "authority" => self.authority_decl(),
            Some(Token::Ident(k)) if k == "actor" => self.actor_decl(),
            Some(Token::Ident(k)) if k == "bootstrap" => self.bootstrap_decl(),
            Some(Token::Ident(k)) if k == "rel" => self.rel_decl(),
            Some(Token::Ident(k)) if k == "context" => {
                self.next(); // context
                Ok(Decl::Context { name: self.ident()? })
            }
            Some(Token::Ident(k)) if k == "view" => self.view_decl(false),
            Some(Token::Ident(k)) if k == "materialized" => {
                self.next(); // materialized
                if !self.is_ident("view") {
                    return Err(self.at_here("`materialized` must be followed by `view`"));
                }
                self.view_decl(true)
            }
            Some(Token::Ident(k)) if k == "form" => self.form_decl(),
            Some(Token::Ident(k)) if k == "on" => self.on_decl(),
            other => Err(self.at_here(format!(
                "expected a declaration (package/entity/requires/authority/actor/bootstrap/rel/context/view/\
                 materialized view/form/on), \
                 found {}",
                found(&other.cloned())
            ))),
        }
    }

    fn keyword(&mut self, expected: &str) -> PResult<()> {
        match self.ident()?.as_str() {
            actual if actual == expected => Ok(()),
            actual => Err(self.at_last(format!("expected `{expected}`, found `{actual}`"))),
        }
    }

    fn package_decl(&mut self) -> PResult<Decl> {
        self.next(); // package
        let id = self.ident()?;
        self.keyword("bootstrap")?;
        let version = match self.next() {
            Some(Token::Int(n)) if (0..=u32::MAX as i64).contains(&n) => n as u32,
            other => {
                let msg = format!(
                    "package bootstrap version must be a u32, found {}",
                    found(&other)
                );
                return Err(self.at_found(&other, msg));
            }
        };
        Ok(Decl::Package {
            id,
            bootstrap_version: version,
        })
    }

    fn signed_int(&mut self, what: &str) -> PResult<i64> {
        match self.next() {
            Some(Token::Int(n)) => Ok(n),
            Some(Token::Minus) => match self.next() {
                Some(Token::Int(n)) => n
                    .checked_neg()
                    .ok_or_else(|| self.at_last(format!("{what} is below i64::MIN"))),
                other => Err(self.expected(&format!("integer after `-` for {what}"), other)),
            },
            other => Err(self.expected(&format!("integer for {what}"), other)),
        }
    }

    fn entity_decl(&mut self) -> PResult<Decl> {
        self.next(); // entity
        let name = self.ident()?;
        self.expect(&Token::Eq)?;
        let id = self.signed_int("entity id")?;
        Ok(Decl::Entity { name, id })
    }

    fn requires_decl(&mut self) -> PResult<Decl> {
        self.next(); // requires
        let kind_pos = self.here();
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
                return Err(Diagnostic::new(
                    kind_pos,
                    format!(
                "unknown capability requirement `{other}` (expected allocate, random, or schedule)"
            ),
                ))
            }
        };
        self.expect(&Token::RParen)?;
        Ok(decl)
    }

    fn authority_decl(&mut self) -> PResult<Decl> {
        self.next(); // authority
        let name = self.ident()?;
        self.expect(&Token::LBrace)?;
        let mut writes = Vec::new();
        while !matches!(self.peek(), Some(Token::RBrace)) {
            if self.peek().is_none() {
                return Err(self.at_here("unterminated authority block"));
            }
            self.keyword("write")?;
            writes.push(self.ident()?);
        }
        self.next();
        Ok(Decl::Authority { name, writes })
    }

    fn actor_decl(&mut self) -> PResult<Decl> {
        let start = self.here();
        self.next(); // actor
        let entity = self.ident()?;
        self.expect(&Token::LBrace)?;
        let mut inbox = None;
        let mut cursor = None;
        let mut authority = None;
        while !matches!(self.peek(), Some(Token::RBrace)) {
            if self.peek().is_none() {
                return Err(self.at_here("unterminated actor block"));
            }
            let field_pos = self.here();
            let field = self.ident()?;
            let value = self.ident()?;
            let slot = match field.as_str() {
                "inbox" => &mut inbox,
                "cursor" => &mut cursor,
                "authority" => &mut authority,
                _ => {
                    return Err(Diagnostic::new(
                        field_pos,
                        format!("unknown actor field `{field}`"),
                    ))
                }
            };
            if slot.replace(value).is_some() {
                return Err(Diagnostic::new(
                    field_pos,
                    format!("actor field `{field}` declared twice"),
                ));
            }
        }
        self.next();
        let missing = |field: &str| Diagnostic::new(start, format!("actor needs `{field}`"));
        Ok(Decl::Actor {
            entity,
            inbox: inbox.ok_or_else(|| missing("inbox"))?,
            cursor: cursor.ok_or_else(|| missing("cursor"))?,
            authority: authority.ok_or_else(|| missing("authority"))?,
        })
    }

    fn bootstrap_decl(&mut self) -> PResult<Decl> {
        self.next(); // bootstrap
        self.expect(&Token::LBrace)?;
        let mut facts = Vec::new();
        while !matches!(self.peek(), Some(Token::RBrace)) {
            if self.peek().is_none() {
                return Err(self.at_here("unterminated bootstrap block"));
            }
            let pos = self.here();
            let rel = self.ident()?;
            self.expect(&Token::LParen)?;
            let values = if matches!(self.peek(), Some(Token::RParen)) {
                Vec::new()
            } else {
                self.bootstrap_values()?
            };
            self.expect(&Token::RParen)?;
            facts.push(BootstrapFact { rel, values, pos });
        }
        self.next(); // }
        Ok(Decl::Bootstrap { facts })
    }

    fn bootstrap_values(&mut self) -> PResult<Vec<BootstrapValue>> {
        let mut values = vec![self.bootstrap_value()?];
        while matches!(self.peek(), Some(Token::Comma)) {
            self.next();
            values.push(self.bootstrap_value()?);
        }
        Ok(values)
    }

    fn bootstrap_value(&mut self) -> PResult<BootstrapValue> {
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
                    .ok_or_else(|| self.at_last("bootstrap integer is below i64::MIN")),
                Some(Token::Float(value)) => Ok(BootstrapValue::Float(
                    grmpl_core::FiniteF64::new(-value.get()).expect("finite negation stays finite"),
                )),
                other => Err(self.expected("a number after `-`", other)),
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
            other => Err(self.expected("bootstrap literal", other)),
        }
    }

    fn identlist(&mut self) -> PResult<Vec<String>> {
        let mut out = vec![self.ident()?];
        while matches!(self.peek(), Some(Token::Comma)) {
            self.next();
            out.push(self.ident()?);
        }
        Ok(out)
    }

    fn rel_decl(&mut self) -> PResult<Decl> {
        self.next(); // rel
        let name = self.ident()?;
        self.expect(&Token::LParen)?;
        let cols = self.collist()?;
        self.expect(&Token::RParen)?;
        Ok(Decl::Rel { name, cols })
    }

    /// `collist := col ("," col)*` where `col := Ident (":" Ident)?`.
    fn collist(&mut self) -> PResult<Vec<ColDecl>> {
        let mut out = vec![self.col_decl()?];
        while matches!(self.peek(), Some(Token::Comma)) {
            self.next();
            out.push(self.col_decl()?);
        }
        Ok(out)
    }

    fn col_decl(&mut self) -> PResult<ColDecl> {
        let pos = self.here();
        let name = self.ident()?;
        let ty = if matches!(self.peek(), Some(Token::Colon)) {
            self.next(); // :
            Some(self.ident()?)
        } else {
            None
        };
        Ok(ColDecl { name, ty, pos })
    }

    fn view_decl(&mut self, materialized: bool) -> PResult<Decl> {
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
                return Err(self.at_here("view body must end with a `yield`"));
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
            materialized,
        })
    }

    /// `yieldlist := yielditem ("," yielditem)*`, splitting the items into the
    /// plain grouping columns and the (at most one) aggregate. A `yielditem`
    /// spelled `Ident "(" Ident? ")"` is an aggregate call; a bare `Ident` is a
    /// grouping column. A second aggregate, an unknown aggregate name, or a
    /// wrong aggregate arity (`count` takes no column; `sum`/`min`/`max` take
    /// one) is a parse error.
    fn yield_clause(&mut self) -> PResult<(Vec<String>, Option<AggYield>)> {
        let mut yields = Vec::new();
        let mut agg: Option<AggYield> = None;
        loop {
            let pos = self.here();
            let name = self.ident()?;
            if matches!(self.peek(), Some(Token::LParen)) {
                self.next(); // (
                let col = if matches!(self.peek(), Some(Token::RParen)) {
                    None
                } else {
                    Some(self.ident()?)
                };
                self.expect(&Token::RParen)?;
                let at = |msg: String| Diagnostic::new(pos, msg);
                let func = match name.as_str() {
                    "count" => AggFunc::Count,
                    "sum" => AggFunc::Sum,
                    "min" => AggFunc::Min,
                    "max" => AggFunc::Max,
                    other => {
                        return Err(at(format!(
                            "unknown aggregate `{other}` (expected count, sum, min, or max)"
                        )))
                    }
                };
                match (func, &col) {
                    (AggFunc::Count, Some(c)) => {
                        return Err(at(format!(
                            "aggregate `count` takes no column, found `{c}`"
                        )))
                    }
                    (AggFunc::Sum | AggFunc::Min | AggFunc::Max, None) => {
                        return Err(at(format!("aggregate `{name}` needs a column")))
                    }
                    _ => {}
                }
                if agg.is_some() {
                    return Err(at("a view `yield` may contain at most one aggregate".into()));
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

    fn atom(&mut self) -> PResult<Atom> {
        let pos = self.here();
        let mut rel = self.ident()?;
        let inherit = rel == "inherit" && matches!(self.peek(), Some(Token::Ident(_)));
        if inherit {
            rel = self.ident()?;
        }
        self.expect(&Token::LParen)?;
        let mut args = vec![self.arg()?];
        while matches!(self.peek(), Some(Token::Comma)) {
            self.next();
            args.push(self.arg()?);
        }
        self.expect(&Token::RParen)?;
        Ok(Atom {
            rel,
            args,
            inherit,
            pos,
        })
    }

    fn arg(&mut self) -> PResult<Arg> {
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
                    .ok_or_else(|| self.at_last("integer literal is below i64::MIN")),
                Some(Token::Float(n)) => Ok(Arg::Float(
                    grmpl_core::FiniteF64::new(-n.get()).expect("finite negation stays finite"),
                )),
                other => Err(self.expected("a number after `-`", other)),
            },
            other => Err(self.expected("an argument", other)),
        }
    }

    fn form_decl(&mut self) -> PResult<Decl> {
        self.next(); // form
        let name = self.ident()?;
        self.expect(&Token::LBrace)?;
        let mut rules = Vec::new();
        while !matches!(self.peek(), Some(Token::RBrace)) {
            if self.peek().is_none() {
                return Err(self.at_here("unterminated form body"));
            }
            rules.push(self.rule()?);
        }
        self.next(); // }
        Ok(Decl::Form { name, rules })
    }

    fn rule(&mut self) -> PResult<FormRule> {
        let mut seq = Vec::new();
        while !matches!(self.peek(), Some(Token::Arrow)) {
            match self.next() {
                Some(Token::Str(s)) => seq.push(PAtom::Lit(s)),
                Some(Token::Ident(s)) => seq.push(PAtom::Bind(s)),
                other => return Err(self.expected("a pattern atom or `->`", other)),
            }
        }
        if seq.is_empty() {
            return Err(self.at_here("form rule has an empty pattern"));
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

    fn on_decl(&mut self) -> PResult<Decl> {
        let start = self.here();
        self.next(); // on
                     // `on watch <view> { … }` — the reactive-handler surface — shares the
                     // `on` keyword with the message-handler `on <inbox> parse <form> { … }`,
                     // disambiguated by the `watch` keyword immediately after `on`.
        if self.is_ident("watch") {
            return self.on_watch_decl(start);
        }
        let inbox = self.ident()?;
        match self.ident()?.as_str() {
            "parse" => {}
            other => {
                return Err(self.at_last(format!("expected `parse` in on-handler, found `{other}`")))
            }
        }
        let form = self.ident()?;
        self.expect(&Token::LBrace)?;
        let mut stmt_arms = Vec::new();
        let mut word_arms = Vec::new();
        while !matches!(self.peek(), Some(Token::RBrace)) {
            if self.peek().is_none() {
                return Err(self.at_here("unterminated on-handler"));
            }
            // Each arm is `match Tag(vars)` followed by either a `{ stmt* }`
            // statement body (v1) or a `[ word* ]` concatenative body (P11);
            // the two surfaces coexist in one handler.
            let (tag, vars) = self.arm_header()?;
            match self.peek() {
                Some(Token::LBrace) => stmt_arms.push(self.stmt_arm(tag, vars)?),
                Some(Token::LBracket) => word_arms.push(self.word_arm(tag, vars)?),
                other => {
                    return Err(self.at_here(format!(
                        "expected `{{` (statement arm) or `[` (concatenative arm), found {}",
                        found(&other.cloned())
                    )))
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
    /// Ident … "}"` — a reactive handler over a maintained view. `on` (which
    /// starts at `start`) and the `watch` keyword are already consumed. Each of
    /// the three relation bindings must appear exactly once; order is free.
    fn on_watch_decl(&mut self, start: Pos) -> PResult<Decl> {
        self.next(); // watch
        let view = self.ident()?;
        let including_current = if self.is_ident("including") {
            self.next(); // including
            match self.ident()?.as_str() {
                "current" => {}
                other => {
                    return Err(self.at_last(format!(
                        "expected `current` after `including`, found `{other}`"
                    )))
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
        let set = |slot: &mut Option<String>, rel: String, key: &str, pos: Pos| -> PResult<()> {
            if slot.is_some() {
                return Err(Diagnostic::new(
                    pos,
                    format!("on-watch binding `{key}` set twice"),
                ));
            }
            *slot = Some(rel);
            Ok(())
        };
        while !matches!(self.peek(), Some(Token::RBrace)) {
            if self.peek().is_none() {
                return Err(self.at_here("unterminated on-watch body"));
            }
            let pos = self.here();
            let key = self.ident()?;
            let rel = self.ident()?;
            match key.as_str() {
                "inbox" => set(&mut inbox, rel, "inbox", pos)?,
                "cursor" => set(&mut cursor, rel, "cursor", pos)?,
                "seqs" => set(&mut seqs, rel, "seqs", pos)?,
                other => {
                    return Err(Diagnostic::new(
                        pos,
                        format!(
                            "unknown on-watch binding `{other}` (expected inbox, cursor, or seqs)"
                        ),
                    ))
                }
            }
        }
        self.next(); // }
        let missing =
            |key: &str| Diagnostic::new(start, format!("on-watch missing `{key}` binding"));
        let inbox = inbox.ok_or_else(|| missing("inbox"))?;
        let cursor = cursor.ok_or_else(|| missing("cursor"))?;
        let seqs = seqs.ok_or_else(|| missing("seqs"))?;
        Ok(Decl::OnWatch {
            view,
            inbox,
            cursor,
            seqs,
            including_current,
        })
    }

    /// `match Tag ( identlist? )` — the shared head of both arm surfaces.
    fn arm_header(&mut self) -> PResult<(String, Vec<String>)> {
        match self.ident()?.as_str() {
            "match" => {}
            other => return Err(self.at_last(format!("expected `match`, found `{other}`"))),
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

    fn stmt_arm(&mut self, tag: String, vars: Vec<String>) -> PResult<Arm> {
        self.expect(&Token::LBrace)?;
        let mut stmts = Vec::new();
        while !matches!(self.peek(), Some(Token::RBrace)) {
            if self.peek().is_none() {
                return Err(self.at_here("unterminated match arm"));
            }
            stmts.push(self.stmt()?);
        }
        self.next(); // }
        Ok(Arm { tag, vars, stmts })
    }

    /// `[ word* ]` — a point-free concatenative arm body.
    fn word_arm(&mut self, tag: String, vars: Vec<String>) -> PResult<ConcatArm> {
        self.expect(&Token::LBracket)?;
        let mut words = Vec::new();
        while !matches!(self.peek(), Some(Token::RBracket)) {
            if self.peek().is_none() {
                return Err(self.at_here("unterminated concatenative arm"));
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
    fn word(&mut self) -> PResult<Word> {
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
                    .ok_or_else(|| self.at_last("integer literal is below i64::MIN")),
                Some(Token::Float(n)) => Ok(Word::Lit(Value::Float(
                    grmpl_core::FiniteF64::new(-n.get()).expect("finite negation stays finite"),
                ))),
                other => Err(self.expected("a number after `-`", other)),
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
                        other => return Err(self.expected("`=` or `~`", other)),
                    };
                    Ok(Word::Resolve { view, col, op })
                }
                "find" => {
                    let rel = self.ident()?;
                    let keyn = match self.next() {
                        Some(Token::Int(n)) if n >= 0 => n as usize,
                        other => {
                            let msg = format!("`find` needs a key count, found {}", found(&other));
                            return Err(self.at_found(&other, msg));
                        }
                    };
                    Ok(Word::Find { rel, keyn })
                }
                "expect" => Ok(Word::Expect(self.ident()?)),
                "assert" => Ok(Word::Assert(self.ident()?)),
                "retract" => Ok(Word::Retract(self.ident()?)),
                "emit" => Ok(Word::Emit(self.ident()?)),
                other => Err(self.at_last(format!("unknown word `{other}`"))),
            },
            other => Err(self.expected("a word", other)),
        }
    }

    fn stmt(&mut self) -> PResult<Stmt> {
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
                        return Err(self.at_last(format!(
                            "expected `as` after fresh capability, found `{other}`"
                        )))
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
                        return Err(self.at_last(format!(
                            "expected `below` after random capability, found `{other}`"
                        )))
                    }
                }
                let bound = self.expr()?;
                match self.ident()?.as_str() {
                    "as" => {}
                    other => {
                        return Err(self
                            .at_last(format!("expected `as` after random bound, found `{other}`")))
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
                    other => return Err(self.at_last(format!("expected `where`, found `{other}`"))),
                }
                let col = self.ident()?;
                let op = match self.next() {
                    Some(Token::Eq) => MatchOp::Exact,
                    Some(Token::Tilde) => MatchOp::Word,
                    other => return Err(self.expected("`=` or `~`", other)),
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
            other => Err(self.at_last(format!("unknown statement `{other}`"))),
        }
    }

    fn stmt_block(&mut self) -> PResult<Vec<Stmt>> {
        self.expect(&Token::LBrace)?;
        let mut statements = Vec::new();
        while !matches!(self.peek(), Some(Token::RBrace)) {
            if self.peek().is_none() {
                return Err(self.at_here("unterminated statement block"));
            }
            statements.push(self.stmt()?);
        }
        self.next();
        Ok(statements)
    }

    fn expr(&mut self) -> PResult<Expr> {
        self.expr_or()
    }

    fn expr_or(&mut self) -> PResult<Expr> {
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

    fn expr_and(&mut self) -> PResult<Expr> {
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

    fn expr_equality(&mut self) -> PResult<Expr> {
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

    fn expr_comparison(&mut self) -> PResult<Expr> {
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

    fn expr_additive(&mut self) -> PResult<Expr> {
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

    fn expr_multiplicative(&mut self) -> PResult<Expr> {
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

    fn expr_unary(&mut self) -> PResult<Expr> {
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

    fn expr_primary(&mut self) -> PResult<Expr> {
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
            other => Err(self.expected("expression", other)),
        }
    }

    fn paren_sargs(&mut self) -> PResult<Vec<SArg>> {
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

    fn paren_exprs(&mut self) -> PResult<Vec<Expr>> {
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

    fn sarg(&mut self) -> PResult<SArg> {
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
                    .ok_or_else(|| self.at_last("integer literal is below i64::MIN")),
                Some(Token::Float(n)) => Ok(SArg::Float(
                    grmpl_core::FiniteF64::new(-n.get()).expect("finite negation stays finite"),
                )),
                other => Err(self.expected("a number after `-`", other)),
            },
            other => Err(self.expected("an argument", other)),
        }
    }
}
