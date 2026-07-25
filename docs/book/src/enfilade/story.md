# A structure to hold a growing document forever

## The problem Xanadu set out to solve

Project Xanadu is the oldest hypertext project — older than the Web by decades.
Its animating idea was not "documents with links." It was something stranger and
more demanding: a single, shared, permanent pool of writing in which

- nothing is ever deleted or overwritten,
- every version of every document is retained and addressable forever,
- any span of any document can be *quoted by reference* (transcluded) into any
  other, without copying the bytes,
- and editing a huge document — inserting a paragraph on page 900 of a
  million-page work — is cheap, not proportional to the size of the document.

Think about what that last point costs on ordinary representations. If a document
is an array of characters, inserting one character in the middle shifts
everything after it. If it is a list of lines, you at least avoid the byte shift,
but you still cannot ask "what is at absolute position 4,000,000?" without walking
the list. And if you also demand that the *old* version survive untouched — so
that a link into it still resolves — then naive copying makes every edit
`O(document)` in both time and space. A world that keeps all history this way
fills the disk in an afternoon.

Xanadu needed a representation where:

- **absolute addresses** into a sparse, effectively transfinite space could be
  searched in time proportional to the *depth* of a tree, not its size;
- **editing** allocated new storage only for the part that actually changed;
- and **old versions kept working** because they still pointed at the shared,
  unchanged parts.

The structure the Xanadu engineers built for this is the **enfilade**.

## What the name means

"Enfilade" is borrowed from architecture and gunnery: a suite of rooms whose
doorways line up so you can see straight through them, or a line of fire that
rakes along a column. The common thread is *a series arranged so that a property
propagates cleanly along the whole line*. That is exactly what the data structure
does — measurements propagate cleanly up and down a tree — and the metaphor is
worth keeping in mind.

## Where the ideas live

The concrete design this book leans on is **Udanax Gold**, the version of Xanadu
whose Smalltalk source was eventually released. In that source the machinery has
a whole zoo of names:

- **`Ent`** — the top-level versioned-content backbone.
- **`Loaf`** and **`Crum`** — the nodes of the measured tree (`CanopyCrum`,
  `HistoryCrum`, `SensorCrum` are specializations).
- **`Dsp`** — a *displacement*, the mechanism that lets a subtree be relocated or
  virtually copied by changing a key rather than moving data.
- **wid** — a *width*, the range of addresses a subtree covers.
- **`Orgl`** / **`OrglRoot`** — an individual content structure (roughly, "one
  document") rooted in an enfilade.
- **`GrandNode`** / **`GrandHashTable`** — the *granfilade*, the persistent node
  store underneath everything.

The next three chapters unpack the mechanics: first the **measured tree**, then
the two kinds of measure — **wids** flowing up and **dsps** flowing down — and
finally how the **`Ent`** stitches those into a versioned world with cheap
history and cheap copies. Part III then shows grmpl rebuilding each of these,
under these same names, in Rust.
