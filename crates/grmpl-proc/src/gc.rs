//! History GC policy — the safe use of the store's consolidation watermark (P6).
//!
//! The store ([`TraceStore::consolidate`]) can physically collapse history up to
//! any edition, but it cannot see the *live frontiers* that must still be read
//! from — those are ordinary trace rows, above the bright line. The most
//! important such frontier is the **minimum durable watch cursor**: a reactive
//! handler's [`OnWatch`](crate::OnWatch) pumps `eval_delta` starting from its
//! durable cursor edition, and `eval_delta` bottoms out at
//! [`TraceStore::scan_updates`], which errors at the watermark door. If GC
//! consolidated *past* a cursor, that watch's next pump would fail loudly
//! (never silently lose activations) — but it would be broken all the same.
//!
//! So the retention *policy* lives here: [`min_watch_cursor`] reads the cursors,
//! and [`consolidate_to`] clamps the requested horizon to them before calling
//! the store. This is the "watermark never past the minimum durable watch
//! cursor" invariant, enforced above the line where the cursors are legible.
//!
//! Cursor rows are the shared `(watch: Ent, edition: Int)` layout that
//! [`OnWatch`](crate::OnWatch) writes — one row per watch, its `edition` the
//! exclusive frontier already delivered.

use grmpl_core::{Edition, RelId, Result, TraceStore, Value};

/// The minimum durable watch cursor across every cursor relation in
/// `cursor_rels`: the least `edition` among all live (positive-weight) cursor
/// rows, read as-of the store's current edition. `None` if no watch is
/// installed in any of them (nothing constrains GC).
///
/// This is the floor GC must not cross: consolidating above it would strand a
/// watch below the watermark. A malformed cursor row (not `(Ent, Int)`) is
/// ignored — it binds no live watch frontier.
pub fn min_watch_cursor(store: &dyn TraceStore, cursor_rels: &[RelId]) -> Result<Option<Edition>> {
    let at = store.current();
    let mut min: Option<u64> = None;
    for rel in cursor_rels {
        for (tuple, diff) in store.read_at(*rel, at)? {
            if diff <= 0 {
                continue;
            }
            if let Some(Value::Int(e)) = tuple.as_slice().get(1) {
                let e = (*e).max(0) as u64;
                min = Some(min.map_or(e, |m| m.min(e)));
            }
        }
    }
    Ok(min.map(Edition))
}

/// Consolidate history toward `up_to`, but never past the minimum durable watch
/// cursor over `cursor_rels` (nor, inside the store, past `current`). Returns
/// the watermark actually reached — which may be below `up_to` if a watch cursor
/// (or an unconsolidated present) held it back, or unchanged if the clamped
/// horizon is at or below the existing watermark.
///
/// This is the sanctioned GC entry point: it keeps the "watermark ≤ minimum
/// durable watch cursor" invariant, so every installed watch stays pumpable.
pub fn consolidate_to(
    store: &dyn TraceStore,
    cursor_rels: &[RelId],
    up_to: Edition,
) -> Result<Edition> {
    let floor = match min_watch_cursor(store, cursor_rels)? {
        Some(cursor) => up_to.0.min(cursor.0),
        None => up_to.0,
    };
    store.consolidate(Edition(floor))
}
