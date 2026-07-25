# The completed Ent plex

grmpl does not try to make one universal `Ent` that contains everything. Its
founding note reframes the backbone as **a coordinated *family* of persistent
enfilades**, each specialized to one job, all sharing one node substrate and all
committed atomically together. This is the "completed Ent plex."

## The five enfilades

| Enfilade | Holds | Xanadu ancestor |
|---|---|---|
| **Fact** | Stored relations and their indexes — net-per-tuple world state. | orgl content trees (`OrglRoot`) |
| **Edition** | The commit-ordered delta log: patches, and (via the DAG) branches and causal ancestry. | `fulltrace` + versioned roots |
| **Context** | DSPative context carried down scopes: namespace, schema, placement. | `Dsp`-inherited context |
| **Canopy** | Standing interest: watches, subscriptions, sensors. | `CanopyCrum` |
| **Derived** | Materialized views and incremental query state. | *(grmpl's own addition)* |

The mapping to Xanadu is direct for four of the five. The **Derived** enfilade is
grmpl's genuine extension: it maintains the results of *queries* incrementally, so
that `watch q` is the maintained derivative of `find q`. Xanadu had no query
language to derive from; grmpl does, which is the second of its two deliberate
generalizations of the `Ent`.

## Two generalizations beyond Xanadu

grmpl takes the `Ent` — a hypertext-document engine — and generalizes it along
two axes Xanadu never had:

1. **The relational / Datalog data model.** The content the enfilades index is
   not spans of text but *facts*: tuples in named relations, joined by views, with
   recursive derivation. An object is not a mutable record; it is an entity
   identifier participating in relations (`located`, `held`, `named`,
   `prototype`, …). Inheritance is a recursive view, not a VM feature.

2. **Differential dataflow.** Queries are values; running one against an edition
   is a computation; *maintaining* one as editions change is the Derived
   enfilade. Deltas carry integer weights, so insertion, retraction, recursive
   maintenance, and incremental views all fall out of one algebra. `watch` is
   `find`'s derivative.

## The common substrate

Underneath all five sits one physical abstraction — the enfilade of Part I, spelled
out as a checklist:

```text
persistent measured action tree over a shared granfilade
  + stable node identities        (content-interned; structural sharing)
  + cheap split/join
  + WIDative subtree summaries     (upward measures; O(depth) range/measure)
  + DSPative coordinate transforms (downward displacement; O(edit) relocation/copy)
  + historical editions            (the fulltrace DAG; branches)
  + canopy indexes                 (interest routing)
```

Every line of that list is a chapter of Part I. The plex is just five instances of
it, specialized by which measure they carry and what they hold, sharing one
granfilade so that structural sharing works *across* the family — an edit touches
the Fact tree, the Edition log, maybe the Canopy, and writes new roots for all of
them in a single atomic batch.

## The patch is the semantic center

What ties the plex to a *language* is a single operation. A handler is
conceptually pure:

```text
handler : Snapshot × Message → Patch × Outbox × Result
```

and a **patch** is more than a bag of writes:

```text
Patch =
    preconditions          (what must hold in the edition it was computed from)
  + asserted tuples
  + retracted tuples
  + emitted messages
  + optionally scheduled work
```

Taking an object, for instance:

```text
expect located(lamp, room)
retract located(lamp, room)
assert  held(player, lamp)
emit    tell(player, "Taken.")
```

The runtime commits this **atomically against the edition it was calculated
from**, allocating the next edition — or it has no effect. If two players race for
the lamp, one patch's precondition holds and it wins; the other's fails and it is
declined or re-evaluated. From this one operation grmpl gets:

- **persistence** — every commit is a new immutable edition (a new set of roots
  in the plex);
- **concurrency** — patches are values, so they can be tested, combined,
  serialized, retried;
- **history** — the editions and deltas are retained (cheaply, by sharing);
- **distribution** — a patch names exactly the authority and data it touches;
- **deterministic replay** — a handler re-run against the same snapshot and
  message produces the same patch.

The next chapter states the laws that keep this honest; the chapter after that
shows the `grmpl-ent` crate implementing the plex; the last shows the features
resting on it.
