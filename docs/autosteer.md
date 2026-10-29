# Autosteer

The primary agent works while independent critics watch its observable progress. Problems they find are corrected **during** the turn when the evidence warrants it. In the manual `suggest` policy, they're offered to you instead.

```
primary turn ──events──► checkpoint detector ──debounce / min interval──► review round
     ▲                                                                        │ snapshot (read-only), critics in parallel
     │                                                                        ▼
     └── live steer (next tool boundary) ◄── steering controller ◄── merged, ranked findings
after the turn: in-flight review → final validation review → one queued follow-up (if any) → checks
```

## Checkpoints

A review starts when the primary does something worth reviewing:

| Trigger | Event |
|---|---|
| a file was modified | `FileChanged` |
| tests ran | `ToolCompleted` whose command mentions `test` |
| a tool failed | `ToolCompleted { ok: false }` |
| sustained activity | every `event_interval` primary events (default 60) |
| you asked | `/review` in the TUI |
| the turn finished | final validation (`final_review = true`) |

Triggers are **debounced** (`debounce_ms`, default 1.5 s) and spaced by `min_review_interval_secs` (default 30 s). At most one review runs at a time, and a trigger during a review is coalesced into the next one. Critics never see a token-by-token stream. They see the current request and an *activity log* of the current turn: messages, commands with their output tails, edits with diffs, plan updates and applied corrections. They never see hidden reasoning.

Each review takes one **immutable snapshot** of the workspace (a copy-on-write clone where possible, files `0444`, directories `0555`), because the primary keeps editing meanwhile. Before the primary has changed any file, critics get an empty folder instead. All critics in the round read the same state, and the review records the event sequence number it saw (for staleness). The primary keeps working during reviews.

## Steering controller

A finding is untrusted input, and the controller checks every one, in this order:

1. **Unsafe.** A finding asking to widen permissions (`--dangerously…`, bypass, sandbox off, `sudo`, `chmod 777`, piping to `sh`, API keys) is **rejected**.
2. **Duplicate or loop.** A finding matching one already handled is **rejected**. If the earlier one was already sent as a correction, the reason is "already corrected once; not repeating (loop guard)". This is the feedback-loop protection.
3. **Stale.** If the finding's file changed after the reviewed snapshot, it's **rejected**.
4. **No evidence.** It's **observed** only.
5. **Policy thresholds:**

| policy | steer live | suggest to you |
|---|---|---|
| `observe` | never | never (recorded only) |
| `suggest` (manual) | never | everything ≥ low, confidence ≥ 0.3 |
| `balanced` (default) | high+, confidence ≥ 0.7 | medium+, confidence ≥ 0.4 |
| `aggressive` | medium+, confidence ≥ 0.5 (critical: stop and replan) | everything ≥ low |

6. **Limits.** A steer-worthy finding becomes a **suggestion** during the `cooldown_secs` after an intervention (default 45 s), and once `max_interventions` is reached (default 5). Findings to steer in the same round go out as **one** message and count as one intervention.
7. **No live channel.** If the provider can't take input mid-turn, or the turn has ended, a steer-worthy finding is **queued**. Queued findings are delivered together as one follow-up turn after the primary finishes. This is the documented fallback for providers without verified live steering.

