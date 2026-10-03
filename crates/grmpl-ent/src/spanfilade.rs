//! **The spanfilade: every graft, indexed from both ends.**
//!
//! A graft ([`EntStore::instance_template`](crate::EntStore::instance_template))
//! copies a block of entity space into another block. The copy is virtual —
//! the instance's Fact trees share the template's nodes — but it points one way
//! only: nothing in an instance's facts says where they came from, and nothing
//! in the template says where it went.
//!
//! Udanax Green answers both questions with two kinds of 2-D enfilade. Each
//! document's **POOM** maps its virtual positions to the permanent I-space
//! content they show, and the global **spanfilade** maps I-space spans back to
//! the documents that include them: "find every document that transcludes
//! this." Here a template block plays I-space and an instance block plays
//! V-space. The index is one sparse relation, *source span × target span*,
//! stored twice:
//!
//! * keyed by **source**, the spanfilade direction: where was this block copied
//!   to? ([`copies_of`](Spanfilade::copies_of))
//! * keyed by **target**, the POOM direction: where did this block come from?
//!   ([`sources_of`](Spanfilade::sources_of), and
//!   [`origin`](Spanfilade::origin), which follows a chain of copies back to
//!   the entity it started as.)
//!
//! That is also D4M's layout: an associative array stored beside its
//! transpose, so a lookup by either dimension is a range walk rather than a
//! scan. It pays what D4M pays — every graft is written twice — and it is
//! **append-only**, as Green's spanfilade is. A graft records that a copy
//! happened at an edition; retracting the instance's facts later does not
//! erase it, and consolidation, which retires versions, leaves provenance alone.
//!
//! Each tree is measured by the [`Hull`] of its spans, the least start and the
//! greatest end under a subtree, so a stab prunes on both ends of a span
//! rather than on its start alone.

use grmpl_core::{Edition, RelId, Result};

use crate::granfilade::{Dec, Enc, Persist};
use crate::measure::Measure;
use crate::tree::Tree;

/// One graft: the entity block `source` copied to `target` at `edition`, in
/// the relations `rels`. Both spans are half-open and the same width.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraftSpan {
    pub source: (u64, u64),
    pub target: (u64, u64),
    pub edition: Edition,
    pub rels: Vec<RelId>,
}

impl GraftSpan {
    /// How far the copy moved each entity.
    pub fn shift(&self) -> i64 {
        self.target.0.wrapping_sub(self.source.0) as i64
    }
}

/// A leg's key: `(this side's start, the other side's start, edition)`. The
/// edition keeps two copies between the same blocks apart — possible once an
/// instance's facts are retracted and the block is filled again.
type LegKey = (u64, u64, u64);

/// What a leg holds beyond its key: the span's width and the relations copied.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Leg {
    width: u64,
    rels: Vec<u32>,
}

impl Persist for Leg {
    fn encode(&self, e: &mut Enc<'_, '_>) {
        self.width.encode(e);
        (self.rels.len() as u32).encode(e);
        for r in &self.rels {
            r.encode(e);
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self> {
        let width = u64::decode(d)?;
        let n = u32::decode(d)? as usize;
        let mut rels = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            rels.push(u32::decode(d)?);
        }
        Ok(Leg { width, rels })
    }
}

/// The least start and greatest end of the spans under a subtree, on the side
/// the tree is keyed by; `None` for the empty subtree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hull(pub Option<(u64, u64)>);

impl Hull {
    /// Whether some span under this hull may overlap `[lo, hi)`.
    fn meets(&self, lo: u64, hi: u64) -> bool {
        self.0.is_some_and(|(start, end)| start < hi && lo < end)
    }
}

impl Measure<LegKey, Leg> for Hull {
    fn empty() -> Self {
        Hull(None)
    }
    fn entry(k: &LegKey, v: &Leg) -> Self {
        Hull(Some((k.0, k.0.saturating_add(v.width))))
    }
    fn combine(&self, right: &Self) -> Self {
        Hull(match (self.0, right.0) {
            (None, r) => r,
            (l, None) => l,
            (Some((a0, a1)), Some((b0, b1))) => Some((a0.min(b0), a1.max(b1))),
        })
    }
    /// The index is never relocated: its keys are fixed coordinates.
    fn displace(&self, _by: i64) -> Self {
        *self
    }
}

impl Persist for Hull {
    fn encode(&self, e: &mut Enc<'_, '_>) {
        self.0.encode(e);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self> {
        Ok(Hull(Option::decode(d)?))
    }
}

type SpanEnf = Tree<LegKey, Leg, Hull>;

/// Every graft a branch has made, keyed by source and again by target.
#[derive(Clone, Default)]
pub struct Spanfilade {
    by_source: SpanEnf,
    by_target: SpanEnf,
}

impl Spanfilade {
    pub fn new() -> Spanfilade {
        Spanfilade::default()
    }

