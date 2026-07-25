# A full session with the grmpl MOO

A real, unedited playthrough of `grmpl run` (the built-in [`worlds/moo.grmpl`](../worlds/moo.grmpl)), captured by piping the commands below into the binary. Every response is produced by the world's own `view`s and behaviors — the host only prints them.

> Reproduce it yourself:
>
> ```sh
> cargo run -p grmpl-cli -- run
> ```

---

```text
grmpl — running the built-in MOO
A manor of six rooms, a wandering cat, a merchant, and a deck of cards.
Type `help` for commands, `quit` to leave.

You are in the Foyer.
You see: a cat, brass lamp, iron key.
Exits: east, north, up, west.


> look
You are in the Foyer.
You see: a cat, brass lamp, iron key.
Exits: east, north, up, west.

> watch
Now watching the world — changes will stream as [world] deltas.

> greet cat
You introduce yourself. You are now acquainted with Whiskers.
Whiskers pads out of the room.

> look
You are in the Foyer.
You see: brass lamp, iron key.
Exits: east, north, up, west.

> take lamp
Taken.
  [world] - brass lamp

> take key
Taken.
  [world] - iron key

> inventory
You are carrying: brass lamp, iron key.

> go north
You go.

> look
You are in the Library.
You see: old book.
Exits: south.

> take book
Taken.
Whiskers slinks into the room.
  [world] - old book

> go south
You go.

> who
1 online: you

> go east
You go.
Whiskers pads out of the room.

> look
You are in the Garden.
You see: red rose.
Exits: south, west.

> take rose
Taken.
  [world] - red rose

> go south
You go.

> look
You are in the Market.
You see: a merchant, gold coin.
Exits: north.

> greet merchant
You introduce yourself. You are now acquainted with Bartleby.

> take coin
Taken.
Whiskers slinks into the room.
  [world] - gold coin

> inventory
You are carrying: brass lamp, gold coin, iron key, old book, red rose.

> treasure
Treasure per room (aggregate view — sum):
  Kitchen: 2 coins

> go north
You go.

> go west
You go.
Whiskers pads out of the room.

> go up
You go.

> look
You are in the Observatory. A great orrery mirrors the whole world:
  · 2 coins of treasure lie scattered about the manor.
  · 2 souls wander the halls.
  · Whiskers prowls the Garden.
  · 10 passages thread the six rooms.
(Every line above is a live query over the world — the room IS a view.)

> go down
You go.

> deal
Dealt a fresh hand. `cards` to see it, `score` to count it.

> cards
Hand: Q♦ 7♥ 4♦ 8♠   |   starter: 10♣

> score
Hand: Q♦ 7♥ 4♦ 8♠   |   starter: 10♣
Score (counted from the scoring views):
  fifteens: 1 × 2 = 2
  pairs:    0 × 2 = 0
  ── total: 2

> deal
Dealt a fresh hand. `cards` to see it, `score` to count it.

> cards
Hand: 8♠ 5♠ 6♥ 10♠   |   starter: A♠

> score
Hand: 8♠ 5♠ 6♥ 10♠   |   starter: A♠
Score (counted from the scoring views):
  fifteens: 2 × 2 = 4
  pairs:    0 × 2 = 0
  ── total: 4

> say hello
hello
Whiskers slinks into the room.

> inventory
You are carrying: brass lamp, gold coin, iron key, old book, red rose.

> quit
Goodbye.
```

---

## What just happened (annotations)

- **`greet cat`** — the cat was shown as *"a cat"* under the fog of identity; greeting it asserts `knows(you, cat)` and `knows(cat, you)` (mutual), so from then on it is *Whiskers*. That whole handshake is the `Greet` behavior in the world file.
- **Whiskers moving on its own** — every turn the host enqueues a `tick` to the cat, and its `Tick` behavior walks one edge of the `patrol` relation. The "pads out / slinks into" lines are just the host narrating a location change it observed; the *movement* is the behavior.
- **`[world] - ...` / `[world] + ...`** — after `watch`, the `on watch world` reactive handler streams each change to the derived `world` view as a signed, exactly-once activation (a thing leaving the floor is a `-`, returning is a `+`).
- **`treasure`** — a `sum` aggregate view, recomputed on read: once you pocket a coin its value leaves the room total.
- **The Observatory** — its description is four live queries over the whole world (treasure total, people, the cat's room, passage count). The room *is* a view.
- **`deal` / `cards` / `score`** — the hand is stored as `card(you, slot, code)` facts; `score` runs the `crib_fif2` / `crib_pairs` **views** over the seeded rule tables and the host merely counts the rows they return.

