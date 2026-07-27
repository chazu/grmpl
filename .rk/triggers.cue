// grmpl repo-local reactor triggers. Loaded by the daemon for the grmpl repo
// only (reactor.trigger_files scans every registered repo's .rk/triggers.cue).
// Kept HERE rather than in the global ~/.rat-kingdom/triggers/ folder because
// this trigger is grmpl-specific — it fires grmpl's own tuned steward, and
// putting it in the global folder made it standard config it never should have
// been (see grmpl-local steward-grmpl.cue, same reasoning).
triggers: [
	{
		name: "steward-grmpl-on-completion"
		// grmpl-only: real workspace-test gate + invariant-aware strong reviewer.
		match: {category: "event", identity: "harness_result", search: "\"role\":\"rat\"", scope: "grmpl"}
		run: "steward-grmpl"
		params: {
			taskId: "{{tuple.payload.task}}"
			branch: "{{tuple.payload.branch}}"
			repo:   "{{tuple.scope}}"
		}
		// Serial sweep runs one grmpl ticket at a time, so one review per
		// completion is correct. Capped at 1 (was 20) to stop the reactor
		// re-firing on every tick for stale matches still in the rolling
		// window — the storm that spawned 3-5 duplicate reviewers per ticket
		// plus a phantom rat on already-landed TKT-127. If a legit review is
		// ever starved, kick it manually: rk workflow run steward-grmpl.
		maxFires: 1
	},
]