    /// How many grafts are recorded.
    pub fn len(&self) -> usize {
        self.by_source.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_source.is_empty()
    }

    /// Record a graft in both trees.
    pub fn record(&mut self, g: &GraftSpan) {
        let leg = Leg {
            width: g.source.1.saturating_sub(g.source.0),
            rels: g.rels.iter().map(|r| r.0).collect(),
        };
        let ed = g.edition.0;
        self.by_source = self.by_source.insert((g.source.0, g.target.0, ed), leg.clone());
        self.by_target = self.by_target.insert((g.target.0, g.source.0, ed), leg);
    }

    /// The grafts made at or before `at` whose **source** overlaps `[lo, hi)`:
    /// every place this block was copied to, in source order.
    pub fn copies_of(&self, lo: u64, hi: u64, at: Edition) -> Vec<GraftSpan> {
        Self::stab(&self.by_source, lo, hi, at)
            .map(|((s, t, ed), leg)| span(s, t, ed, leg))
            .collect()
    }

    /// The grafts made at or before `at` whose **target** overlaps `[lo, hi)`:
    /// where the facts in this block were copied from, in target order.
    pub fn sources_of(&self, lo: u64, hi: u64, at: Edition) -> Vec<GraftSpan> {
        Self::stab(&self.by_target, lo, hi, at)
            .map(|((t, s, ed), leg)| span(s, t, ed, leg))
            .collect()
    }

    /// **The entity `e` started as**, as of `at`, and the grafts that carried
    /// it here, most recent first. An entity no graft wrote is its own origin.
    ///
    /// Each step takes the latest graft into a block holding `e` that is
    /// strictly older than the step before it — the source has to exist before
    /// it is copied — so the walk ends however blocks were reused.
    pub fn origin(&self, e: u64, at: Edition) -> (u64, Vec<GraftSpan>) {
        let (mut e, mut before, mut chain) = (e, at.0.saturating_add(1), Vec::new());
        loop {
            // Strictly older than the previous step: `stab` keeps editions
            // at or before its bound.
            let hit = Self::stab(&self.by_target, e, e.saturating_add(1), Edition(before - 1))
                .max_by_key(|((_, _, ed), _)| *ed);
            let Some(((t, s, ed), leg)) = hit else { return (e, chain) };
            e = s + (e - t);
            before = ed;
            chain.push(span(s, t, ed, leg));
        }
    }

    /// Only the grafts made at or before `at` — the index a branch forked into
    /// the past starts from. `O(grafts)`: the trees are keyed by span, not by
    /// edition, so there is no single cut to make.
    pub fn as_of(&self, at: Edition) -> Spanfilade {
        let mut out = Spanfilade::new();
        for ((s, t, ed), leg) in self.by_source.iter() {
            if ed <= at.0 {
                out.record(&span(s, t, ed, leg.clone()));
            }
        }
        out
    }

