//! # grmpl-pattern
//!
//! The pattern algebra (DESIGN.md §2.3 #3, §6). A `Pattern` is a relation
//! between an ordered input structure and a binding environment; running it
//! returns *every* way the input matches (ambiguity is a relation of parses,
//! not an error). A `Form` attaches a constructor to each rule — this realizes
//! the design's `-> construction` arrow — turning matches into semantic values.
//!
//! Pattern law: parsing is matching over ordered data, using the same
//! structural operations (sequence, choice, repetition, capture, guard) that
//! apply to tuples, messages, and ASTs. v1 runs over `Value` sequences.

use std::collections::HashMap;
use std::sync::Arc;

use grmpl_core::Value;

/// A capture variable.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct VarId(pub u32);

/// The environment produced by a successful match.
pub type Bindings = HashMap<VarId, Value>;

/// A predicate over the bindings gathered so far.
pub type Guard = Arc<dyn Fn(&Bindings) -> bool + Send + Sync>;

/// A structural pattern over a sequence of values.
#[derive(Clone)]
pub enum Pattern {
    /// Match one value equal to this literal, consuming it.
    Lit(Value),
    /// Capture one value under a variable, consuming it.
    Bind(VarId),
    /// Match sub-patterns in order.
    Seq(Vec<Pattern>),
    /// Match any one alternative (all successful parses are returned).
    Choice(Vec<Pattern>),
    /// Match zero or more repetitions (each repetition must consume input).
    Repeat(Box<Pattern>),
    /// Match the inner pattern, then keep only bindings satisfying the guard.
    Guard(Box<Pattern>, Guard),
}

impl Pattern {
    /// Every way `input` matches: each result is `(bindings, remaining input)`.
    pub fn run<'a>(&self, input: &'a [Value]) -> Vec<(Bindings, &'a [Value])> {
        match self {
            Pattern::Lit(v) => match input.split_first() {
                Some((head, rest)) if head == v => vec![(Bindings::new(), rest)],
                _ => vec![],
            },
            Pattern::Bind(var) => match input.split_first() {
                Some((head, rest)) => {
                    let mut b = Bindings::new();
                    b.insert(*var, head.clone());
                    vec![(b, rest)]
                }
                None => vec![],
            },
            Pattern::Seq(ps) => {
                let mut states: Vec<(Bindings, &[Value])> = vec![(Bindings::new(), input)];
                for p in ps {
                    let mut next = Vec::new();
                    for (b, rest) in states {
                        for (b2, rest2) in p.run(rest) {
                            let mut merged = b.clone();
                            merged.extend(b2);
                            next.push((merged, rest2));
                        }
                    }
                    states = next;
                }
                states
            }
            Pattern::Choice(ps) => ps.iter().flat_map(|p| p.run(input)).collect(),
            Pattern::Repeat(p) => {
                // Zero or more, returning every repetition count; each step must
                // make progress (shrink the input) to guarantee termination.
                let mut results: Vec<(Bindings, &[Value])> = vec![(Bindings::new(), input)];
                let mut frontier: Vec<(Bindings, &[Value])> = vec![(Bindings::new(), input)];
                loop {
                    let mut next = Vec::new();
                    for (b, rest) in &frontier {
                        for (b2, rest2) in p.run(rest) {
                            if rest2.len() >= rest.len() {
                                continue; // no progress — stop this branch
                            }
                            let mut merged = b.clone();
                            merged.extend(b2);
                            next.push((merged, rest2));
                        }
                    }
                    if next.is_empty() {
                        break;
                    }
                    results.extend(next.iter().cloned());
                    frontier = next;
                }
                results
            }
            Pattern::Guard(p, pred) => {
                p.run(input).into_iter().filter(|(b, _)| pred(b)).collect()
            }
        }
    }
}

/// A constructor turning bindings into a semantic value (`-> Take { .. }`).
pub type Ctor = Arc<dyn Fn(&Bindings) -> Value + Send + Sync>;

/// One grammar rule: a pattern and the value it constructs.
#[derive(Clone)]
pub struct Rule {
    pub pattern: Pattern,
    pub ctor: Ctor,
}

impl Rule {
    pub fn new(pattern: Pattern, ctor: impl Fn(&Bindings) -> Value + Send + Sync + 'static) -> Rule {
        Rule { pattern, ctor: Arc::new(ctor) }
    }
}

/// A `form` declaration: a choice of rules producing semantic values.
#[derive(Clone)]
pub struct Form {
    pub rules: Vec<Rule>,
}

impl Form {
    pub fn new(rules: Vec<Rule>) -> Form {
        Form { rules }
    }

    /// Every parse: `(value, remaining input)` across all rules.
    pub fn parse<'a>(&self, input: &'a [Value]) -> Vec<(Value, &'a [Value])> {
        let mut out = Vec::new();
        for r in &self.rules {
            for (b, rest) in r.pattern.run(input) {
                out.push(((r.ctor)(&b), rest));
            }
        }
        out
    }

    /// Parses that consume the entire input.
    pub fn parse_all(&self, input: &[Value]) -> Vec<Value> {
        self.parse(input)
            .into_iter()
            .filter(|(_, rest)| rest.is_empty())
            .map(|(v, _)| v)
            .collect()
    }
}
