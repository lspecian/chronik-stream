# Agent Memory — Regression Protocol

Operational rules for handling baseline metric regressions, baseline updates,
and the weekly soak triage. Referenced by AM-1.8 (Testing Strategy).

## The contract

Every nightly LongMemEval-S run produces a metrics JSON. The
[`check_baseline`](../../crates/chronik-memory/tests/check_baseline.rs) test
diffs that JSON against
[`tests/baselines/longmemeval-s.json`](../../crates/chronik-memory/tests/baselines/longmemeval-s.json)
and fails when any gated metric drops more than `tolerance` (currently 0.05)
below the floor.

Failing the floor is **not** a release-blocker for PRs (they don't run the
nightly job), but it **is** a release-blocker for tagged release commits and
will open a GitHub issue for the soak job.

## When the floor fails

1. **Don't bypass the check.** Don't disable the nightly workflow, don't bump
   `tolerance`, and don't lower a floor metric without doing the work below.
2. **Open or re-open the tracking issue.** The nightly opens one automatically
   on failure. Reply with: which metric dropped, which commit introduced it
   (`git bisect` if not obvious), and the hypothesis.
3. **Decide one of the three responses**:

| Response | When | What to do |
|----------|------|------------|
| **Fix forward** | The regression is a bug or unintended side-effect. | Revert the offending change OR ship a correction that brings the metric back above floor. No baseline update. |
| **Accept the regression** | A deliberate trade-off (e.g. lowered a metric to fix a worse problem; switched providers; restructured pipeline). | Update the baseline (see below). Document the trade-off in the commit body. |
| **Quarantine** | The metric is unstable (provider drift, intermittent timeout) and not a real signal. | Move the metric out of `metrics` into a notes field; don't gate on it. Open a separate issue to make it stable. |

Fix-forward is the default. Acceptance is the rare path. Quarantine is the
last resort — only for metrics we don't trust.

## How to update a baseline

The baseline JSON ships in-repo. Updates require **both**:

1. A deliberate edit to
   [`crates/chronik-memory/tests/baselines/longmemeval-s.json`](../../crates/chronik-memory/tests/baselines/longmemeval-s.json).
   Change the value(s); update the `notes` to explain why.
2. A commit-message tag `baseline-update: <reason>` on a line by itself, e.g.

   ```
   prompt(extractor): switch to V6 — better factual grounding, lower abstain rate

   V6 trades a -0.04 drop on synth_judge for a +0.09 on raw_judge by removing
   the anti-abstention bias from the synthesis prompt. The net effect is
   correctness gain (we'd rather abstain correctly than fabricate).

   baseline-update: lower synth_judge floor 0.556 → 0.516 to absorb the trade
   ```

The nightly workflow doesn't auto-check the tag (it just reads the file), but
PR review must confirm the tag is present whenever the baseline file changes.
This is the human-in-the-loop guard against silently lowering the bar.

**Never raise the floor on a single good run.** A pilot can fluctuate. Wait
for 3 consecutive nightlies above the proposed new floor before locking it
in. The point of the floor is to catch regressions, not to celebrate spikes.

## Who owns what

| Channel | Owner | Cadence |
|---------|-------|---------|
| PR workflow failures | PR author | Per PR |
| Nightly baseline drops | On-call / area maintainer | < 24h response |
| Provider parity drift (warn-only) | Area maintainer | Weekly review |
| Weekly soak issue | On-call / area maintainer | < 48h response |
| Baseline updates | Area maintainer, with review | As needed |

## Soak triage

The soak job runs Sunday 04:00 UTC. On failure it auto-opens an issue tagged
`area/agent-memory` + `soak-failure`. Triage steps:

1. Pull the artifact (`soak-server-log`).
2. Look for: RSS growth, extraction lag growth, segment count blowup,
   ProduceHandler timeout patterns.
3. If reproducible locally: open a bug, link the soak issue, fix forward.
4. If transient (infra blip, killed runner): close the issue with a `flake`
   label and re-trigger via `workflow_dispatch`.

A flaky soak is a problem on its own — three flakes in a quarter means the
soak harness needs hardening, not the system under test.

## What we don't gate on

These metrics are reported but **not** in `metrics` (they're in
`per_category_synth_judge` or `notes`):

- **Per-category synth_judge** — `single-session-assistant` is a known
  structural 0/3 (extraction gap). Gating on it would force false-positive
  failures. Track at the aggregate level; fix the category by closing one of
  the three structural levers in the roadmap.
- **Raw substring rates** — substring matching is strictly inferior to
  LLM-judge for paraphrase-tolerant scoring. Reported for diagnostics only.
- **Latency** — gated in `bench_recall_latency` (separate test), not in the
  baseline JSON, because latency floors depend on hardware.

## Related

- Roadmap: [`docs/ROADMAP_AGENT_MEMORY.md`](../ROADMAP_AGENT_MEMORY.md) §
  Testing Strategy
- Baseline file: [`crates/chronik-memory/tests/baselines/longmemeval-s.json`](../../crates/chronik-memory/tests/baselines/longmemeval-s.json)
- Check script: [`crates/chronik-memory/tests/check_baseline.rs`](../../crates/chronik-memory/tests/check_baseline.rs)
- PR workflow: [`.github/workflows/agent-memory-pr.yml`](../../.github/workflows/agent-memory-pr.yml)
- Nightly workflow: [`.github/workflows/agent-memory-nightly.yml`](../../.github/workflows/agent-memory-nightly.yml)
- Soak workflow: [`.github/workflows/agent-memory-soak.yml`](../../.github/workflows/agent-memory-soak.yml)
