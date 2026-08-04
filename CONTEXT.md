# grmpl world runtime

grmpl derives, watches, and patches durable versioned worlds. These terms name
the public execution model shared by every client adapter.

## Language

**World program**:
A compiled `.grmpl` description containing relation schemas, views, forms, behaviors, and reactive watches.
_Avoid_: Script, rules file

**World runtime**:
The public module that binds one world program to one durable world store and owns its process, sequence, query, and watch lifecycle.
_Avoid_: Session engine, host runtime

**World adapter**:
An edge module that translates terminal, TCP, or embedded Rust interaction into world-runtime operations without owning world lifecycle.
_Avoid_: Frontend engine, separate runtime

**Native world capability**:
Deterministic Rust behavior used when the world program cannot yet express an operation, such as entity allocation or formatted presentation.
_Avoid_: Privileged verb, engine special case
