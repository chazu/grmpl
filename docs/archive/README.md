# Archive

Finished plans and documents that describe code no longer in the tree. Kept for
history; nothing here describes the current system.

* `ENT-MIGRATION-PLAN.md`, `ENT-GAPS-PLAN.md` — the plans that replaced the
  `grmpl-store` fjall LSM with the Ent. Both are complete.
* `PERFORMANCE.md` — measurements of the deleted `grmpl-store`. Current numbers
  are in [`../PERFORMANCE-ENT.md`](../PERFORMANCE-ENT.md).
* `p9c-delta-stream-patterns.md` — the design for windowed delta streams and
  differential stream parsing in `grmpl-diff`, removed because it was never
  wired into the language and kept its state outside the Ent.