    /// The legs of `tree` overlapping `[lo, hi)` made at or before `at`,
    /// pruned on each subtree's hull.
    fn stab(tree: &SpanEnf, lo: u64, hi: u64, at: Edition) -> impl Iterator<Item = (LegKey, Leg)> {
        tree.search(|h| h.meets(lo, hi), |k, leg| Hull::entry(k, leg).meets(lo, hi) && k.2 <= at.0)
            .into_iter()
    }
}

fn span(s: u64, t: u64, ed: u64, leg: Leg) -> GraftSpan {
    GraftSpan {
        source: (s, s + leg.width),
        target: (t, t + leg.width),
        edition: Edition(ed),
        rels: leg.rels.into_iter().map(RelId).collect(),
    }
}

/// The two trees persist as links, so GC follows both from the branch state.
impl Persist for Spanfilade {
    fn encode(&self, e: &mut Enc<'_, '_>) {
        e.link(&self.by_source);
        e.link(&self.by_target);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self> {
        Ok(Spanfilade { by_source: d.link()?, by_target: d.link()? })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(src: u64, dst: u64, width: u64, ed: u64) -> GraftSpan {
        GraftSpan {
            source: (src, src + width),
            target: (dst, dst + width),
            edition: Edition(ed),
            rels: vec![RelId(1), RelId(2)],
        }
    }

    #[test]
    fn a_graft_is_found_from_either_end() {
        let mut s = Spanfilade::new();
        s.record(&g(1_000, 2_000, 10, 5));
        s.record(&g(1_000, 3_000, 10, 6));
        s.record(&g(500, 4_000, 20, 7));
        let now = Edition(10);

        let copies: Vec<u64> = s.copies_of(1_003, 1_004, now).iter().map(|c| c.target.0).collect();
        assert_eq!(copies, vec![2_000, 3_000], "both instances of the template");
        assert!(s.copies_of(1_010, 1_100, now).is_empty(), "the span ends at 1010");
        let sources = s.sources_of(3_000, 3_010, now);
        assert_eq!(sources, vec![g(1_000, 3_000, 10, 6)]);
        assert_eq!(sources[0].shift(), 2_000);
        // As of an earlier edition, the later grafts had not happened.
        assert_eq!(s.copies_of(1_000, 1_010, Edition(5)).len(), 1);
    }

    #[test]
    fn origin_follows_a_chain_of_copies_back() {
        let mut s = Spanfilade::new();
        s.record(&g(1_000, 2_000, 100, 3)); // template → instance
        s.record(&g(2_000, 9_000, 100, 8)); // instance → copy of the instance
        let (start, chain) = s.origin(9_042, Edition(10));
        assert_eq!(start, 1_042);
        assert_eq!(chain.iter().map(|c| c.edition.0).collect::<Vec<_>>(), vec![8, 3]);
        // Before the second copy, 9042 was nobody's copy.
        assert_eq!(s.origin(9_042, Edition(7)), (9_042, vec![]));
        // An entity no graft wrote is its own origin.
        assert_eq!(s.origin(1_042, Edition(10)).0, 1_042);
    }

    #[test]
    fn a_reused_block_cannot_send_origin_round_in_a_cycle() {
        let mut s = Spanfilade::new();
        s.record(&g(1_000, 2_000, 10, 3)); // A → B
        s.record(&g(2_000, 1_000, 10, 6)); // B → A, after A was emptied
        // At 6, A's facts came from B, which came from A at 3: two steps, then
        // stop, because nothing was copied into A before edition 3.
        let (start, chain) = s.origin(1_004, Edition(9));
        assert_eq!((start, chain.len()), (1_004, 2));
    }

    #[test]
    fn origin_never_steps_to_a_graft_of_the_same_edition() {
        // Two grafts at one edition swapping blocks. Following both would
        // cycle; the source of a copy must predate it, so the walk takes one.
        let mut s = Spanfilade::new();
        s.record(&g(1_000, 2_000, 10, 3));
        s.record(&g(2_000, 1_000, 10, 3));
        let (start, chain) = s.origin(1_004, Edition(9));
        assert_eq!((start, chain.len()), (2_004, 1));
    }

    #[test]
    fn as_of_keeps_only_earlier_grafts() {
        let mut s = Spanfilade::new();
        for ed in 1..=6 {
            s.record(&g(1_000, 1_000 + ed * 100, 10, ed));
        }
        let past = s.as_of(Edition(3));
        assert_eq!(past.len(), 3);
        assert_eq!(past.copies_of(1_000, 1_010, Edition(99)).len(), 3);
    }
}
