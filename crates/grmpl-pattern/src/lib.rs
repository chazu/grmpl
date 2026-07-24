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
//! apply to tuples, messages, and ASTs.
//!
//! ## Input abstraction ([`MatchInput`])
//!
//! The engine reads its input through one seam: a [`MatchInput`] is a *cursor*
//! into some ordered structure plus a well-founded *progress measure*. The run
//! engine is written once against the trait, so the same algebra runs over
//! three shapes without change:
//!
//! * **tuple/record** — a flat `&[Value]` row (a relation tuple, a message
//!   body); the v1 input, kept as a bare slice.
//! * **AST-descend** ([`AstInput`]) — the children of a `Value::Tuple` node;
//!   `descend` opens a child node into its own cursor, so a nested shape is
//!   matched by descending and recursing.
//! * **bytes** ([`BytesInput`]) — a `Value::Bytes` byte string, each byte
//!   surfaced as `Value::Int(byte)` so the same literal/capture patterns apply.
//!
//! The measure is what makes `Repeat` terminate: every iteration must strictly
//! lower it, so an empty match can never loop forever.

use std::collections::HashMap;
use std::sync::Arc;

use grmpl_core::Value;

/// A cursor over an ordered input with a well-founded progress measure.
///
/// A pattern consumes its input left to right; a `MatchInput` is *where the
/// cursor sits now*. The whole algebra needs only two operations:
///
/// * [`next`](MatchInput::next) — read the [`Value`] under the cursor and hand
///   back the cursor advanced past it, or `None` at the end.
/// * [`measure`](MatchInput::measure) — a natural number that **strictly
///   decreases** on every `next` and hits zero exactly at the end. `Repeat`
///   requires each iteration to lower it, which is what makes repetition
///   well-founded and guarantees termination.
///
/// Implementors are cheap-to-clone cursors (a slice reference and, for byte or
/// AST inputs, a thin wrapper), so the backtracking engine may fork freely.
pub trait MatchInput: Clone {
    /// Read the item under the cursor and the cursor past it, or `None` at end.
    fn next(&self) -> Option<(Value, Self)>;

    /// The remaining progress measure: strictly decreases on every [`next`] and
    /// is zero exactly when no input remains.
    ///
    /// [`next`]: MatchInput::next
    fn measure(&self) -> usize;

    /// No input remains under the cursor. Default: the measure has bottomed out.
    fn at_end(&self) -> bool {
        self.measure() == 0
    }
}

/// The **tuple/record** instance: a flat row of [`Value`]s (a relation tuple, a
/// message body). Kept as a bare `&[Value]` so existing callers pass slices and
/// get slices back unchanged.
impl<'a> MatchInput for &'a [Value] {
    fn next(&self) -> Option<(Value, Self)> {
        let row: &'a [Value] = self;
        row.split_first().map(|(head, rest)| (head.clone(), rest))
    }

    fn measure(&self) -> usize {
        self.len()
    }
}

/// The **AST-descend** instance: a cursor over the children of an AST node.
///
/// An AST is a `Value` tree whose interior nodes are `Value::Tuple`s. A pattern
/// matches one level of children; [`descend`](AstInput::descend) opens a child
/// tuple into its own cursor, so matching a nested shape is *descend, match the
/// sub-pattern, recurse*. It is distinct from a plain slice by construction:
/// you get an `AstInput` by descending a node, not by naming a row.
#[derive(Clone, Copy)]
pub struct AstInput<'a>(&'a [Value]);

impl<'a> AstInput<'a> {
    /// A cursor over `node`'s children, if `node` is an interior (tuple) node;
    /// `None` for a leaf (scalar) node.
    pub fn descend(node: &'a Value) -> Option<AstInput<'a>> {
        match node {
            Value::Tuple(children) => Some(AstInput(children)),
            _ => None,
        }
    }

    /// A cursor over a forest of sibling nodes (e.g. a top-level term list).
    pub fn forest(nodes: &'a [Value]) -> AstInput<'a> {
        AstInput(nodes)
    }

    /// The nodes still under the cursor — the handle you pass back to
    /// [`descend`] on a captured child.
    ///
    /// [`descend`]: AstInput::descend
    pub fn nodes(&self) -> &'a [Value] {
        self.0
    }
}

impl<'a> MatchInput for AstInput<'a> {
    fn next(&self) -> Option<(Value, Self)> {
        self.0.split_first().map(|(head, rest)| (head.clone(), AstInput(rest)))
    }

