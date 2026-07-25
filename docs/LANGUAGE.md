# The grmpl language — a guide

`grmpl` is a *differential, relational substrate for a versioned world*. This
guide is a practical reference to the **text surface** — the little language you
write in a `.grmpl` file — its features, its syntax, and the idioms that fall out
of its design. It is written to be read top to bottom the first time and dipped
into afterwards.

The running example throughout is [`worlds/moo.grmpl`](../worlds/moo.grmpl), a
small MOO (a multi-room text world). Everything the world *means* — rooms,
verbs, an autonomous cat, a hand of cards and how it scores — lives in that file
as relations, views, and behaviors. The binary that runs it (`grmpl run`) only
does terminal I/O and drives the clock.

---

## 1. The mental model

Five ideas carry the whole language.

* **Relations** are the only state. A relation is a named, typed set of tuples —
  `located(thing, place)`, `named(thing, name)`. There are no objects, records,
  or tables with hidden fields; everything the world knows is a tuple in some
  relation.

* **Editions** are versions. Every change advances a monotonic *edition*. The
  world is never overwritten — you can read any relation *as of* any past
  edition. Time is a first-class axis, not a mutation you lose.

* **Patches** are how the world changes. A patch is a small transaction:
  *preconditions* it expects to hold, tuples to *assert* (add) and *retract*
  (remove), and *messages* to *emit*. A patch either commits atomically at the
  next edition or has no effect.

* **Views** are how you read. A view is a query — a join of relations with a
  projection — that is *derived on read*, never stored. `here(you)` is not a
  table; it is `located ⋈ located ⋈ named` evaluated against the current (or any
  past) edition.

* **Behaviors** are how things act. A behavior is a pure function from
  `(snapshot, message)` to a patch. A player's verbs are a behavior; so is a
  wandering NPC. Because a behavior is pure in its inputs, the whole world is
  deterministically **replayable**.

Everything below is syntax for producing relations, views, and behaviors.

---

## 2. Lexical basics

* **Comments** run from `//` to end of line.
* **Identifiers** are `letter (letter | digit | _)*`.
* **String literals** are `"double quoted"`.
* **Integer literals** are bare decimals: `15`, `-3`.
* Whitespace and newlines are insignificant except as token separators.

A program is a sequence of top-level declarations: `rel`, `view`, `form`, `on`.
Order is free (a `view` may mention a `rel` declared later).

---

## 3. Relations — `rel`

```grmpl
rel located(thing, place)                    // untyped columns (type = Any)
rel value(thing: Ent, coins: Int)            // typed columns
rel card(holder: Ent, slot: Int, code: Int)
```

A `rel` names a relation and its ordered columns. A column is `name` or
`name: Type`. The types are:

| Type    | Values                                             |
|---------|----------------------------------------------------|
| `Ent`   | an entity id (an opaque handle to a "thing")       |
| `Int`   | a 64-bit integer                                   |
| `Text`  | a string                                           |
| `Bool`  | a boolean                                          |
| `Tuple` | a nested tuple of values                           |
| `Bytes` | an opaque byte string                              |
| `Any`   | anything (the default for an unannotated column)   |

Types are **documentation and optional enforcement**: when a relation's schema
is registered, every asserted or retracted tuple is checked at the commit
boundary (arity + column types), and schemas may only *grow* (append columns) —
never change an existing column. If you don't register schemas, the columns are
unchecked. Either way the annotations tell a reader what a tuple means.

A `rel` declares no data. Tuples enter the world through patches (`assert`) or
are seeded by the host program — **there is no fact-literal syntax** (see
§9).

---

## 4. Views — `view`

A view is a conjunctive query that reads the world.

```grmpl
view here(viewer) {
    located(viewer, room)      // viewer is in some room
    located(thing, room)       // thing is in the SAME room (shared `room`)
    named(thing, name)         // and has a name
    yield thing, name
}
```

### How a body reads

The body is a list of **atoms** `Rel(arg, …)`. Each `arg` is a variable
(identifier), a string, or an integer. The rules:

* A **shared variable** across atoms is a **join**: `room` appears in two
  `located` atoms, so they join on it.
* A **repeated variable within one atom** is an equality constraint.
* A **literal** argument is a **filter**: `permits(viewer, "see", thing)` keeps
  only rows whose second column is `"see"`.
* A **view parameter** (here `viewer`) is bound by the caller and filters
  likewise.

`yield` projects the result to the listed columns and **de-duplicates** it
(distinct rows). Every yielded variable must be bound somewhere in the body, or
the view is rejected at instantiation.

A view compiles to a relational query (`joins → project → distinct`) and is
evaluated with `find`, against a snapshot at any edition — so the *same* view is
both "the world now" and "the world then."

