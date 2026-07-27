// steward-grmpl: grmpl-tuned reactive triage of a completed rat's branch.
//
// A repo-specific variant of `steward`, fired ONLY on grmpl completions by the
// `steward-grmpl-on-completion` trigger (scope: "grmpl"). It differs from the
// generic steward in exactly the ways grmpl's quality bar demands:
//
//   1. RUN GATE runs grmpl's REAL suite: `cargo test --workspace` (the generic
//      steward's `cargo test --quiet` does not exercise the whole workspace).
//   2. The reviewer runs on the STRONG model (not haiku). With no human at the
//      merge boundary, the reviewer is the only judgment before auto-merge to
//      main — reasoning about the seven laws is not a cheap-model task.
//   3. The reviewer is briefed on grmpl's INVARIANTS (the seven laws, the
//      format-policy, determinism) and must REWORK any diff that breaks them —
//      most importantly a wire/codec encoding change with no version-byte bump.
//   4. protectedPaths guards only infra a coding phase must never touch
//      (.github, .rk). Codec/wire SOURCE is deliberately NOT protected: grmpl
//      phases legitimately edit it, and the format-policy is enforced by the
//      reviewer's mandate above rather than a blanket path block (which would
//      force every encoding phase to escalate).
//   5. The diff-scope budget is a generous runaway BACKSTOP, not a routine
//      gate — grmpl roadmap phases are large. Exceeding it holds the branch for
//      a human (the one residual non-auto path — a bug-catch, not a gate).
//
// Every gate still fails CLOSED, and auto-merge is reached only through a clean
// policy gate + within-budget diff + green `cargo test --workspace` + an
// explicit APPROVE from the strong reviewer.
workflow: {
	name:        "steward-grmpl"
	description: "grmpl-tuned reactive triage: workspace-test gate + invariant-aware strong reviewer -> auto-merge / rework-ticket / escalate"

	params: {
		taskId: {type: "string", required: false, default: "unknown"}
		branch: {type: "string", required: false, default: ""}
		repo:   {type: "string", required: false, default: "grmpl"}
		target: {type: "string", required: false, default: "main"}
		// grmpl's REAL gate (CLAUDE.md): the whole workspace must be green.
		check: {type: "string", required: false, default: "cargo test --workspace"}
		// Guard only infra a coding phase must never touch. Codec/wire SOURCE is
		// intentionally absent — see header note 4; the reviewer enforces the
		// format-policy on those files instead.
		protectedPaths: {type: "string", required: false, default: "(^|/)(\\.github|\\.rk)/"}
		// Runaway backstop, not a routine gate: grmpl phases are large. A hit
		// holds the branch for a human rather than auto-merging a runaway diff.
		maxDiffFiles: {type: "int", required: false, default: 150}
		maxDiffLines: {type: "int", required: false, default: 8000}
		reviewTimeout: {type: "string", required: false, default: "25m"}
		gateTimeout: {type: "string", required: false, default: "30m"}
	}

	agents: {
		// STRONG model for both: the reviewer is the sole judgment before an
		// unattended merge to main, and grmpl's invariants are subtle.
		default:  {harness: "claude"}
		reviewer: {harness: "claude"}
	}

	steps: [
		// 1. The strong reviewer chains onto the completed branch.
		{
			type:   "spawn"
			role:   "reviewer"
			agent:  "reviewer"
			branch: _input.branch
			task: {
				title: "steward-review-" + _input.taskId
				description: """
					You are the steward's reviewer on branch {{ctx.activeBranch}},
					chained off a rat's completed work for grmpl: \(_input.taskId)

					Compare with: git log \(_input.target)..HEAD and git diff \(_input.target)...HEAD

					This branch WILL AUTO-MERGE to \(_input.target) on your APPROVE — no
					human sees it first. Hold grmpl's bar accordingly. Read CLAUDE.md
					and docs/ROADMAP.md, then verify the diff against grmpl's invariants:

					  * The seven laws (DESIGN §12): snapshot–stream, patch–edition,
					    replay determinism, one authority domain per commit, pattern=
					    matching, entities-as-values, reactivity-as-maintained-query.
					  * FORMAT POLICY: any change to a persisted or wire encoding
					    (grmpl-core/src/wire.rs, grmpl-store/src/codec.rs,
					    grmpl-transport/src/wire.rs, or any (de)serialization) MUST bump
					    the codec version byte AND state its migration in the phase
					    notes. A silent encoding change is an automatic REWORK.
					  * DETERMINISM: no HashMap-order results, no unlogged clock/random
					    reads, no first-match-of-unordered-set binding in new code.
					  * The bright line: no fjall detail above the store trait, no iroh
					    detail above the transport trait, no cross-crate layering breach.
					  * TESTS ARE THE SPEC: acceptance tests must be law oracles
					    (randomized churn + an invariant checked each round), not just
					    example-based unit tests.

					Decide APPROVE (clean, safe to auto-merge to \(_input.target)),
					REWORK (fixable issues remain — including any invariant breach
					above), or STOP (fundamentally wrong / needs a human call). When in
					doubt, do NOT APPROVE. Record the verdict before finishing:
					rk out artifact \(_input.repo) review --payload '{"task": "\(_input.taskId)", "recommendation": "APPROVE|REWORK|STOP", "notes": "..."}'
					"""
			}
		},
		{type: "wait", timeout: _input.reviewTimeout},
		{type: "evaluate", expect: {is_error: false}},

		// 2. POLICY GATE: protected-path diff fails closed (held for a human).
		{
			type: "run"
			command: "! git diff --name-only \(_input.target)...HEAD | grep -qE '\(_input.protectedPaths)'"
			timeout: "2m"
		},
		{type: "evaluate", expect: {exit: 0}},

		// 3. RUN GATE: grmpl's real workspace suite. Red fails closed.
		{type: "run", command: _input.check, timeout: _input.gateTimeout},
		{type: "evaluate", expect: {exit: 0}},

		// 4. Lift the reviewer's verdict.
		//
		//    `fromAgent` is load-bearing, not decoration (TKT-161/TKT-174). A
		//    steward is fired PER rat completion, so several instances can be in
		//    flight against one repo — and every one of them reads
		//    (artifact, <repo>, review). Without the binding "newest match wins"
		//    hands this steward whichever reviewer finished last, which may be a
		//    peer instance's, and the APPROVE arm below auto-merges a branch on
		//    a verdict passed on code it never looked at. Bound, the read matches
		//    only what the reviewer spawned in step 1 wrote (`rk out` stamps
		//    every payload with its writer's name), and fails CLOSED into
		//    `rk inbox` if that reviewer recorded nothing.
		//
		//    NOTE: this file has no version-controlled source — it lives only in
		//    the deployment dir, which is why the TKT-161 fix missed it for three
		//    days. TKT-178 places it; TKT-179 restores the two gates it also
		//    dropped (diff-scope, land-result). Do not hand-edit it further.
		{
			type:      "read"
			category:  "artifact"
			identity:  "review"
			fromAgent: true
			field:     "recommendation"
			into:      "verdict"
			timeout:   "5m"
		},

		// 5. Route. APPROVE is the only path to `land`.
		{
			type: "when"
			var:  "verdict"
			cases: {
				"APPROVE": [
					{type: "dismiss", noMerge: true},
					{type: "land", branch: "{{ctx.activeBranch}}", target: _input.target},
				]
				"REWORK": [
					{
						type: "run"
						command: "rk ticket new 'rework: \(_input.taskId)' --repo \(_input.repo) --body 'Steward routed REWORK on branch {{ctx.activeBranch}}. Read the reviewer notes: rk scan artifact \(_input.repo)'"
						timeout: "2m"
					},
					{type: "dismiss", noMerge: true},
				]
				"STOP": [
					{
						type: "run"
						command: "rk out need \(_input.repo) steward --payload '{\"agent\":\"steward\",\"task\":\"\(_input.taskId)\",\"text\":\"steward-grmpl: reviewer returned STOP for \(_input.taskId) on {{ctx.activeBranch}} — needs a human merge decision; branch held unmerged\"}'"
						timeout: "2m"
					},
					{type: "dismiss", noMerge: true},
				]
			}
			default: [
				{
					type: "run"
					command: "rk out need \(_input.repo) steward --payload '{\"agent\":\"steward\",\"task\":\"\(_input.taskId)\",\"text\":\"steward-grmpl: unrecognized review verdict for \(_input.taskId) on {{ctx.activeBranch}} — branch held unmerged, needs a human\"}'"
					timeout: "2m"
				},
				{type: "dismiss", noMerge: true},
				{type: "stop", reason: "unrecognized review verdict for " + _input.taskId},
			]
		},
	]
}
