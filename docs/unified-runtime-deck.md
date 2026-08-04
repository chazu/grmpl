---
marp: true
html: true
theme: default
paginate: true
size: 16:9
title: One grmpl Runtime
description: How grmpl moved from divergent entry points to one runtime surface
footer: grmpl unified runtime · 2026-08-04
style: |
  :root {
    --ink: #172033;
    --muted: #5f6b7a;
    --accent: #0f766e;
    --accent-soft: #dff7f3;
    --warm: #fff4db;
    --line: #cbd5e1;
  }
  section {
    color: var(--ink);
    font-family: Inter, Avenir, "Helvetica Neue", Arial, sans-serif;
    padding: 54px 64px;
  }
  section::after {
    color: var(--muted);
    font-size: 16px;
  }
  h1, h2 { color: var(--ink); letter-spacing: -0.025em; }
  h1 { font-size: 52px; }
  h2 { font-size: 38px; margin-bottom: 24px; }
  p, li { font-size: 24px; line-height: 1.35; }
  strong { color: var(--accent); }
  code { font-size: 0.86em; }
  pre { border: 1px solid var(--line); border-radius: 10px; }
  table { font-size: 19px; }
  th { background: var(--accent); color: white; }
  td, th { padding: 9px 12px; }
  .subtitle { color: var(--muted); font-size: 28px; }
  .eyebrow { color: var(--accent); font-size: 18px; font-weight: 700; letter-spacing: .12em; text-transform: uppercase; }
  .grid { display: grid; grid-template-columns: 1fr 1fr; gap: 28px; }
  .card { border: 1px solid var(--line); border-radius: 14px; padding: 20px 24px; }
  .card h3 { color: var(--accent); margin: 0 0 10px; font-size: 25px; }
  .card p, .card li { font-size: 20px; }
  .good { background: var(--accent-soft); }
  .warm { background: var(--warm); }
  .small { color: var(--muted); font-size: 18px; }
  .center { text-align: center; }
---

<p class="eyebrow">Architecture change</p>

# One grmpl runtime

How terminal play, TCP clients, and embedded Rust now share the same world

<p class="subtitle">A six-slide introduction for people new to the project</p>

---

## What grmpl is—and what went wrong

<div class="grid">
<div class="card good">
<h3>A durable world model</h3>
<p>grmpl is a <strong>durable relational world runtime</strong>.</p>
<p>Facts store world state. Queries derive what is true. Behaviors produce atomic changes. Every edition remains replayable.</p>
<p>The built-in <strong>MOO</strong> is a small multi-user object world with rooms, people, objects, commands, and watches.</p>
</div>
<div class="card warm">
<h3>Two paths had diverged</h3>
<ul>
<li><code>grmpl run</code> used the language-defined MOO.</li>
<li><code>grmpld</code> used a second, hard-coded Rust world.</li>
<li>Relation IDs, schemas, counters, verbs, and tests could drift.</li>
</ul>
</div>
</div>

Names like `grmpl-lang`, `grmpl-proc`, and `grmpl-diff` added confusion. They are focused internal libraries—not separate user products—but the split entry points made them look like competing surfaces.

---

## The new shape: one deep runtime, several thin adapters

```text
                ┌────────────────────────────┐
Terminal ──────►│                            │
TCP ───────────►│  grmpl::MooRuntime         │
Embedded Rust ─►│  over grmpl::Runtime       │
                │                            │
                └─────────────┬──────────────┘
                              │
             compile · schemas · queries · processes
             inbox sequences · watches · retry policy
                              │
                ┌─────────────▼──────────────┐
                │ Internal semantic modules │
                │ + durable Ent store       │
                └────────────────────────────┘
```

**Adapters translate input and output. The runtime owns the world lifecycle.**

---

## What changed in the code and command line

<div class="grid">
<div class="card good">
<h3>Added and unified</h3>
<ul>
<li>Public library crate: <code>grmpl</code></li>
<li>General <code>grmpl::Runtime</code></li>
<li>Built-in <code>grmpl::MooRuntime</code></li>
<li>Transport-neutral player <code>Server</code></li>
<li>Dynamic relation lookup from the durable catalog</li>
<li>Shared schema, sequence, process, and watch setup</li>
</ul>
</div>
<div class="card warm">
<h3>Simplified surface</h3>
<pre><code>grmpl run …       terminal adapter
grmpl serve …     TCP adapter
grmpl showcase    technical tour</code></pre>
<p>The separate <code>grmpld</code> binary and hard-coded session world were removed.</p>
</div>
</div>

The semantic crates remain separate because those boundaries improve testing and locality; they no longer imply separate product runtimes.

---

## Where MOO behavior lives now

| Defined in `worlds/moo.grmpl` | Native Rust capability—for now |
|---|---|
| Typed world relations and schemas | Open the durable store and compile a package |
| Relational views and aggregates | Open the store and grant package capabilities |
| Command grammar | Provision durable player/login identity |
| `take`, `drop`, `go`, `say`, `greet`, `dig`, `create` | Formatted `look` and other presentation |
| NPC patrol behavior | Drive processes and retry contested commits |
| Reactive world watch | Terminal/TCP I/O, card RNG, DSP vault instances |

The goal is not “all Rust” or “all language.” It is a clear rule:

> Portable world rules belong in `.grmpl`; deterministic platform powers are granted through the runtime; presentation stays in adapters.

---

## Confidence now—and the path to Shotengai

<div class="grid">
<div class="card good">
<h3>What is proven</h3>
<ul>
<li>Full workspace build, tests, and clippy pass</li>
<li>Real loopback TCP acceptance tests</li>
<li>Concurrent allocation and one-winner races</li>
<li>Durable reconnect and reactive resume</li>
<li>Deterministic replay and fork checkpoints</li>
<li>Atomic package bootstrap, float, allocation, RNG, and stored-code laws</li>
<li>Durable static actors, canonical timer driving, and restart-safe combat</li>
</ul>
</div>
<div class="card warm">
<h3>Phases 1–2 landed; next</h3>
<ol>
<li>Authority-scoped DSP and fork effects</li>
<li>Deterministic collection/shuffle support</li>
<li>Richer presentation values</li>
</ol>
</div>
</div>

Manor and Shotengai now install through the same v4 package path. Shotengai's patrol and combat continuation use package actors; its remaining native seams are phase-3 structural, collection, and presentation work—not another bootstrap path or engine.

<p class="small">Capability report: docs/RUNTIME-CAPABILITIES.md · Remaining-work design: docs/WORLD-PACKAGE-REMAINING-WORK.md</p>