### Aggregates — `yield group…, agg(col)`

A `yield` whose last item is an aggregate lowers to a grouped reduction:

```grmpl
view treasure() {
    located(thing, room)
    value(thing, coins)
    named(room, rname)
    yield rname, sum(coins)          // total coin value per room
}
```

The columns before the aggregate are the **grouping key**; the aggregate folds
each group. Available aggregates:

| Aggregate  | Meaning                                    |
|------------|--------------------------------------------|
| `sum(col)` | sum of a column over the group             |
| `min(col)` | minimum                                    |
| `max(col)` | maximum                                    |
| `count()`  | number of **distinct projected rows** *    |

At most one aggregate per view, and it must be last. `sum/min/max` take a
column; `count()` takes none.

> **⚠ The `count()` gotcha.** `count()` counts the *distinct projected rows* in
> each group **after** the view's `distinct`. Because a `count()` yield projects
> only the grouping columns, each group collapses to a single distinct row and
> the count is always `1`. To count *members* you need a non-grouping column in
> play — which the `yield` grammar doesn't give you. In practice, reach for
> `sum` over a value column (see how `moo.grmpl` scores treasure), or count rows
> host-side.

---

## 5. Command grammars — `form`

A `form` turns a line of text into a tagged tuple — a tiny parser.

```grmpl
form command {
    "take" noun -> Take(noun)
    "go"   dir  -> Go(dir)
    "look"      -> Look()
}
```

Each rule is `pattern+ -> Tag(vars?)`. A pattern atom is either a **string
literal** (matches that exact token) or an **identifier** (binds one token). The
right-hand `Tag(vars)` builds a tuple `[Tag, bound values…]`.

> **One token per bind.** `noun` binds a *single* token. `take brass lamp`
> does not parse against `"take" noun` (three tokens, two-token pattern);
> `take lamp` does. Match nouns are keywords, and `~` (word-match, below) lets
> `lamp` resolve against a thing named `"brass lamp"`.

A parsed line produces zero or more tagged tuples (the ones whose pattern
matched). A behavior dispatches on the tag.

---

## 6. Behaviors — `on <inbox> parse <form>`

A behavior consumes messages from an inbox, parses each with a `form`, and
lowers the matched verb to a **patch**.

```grmpl
on inbox parse command {
    match Take(noun) {
        resolve here(self) where name ~ noun    // find the thing by name
        find located(thing, room)               // where is it?
        expect located(thing, room)             // guard: still there
        retract located(thing, room)            // remove it from the room
        assert held(self, thing)                // put it in my hands
        emit tell(self, "Taken.")               // tell the player
    }
    match Look() { … }
}
```

`self` is bound to the process's own entity. Each `match Tag(vars)` arm binds the
tag's payload, then runs a body. **A handler may have many arms**; the two body
styles below can be freely mixed within one handler.

### The statement surface

A statement body is a `{ … }` block of these statements, evaluated top to bottom
while accumulating a patch and a set of bound variables:

| Statement                                   | Effect                                                                                   |
|---------------------------------------------|------------------------------------------------------------------------------------------|
| `resolve View(args) where col ~ rhs`        | run `View`, pick the **least** row whose `col` matches `rhs`, bind all its yielded columns |
| `resolve View(args) where col = rhs`        | same, but `col` must equal `rhs` exactly                                                  |
| `find Rel(args)`                            | bind the unbound variable columns from the **least** matching base tuple                  |
| `expect Rel(args)`                          | add a **precondition**: this tuple must hold at commit                                    |
| `assert Rel(args)`                          | add this tuple                                                                            |
| `retract Rel(args)`                         | remove this tuple                                                                         |
| `emit Rel(args)`                            | enqueue a message tuple into `Rel` (an inbox)                                             |

`args`/`rhs` are variables, strings, or integers. The two match operators:

* `=` — **exact**: the column equals the value.
* `~` — **word-membership**: the value appears as a whitespace-delimited word in
  the column (so `name ~ "lamp"` matches `"brass lamp"`).

A `resolve` or `find` that matches nothing **aborts the arm with an empty
patch** — nothing changes. This is why `take sword`, when there is no sword, is
a clean no-op.