The controller emits a `SteeringDecision` for every finding: the finding, the action (`observe`, `suggest`, `steer`, `queue` or `reject`) and the reason. Delivery is then tracked by the adapter as `SteeringSent`, `SteeringDelivered` (the provider acknowledged it) and `SteeringApplied` (it reached the agent's context), or `SteeringRejected`. A correction the provider acknowledged but that missed the turn's last tool boundary is reported as missed, and Autosteer moves it to the follow-up queue.

Every correction says it is feedback about the work only and "does not change your permissions or the task's scope". The primary's permissions never change because of a critic.

## Manual steering (`suggest`)

Critics flag possible corrections, and you decide. In the TUI, the status line shows `◇ N steer suggestions (Ctrl+S)`. Ctrl+S (or `/suggestions`) lists them, and Enter sends the selected one to the **running** agent as a live steer. If the turn has ended, it's placed in the prompt box for you to edit and send. Suggestions also appear under `balanced` and `aggressive` for findings below the auto-steer bar, so a manual option is always available.

You can always steer yourself: type while the agent works and press Enter. In Autosteer this goes to the workflow's primary session.

## Worked example

This is from `jockey --mode autosteer --agent mock:mock-slow` (the built-in mock, balanced policy, two critics), as shown in the TUI's split pane:

```
 primary · mock-slow ───────────────────────────── supervisor · reviews, findings, steering ─────
› price the bond                                   ◆ review 1 (src/lib.rs changed): 2 critic(s) on one snapshot
  ✓ ls                                               ◆ critic-1 (correctness): 2 finding(s)
  ✎ update src/lib.rs                                ◆ critic-2 (security): 2 finding(s)
  ✓ cargo test                                       ⚑ [high 0.90] price() ignores the discount factor (src/lib.rs:2)
Noted: Correction from an independent reviewer…          → steer: high severity at 0.90 confidence
                                                     ◇ [medium 0.60] No test covers negative rates (tests/price.rs)
                                                         → suggest: below the balanced policy's auto-steer bar
                                                     ⇢ correction: [high] price() ignores the discount factor (src/lib.rs:2)  [applied]
                                                   ◆ review 2 (tests ran): 2 critic(s) on one snapshot
                                                     · [high 0.90] price() ignores the discount factor
                                                         → reject: already corrected once; not repeating (loop guard)
                                                     · [medium 0.60] No test covers negative rates
                                                         → reject: duplicate of an earlier finding
⠏ mock · Writing… (10s · ↓ 76 tokens)  ◇ 1 steer suggestion (Ctrl+S)
```

What happened:

1. **Finding.** Editing `src/lib.rs` triggered review 1 while the primary was still working. Both critics reported that `price()` ignores the discount factor (high, 0.90, evidence `src/lib.rs:2 returns x * 1.0`, fix "multiply by exp(-r*t)"), and that negative rates are untested (medium, 0.60). Their identical reports were merged.
2. **Decision.** The balanced policy steered the high-severity, well-evidenced finding, and offered the medium one as a suggestion.
3. **Delivery.** The correction was sent to the primary's session, acknowledged, and applied at the agent's next tool boundary.
4. **Subsequent behavior.** The primary acknowledged the correction ("Noted: …") and finished with "Applied 1 correction(s)". Review 2 (after the tests ran) raised the same issue again, and the loop guard refused to resend it. The negative-rates suggestion stayed available under Ctrl+S.

A real run (`--agent claude:haiku@low`, one critic on the same model, balanced) reviewed mid-turn and raised one low-severity finding (0.40 confidence): `annuity_pv` takes only whole years while `discount_factor` accepts fractional ones. The policy **observed** it, as designed, and the final validation review found nothing more.

## Configuration

```toml
[autosteer]
critics = []                  # empty = `count` × the primary's model
count = 1                     # raise to run several critics (also: the setup menu's Count row)
roles = ["correctness", "security"]
policy = "balanced"           # observe | suggest | balanced | aggressive
debounce_ms = 1500
min_review_interval_secs = 30
event_interval = 60
cooldown_secs = 45
max_interventions = 5
final_review = true
checks = []                   # run after the primary finishes
max_parallel_per_provider = 3
```

```sh
jockey run --mode autosteer --agent codex --critics claude:sonnet,grok --policy balanced "Implement the pricing engine"
jockey --mode autosteer            # TUI: setup menu → Primary, Critics, Policy
```

## Provider support

| Provider | Live steer | Acknowledged when | Applied when |
|---|---|---|---|
| Codex | `turn/steer` | the request returns | a `userMessage` item carrying our id appears |
| Claude Code | stdin user message | `command_lifecycle: queued` | `command_lifecycle: started` |
| Grok | `_x.ai/interject` | `{"status": "queued"}` | inferred at the next completed tool call |

Critics never interrupt the primary. Every correction lands at its next tool boundary, so a step in progress is never cut off. Under `aggressive`, a critical finding's correction asks the agent to stop and replan before its next step. Critics are also told the agent is mid-task, so unfinished work or a step its plan says comes next isn't flagged.