    fn measure(&self) -> usize {
        self.0.len()
    }
}

/// The **bytes** instance: a cursor over a byte string. Each byte is surfaced as
/// `Value::Int(byte)` (0..=255) so the same literal and capture patterns that
/// match values match bytes. [`of`](BytesInput::of) opens a `Value::Bytes`;
/// the tuple field wraps a raw slice directly.
#[derive(Clone, Copy)]
pub struct BytesInput<'a>(pub &'a [u8]);

impl<'a> BytesInput<'a> {
    /// A cursor over a `Value::Bytes` byte string, or `None` for any other
    /// value.
    pub fn of(v: &'a Value) -> Option<BytesInput<'a>> {
        match v {
            Value::Bytes(b) => Some(BytesInput(b)),
            _ => None,
        }
    }
}

impl<'a> MatchInput for BytesInput<'a> {
    fn next(&self) -> Option<(Value, Self)> {
        self.0.split_first().map(|(b, rest)| (Value::Int(i64::from(*b)), BytesInput(rest)))
    }

    fn measure(&self) -> usize {
        self.0.len()
    }
}

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
    /// Every way `input` matches, over any [`MatchInput`]: each result is
    /// `(bindings, remaining input)`. This is the one engine; the tuple/record,
    /// AST-descend, and byte instances are the same algebra over different
    /// cursors.
    pub fn run_in<I: MatchInput>(&self, input: I) -> Vec<(Bindings, I)> {
        match self {
            Pattern::Lit(v) => match input.next() {
                Some((head, rest)) if &head == v => vec![(Bindings::new(), rest)],
                _ => vec![],
            },
            Pattern::Bind(var) => match input.next() {
                Some((head, rest)) => {
                    let mut b = Bindings::new();
                    b.insert(*var, head);
                    vec![(b, rest)]
                }
                None => vec![],
            },
            Pattern::Seq(ps) => {
                let mut states: Vec<(Bindings, I)> = vec![(Bindings::new(), input)];
                for p in ps {
                    let mut next = Vec::new();
                    for (b, rest) in states {
                        for (b2, rest2) in p.run_in(rest.clone()) {
                            let mut merged = b.clone();
                            merged.extend(b2);
                            next.push((merged, rest2));
                        }
                    }
                    states = next;
                }
                states
            }
            Pattern::Choice(ps) => ps.iter().flat_map(|p| p.run_in(input.clone())).collect(),
            Pattern::Repeat(p) => {
                // Zero or more, returning every repetition count; each step must
                // make progress (lower the measure) to guarantee termination.
                let mut results: Vec<(Bindings, I)> = vec![(Bindings::new(), input.clone())];
                let mut frontier: Vec<(Bindings, I)> = vec![(Bindings::new(), input)];
                loop {
                    let mut next = Vec::new();
                    for (b, rest) in &frontier {
                        for (b2, rest2) in p.run_in(rest.clone()) {
                            if rest2.measure() >= rest.measure() {
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
                p.run_in(input).into_iter().filter(|(b, _)| pred(b)).collect()
            }
        }
    }

    /// The tuple/record convenience: every way a flat `Value` row matches. This
    /// is [`run_in`](Pattern::run_in) at `I = &[Value]`, kept as the original
    /// signature so existing callers pass and receive slices unchanged.
    pub fn run<'a>(&self, input: &'a [Value]) -> Vec<(Bindings, &'a [Value])> {
        self.run_in(input)
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

    /// Every parse over any [`MatchInput`]: `(value, remaining input)` across
    /// all rules.
    pub fn parse_in<I: MatchInput>(&self, input: I) -> Vec<(Value, I)> {
        let mut out = Vec::new();
        for r in &self.rules {
            for (b, rest) in r.pattern.run_in(input.clone()) {
                out.push(((r.ctor)(&b), rest));
            }
        }
        out
    }

    /// Parses over any [`MatchInput`] that consume the entire input.
    pub fn parse_all_in<I: MatchInput>(&self, input: I) -> Vec<Value> {
        self.parse_in(input)
            .into_iter()
            .filter(|(_, rest)| rest.at_end())
            .map(|(v, _)| v)
            .collect()
    }

    /// The tuple/record convenience: every parse of a flat `Value` row —
    /// [`parse_in`](Form::parse_in) at `I = &[Value]`.
    pub fn parse<'a>(&self, input: &'a [Value]) -> Vec<(Value, &'a [Value])> {
        self.parse_in(input)
    }

    /// The tuple/record convenience: parses of a flat `Value` row that consume
    /// it entirely.
    pub fn parse_all(&self, input: &[Value]) -> Vec<Value> {
        self.parse_all_in(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(v: u8) -> Value {
        Value::Int(i64::from(v))
    }

    #[test]
    fn bytes_input_matches_literals_and_captures() {
        // `Value::Bytes` opens into a byte cursor; each byte is a `Value::Int`.
        let blob = Value::bytes([0x41u8, 0x42, 0x43]); // "ABC"
        let input = BytesInput::of(&blob).expect("Value::Bytes opens");

        // Literal 'A', capture 'B', literal 'C', consuming the whole string.
        let pat = Pattern::Seq(vec![
            Pattern::Lit(b(0x41)),
            Pattern::Bind(VarId(0)),
            Pattern::Lit(b(0x43)),
        ]);
        let results = pat.run_in(input);
        assert_eq!(results.len(), 1);
        let (bindings, rest) = &results[0];
        assert!(rest.at_end(), "the pattern consumed every byte");
        assert_eq!(bindings.get(&VarId(0)), Some(&b(0x42)));
    }

    #[test]
    fn bytes_of_rejects_non_bytes() {
        assert!(BytesInput::of(&Value::Int(3)).is_none());
    }

    #[test]
    fn repeat_over_bytes_terminates_via_progress_measure() {
        // `Repeat` leans on the progress measure to terminate; a byte cursor's
        // measure is its remaining length, which each `Bind` strictly lowers.
        let blob = Value::bytes([1u8, 2, 3, 4]);
        let input = BytesInput::of(&blob).unwrap();
        let star = Pattern::Repeat(Box::new(Pattern::Bind(VarId(0))));

        let full: Vec<_> =
            star.run_in(input).into_iter().filter(|(_, rest)| rest.at_end()).collect();
        // Exactly one way to consume all four bytes (last capture wins in the map).
        assert_eq!(full.len(), 1);
        assert_eq!(full[0].0.get(&VarId(0)), Some(&b(4)));
    }

    #[test]
    fn ast_descend_matches_a_nested_shape() {
        // AST: (call "f" (args 1 2)). Descend the top node, match the head, then
        // descend the captured child and match its contents.
        let args = Value::Tuple(Arc::from(vec![
            Value::text("args"),
            Value::Int(1),
            Value::Int(2),
        ]));
        let node = Value::Tuple(Arc::from(vec![
            Value::text("call"),
            Value::text("f"),
            args.clone(),
        ]));

        // Match the interior node's children: "call", capture the callee, capture
        // the arg list.
        let top = Pattern::Seq(vec![
            Pattern::Lit(Value::text("call")),
            Pattern::Bind(VarId(0)),
            Pattern::Bind(VarId(1)),
        ]);
        let cursor = AstInput::descend(&node).expect("interior node descends");
        let outer = top.run_in(cursor);
        assert_eq!(outer.len(), 1);
        let (b0, rest) = &outer[0];
        assert!(rest.at_end());
        assert_eq!(b0.get(&VarId(0)), Some(&Value::text("f")));

        // Descend into the captured arg child and match "args" then two ints.
        let child = b0.get(&VarId(1)).unwrap();
        let inner_cursor = AstInput::descend(child).expect("arg list descends");
        let inner = Pattern::Seq(vec![
            Pattern::Lit(Value::text("args")),
            Pattern::Bind(VarId(2)),
            Pattern::Bind(VarId(3)),
        ])
        .run_in(inner_cursor);
        assert_eq!(inner.len(), 1);
        assert!(inner[0].1.at_end());
        assert_eq!(inner[0].0.get(&VarId(2)), Some(&Value::Int(1)));
        assert_eq!(inner[0].0.get(&VarId(3)), Some(&Value::Int(2)));
    }

    #[test]
    fn ast_descend_rejects_a_leaf() {
        assert!(AstInput::descend(&Value::Int(7)).is_none());
    }

    #[test]
    fn slice_and_run_in_agree_for_value_rows() {
        // The tuple/record instance: `run` (slice) and `run_in` coincide.
        let row = [Value::text("take"), Value::text("lamp")];
        let pat = Pattern::Seq(vec![Pattern::Lit(Value::text("take")), Pattern::Bind(VarId(0))]);
        let via_slice = pat.run(&row);
        let via_generic = pat.run_in(&row[..]);
        assert_eq!(via_slice.len(), 1);
        assert_eq!(via_slice, via_generic);
        assert_eq!(via_slice[0].0.get(&VarId(0)), Some(&Value::text("lamp")));
    }
}