Binding to the **least** matching tuple (never "whichever the scan surfaced
first") is what keeps behaviors deterministic.

### The concatenative surface (point-free)

The same arm can be written as a `[ … ]` stack-machine body. The arm's match
variables are the **initial stack**; words push, shuffle, and drive the effect
seam. This handler is byte-for-byte equivalent to the statement `Take` above:

```grmpl
match Take(noun) [
    self swap resolve here name ~     // resolve the thing by name
    drop
    dup find located 1                // look up its room
    dup2 expect located               // keep a copy to both guard and retract
    retract located
    self swap assert held
    self "Taken." emit tell
]
```

The vocabulary:

* **Stack shufflers**: `dup drop swap over rot nip tuck dup2 drop2`.
* **Pushers**: `self` (your entity), string/integer literals.
* **Effect seam** (consumes immediate operands from the source, stack operands
  at run time): `resolve <view> <col> (=|~)`, `find <rel> <keyn>`,
  `expect <rel>`, `assert <rel>`, `retract <rel>`, `emit <rel>`.

Every word has a declared **stack effect**, and the compiler rejects a body that
under-flows the stack or leaves it non-empty. The two surfaces lower to the
identical effect primitives, so a program written either way produces identical
editions.

### What a behavior *is*

`Program::behavior(prog, "inbox", entity)` compiles the handler into a pure
`Fn(&Snapshot, &Tuple) -> Patch`. The runtime enqueues a message, runs the
process (which reads a snapshot, calls the behavior, and commits the patch
optimistically), and advances an inbox cursor. Because the behavior is pure in
`(snapshot, message)`, the entire run can be **replayed** from history and
reproduces every patch and edition exactly.

---

## 7. Reactive handlers — `on watch <view>`

A watch turns a view into a live feed: as the view's result changes, each delta
is delivered, **exactly once**, into an inbox.

```grmpl
on watch world including current { inbox wmail cursor wcursor seqs wseq }
```

* `<view>` is the maintained view (`world` here).
* The three bindings name the relations the pump uses: the activation `inbox`,
  the durable `cursor` (the exactly-once guard, which survives restarts), and
  the shared `seqs` counter.
* `including current` delivers the *entire current view once* on the first pump;
  the default (omit it) is skip-initial — only changes after installation
  stream.

Each activation is a signed row (`+` added, `−` removed) with a contiguous,
monotonic sequence number. The cursor is durable, so a client that disconnects
and reconnects resumes with no gaps and no double-delivery.

---

## 8. How a program runs

1. **Compile.** `Program::compile(src, rel_base)` parses the file and assigns a
   stable `RelId` to each relation. Nothing executes yet — the program is stored
   AST plus a name→id table. (`compile_with_catalog` resolves ids through a
   durable catalog so they survive reopen.)
2. **Read** with a view: `prog.view("here", &[player])` yields a `Query`; run it
   with `query.find(&snapshot)` at any edition.
3. **Act** with a behavior: build a `Process` (entity + inbox + cursor +
   behavior + authority), `enqueue` a message, `run_to_idle`. Each step reads a
   snapshot, produces a patch, and commits it.
4. **Commit.** A patch commits **atomically** at the next edition, *or not at
   all*. Preconditions (`expect`) are re-checked at commit time via an optimistic
   `commit_if`: if two processes race the same precondition, exactly one wins and
   the loser's patch is rejected (it changed nothing). This is how the world
   stays consistent without locks.

**Authority.** Every commit is made under an *authority* that scopes which
relations a process may write. It is a capability, checked at the commit
boundary — not part of the text surface, but supplied when you wire a process.

---

## 9. What the surface deliberately leaves out (and the idioms)

The text surface is small on purpose. These are the walls you will hit, and the
grmpl-idiomatic way around each — **the pattern is almost always "turn the
computation into data and join against it."**

* **No arithmetic or comparison in views.** You cannot write `a + b = 15` or
  `i < j`. Instead, **tabulate the relation as data**. `moo.grmpl` scores
  cribbage by seeding `sum15two(a, b)` (all value pairs totalling 15) and
  `cardval(code, pips)`, then a view *joins* a hand against them:

  ```grmpl
  view crib_fif2(who) {
      slotpair(i, j)          // the 10 valid i<j slot pairs, as data
      card(who, i, c1)  card(who, j, c2)
      cardval(c1, a)    cardval(c2, b)
      sum15two(a, b)          // "a + b == 15", precomputed
      yield i, j
  }
  ```

  The *rules* are relations; scoring is a query; the host just counts the rows.

* **No inequality (`i ≠ j`) in a view.** Enumerate the valid combinations as a
  relation — `slotpair(i, j)` lists exactly the `i < j` pairs — and join through
  it.

* **No negation in a view.** "Things I do *not* know" isn't expressible as one
  join. Compose two views (all-present vs. acquainted) and take the difference
  where you consume them — that's how the MOO's "fog of identity" renders
  strangers.

* **No entity allocation in a behavior.** A handler moves and relabels existing
  entities; it cannot mint a new one. New entities come from a replay-safe
  allocator on the host/programmatic side (world construction: `dig`, `create`).

* **No fact literals.** A `.grmpl` file declares relations, views, and
  behaviors; initial *data* is committed by the host. Think of the file as the
  world's *laws* and the seed as its *initial conditions*.

* **No loops in a behavior.** One message produces one patch. For ongoing or
  fan-out activity, drive it with the clock: enqueue a periodic `tick` and let a
  behavior act once per tick. The MOO's cat is exactly this — its "AI" is a
  `Tick` handler that walks one `patrol` edge per tick:

  ```grmpl
  match Tick() {
      find located(self, loc)     // where am I?
      find patrol(loc, dest)      // my next room, from a data table
      expect located(self, loc)
      retract located(self, loc)
      assert located(self, dest)
  }
  ```

None of these is a missing feature so much as a nudge toward the relational way:
data over control flow, joins over arithmetic, editions over mutation.

---

## 10. Determinism & the invariants worth knowing

grmpl is deterministic by construction, and a few rules are load-bearing:

* **Least-match binding.** `resolve`/`find` bind the *least* matching tuple, so a
  program never depends on physical scan order.
* **Tuple-sorted reads, commit-ordered streams.** A relation read returns rows in
  tuple order; an update stream (`scan_updates`, a watch) returns them in the
  exact order they were committed. No `HashMap` iteration leaks into results.
* **One serialization.** There is a single value/tuple encoding; every artifact
  (messages, on-disk records) is framed on it and versioned by a format byte.
* **Patch–edition law.** A commit allocates the next edition *and* writes as one
  atomic step; there is no window where an edition exists but is unwritten.

The upshot for you as an author: the same program over the same history always
produces the same world, byte for byte — which is what makes as-of history,
replay, and forking trustworthy.

---

## 11. A guided read of `worlds/moo.grmpl`

Putting it together, the MOO uses every construct:

| Feature in the file                         | Language mechanism                              |
|---------------------------------------------|-------------------------------------------------|
| rooms, things, exits, the map               | `rel` + host-seeded data                        |
| "what's in my room" / "my inventory"        | `view here` / `view inventory` (joins)          |
| "coin value per room"                       | `view treasure` (a `sum` aggregate)             |
| the command line → a verb                   | `form command`                                  |
| `take` / `drop` / `go` / `say` / `greet`    | `on inbox parse command` (statement + concat)   |
| the **fog of identity** (learn names)       | `rel knows` / `label`, `view folks`/`acquainted`, the `greet` handshake |
| the wandering cat (an autonomous NPC)       | `rel patrol` + the `Tick` behavior              |
| reactive "the world changed" feed           | `on watch world including current`              |
| cribbage scoring                            | rule tables (`cardval`, `sum15two`, …) + the `crib_*` scoring views |
| the Observatory (a room that is a report)   | the host rendering aggregate views live         |

Run it and poke at it:

```sh
grmpl run                 # play the built-in MOO in a REPL
grmpl run worlds/moo.grmpl /tmp/mansion   # a file + a persistent store
grmpl showcase            # a narrated tour of the substrate's features
```

Inside the REPL, `help` lists the world's commands: `look`, `go north`,
`take lamp`, `greet cat`, `deal`/`cards`/`score`, `watch`, and more. Every one of
them is a view or a behavior in the file above — never a privileged engine
operation.

---

## Appendix: grammar sketch

```
program   := decl*
decl      := "rel"  Ident "(" collist ")"
           | "view" Ident "(" identlist? ")" "{" atom* "yield" yieldlist "}"
           | "form" Ident "{" rule* "}"
           | "on" Ident "parse" Ident "{" arm* "}"
           | "on" "watch" Ident ("including" "current")? "{" watchbind* "}"

col       := Ident (":" Ident)?
atom      := Ident "(" arg ("," arg)* ")"
arg       := Ident | Str | Int
yielditem := Ident | Ident "(" Ident? ")"          // group col | agg(col) / count()

rule      := patom+ "->" Ident "(" identlist? ")"
patom     := Str | Ident

arm       := "match" Ident "(" identlist? ")" ( "{" stmt* "}" | "[" word* "]" )
stmt      := "resolve" Ident "(" sargs? ")" "where" Ident ("="|"~") sarg
           | ("find"|"expect"|"assert"|"retract"|"emit") Ident "(" sargs? ")"
word      := "self" | "dup" | "drop" | "swap" | "over" | "rot" | "nip"
           | "tuck" | "dup2" | "drop2" | Str | Int
           | "resolve" Ident Ident ("="|"~")
           | "find" Ident Int
           | ("expect"|"assert"|"retract"|"emit") Ident
watchbind := ("inbox"|"cursor"|"seqs") Ident
```
