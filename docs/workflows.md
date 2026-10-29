# Workflows

All modes run on the same event bus and save runs the same way (`~/.jockey/runs/<id>/`). Workflow code only sees `ProviderAdapter` and `AgentSession`, never provider names.

## Single

One agent, one turn per prompt. In the TUI, a conversation keeps one provider session across turns. Enter during a turn steers the agent live.

```sh
jockey run --agent claude:sonnet@high "add a --verbose flag"
```

## Cascade

```
primary attempt ─► checks ─► judge (read-only)         ─► accepted? ─► done
                                                        └─ no ─► retry with feedback (fallback, or the primary) ─► checks ─► judge … (≤ max_attempts)
```

1. **Primary** works in the real workspace under the run's permission profile.
2. **Checks.** Each command in `[cascade] checks` runs in the workspace with `sh -c`, with a timeout. A check can't be overridden by the judge: one failing check means the attempt is not accepted.
3. **Judge.** It runs as a **reviewer**, which means two things:
   - The provider's read-only mode is forced: Codex `sandbox: read-only`, Claude `--tools Read,Grep,Glob` with `dontAsk`, and Grok rejects every permission request.
   - It reads the real workspace, since the primary has finished. If nothing changed, it gets an empty folder instead: the answer and diff in its prompt are all there is to review.

   The judge receives the task, the agent's answer, the reported diffs and the check output, all fenced as data. It must reply with:
   ```json
   {"pass": true, "score": 0-100, "issues": ["..."], "evidence": ["file:line or check output"], "recommended_action": "accept|revise|escalate"}
   ```
   If the reply has no parseable JSON, the judge gets one retry; after that the attempt counts as not accepted.
4. **Accepted** only when every check passes, `pass` is true and `score >= threshold` (default 85).
5. **Escalation.** The retry goes to the fallback agent if one is set, otherwise back to the primary in its own session. Either way it gets the original task, the previous answer, the judge's issues and evidence, and any failing check output. It continues from the current workspace. The retrying agent keeps one session across later attempts.
6. **Cap.** Attempts stop at `max_attempts` (default 2: the primary plus one retry). The final status is `failed` if no attempt was accepted, and the last attempt's work stays in the workspace.

There is no built-in ranking of providers, so the judge must be chosen explicitly. The fallback is optional (`/fallback off` in the TUI, or leave it out):

```sh
jockey run --mode cascade --agent grok:grok-4.6@low --judge claude:haiku --fallback codex "optimize this code"
```

or in config:

```toml
[cascade]
judge = "claude:sonnet"
fallback = "codex:gpt-6.1-sol@high"
threshold = 85
max_attempts = 2
checks = ["cargo test", "cargo clippy -- -D warnings"]
check_timeout_secs = 600
```

The **TUI** has the same mode: `/mode cascade`, `/judge <spec>`, `/fallback <spec>`, or `jockey --mode cascade --judge … --fallback …`. Check results, judge verdicts (score, issues, and evidence with Ctrl+O) and escalations appear in the conversation. Esc cancels the whole cascade and kills every agent it started. The primary agent's conversation carries over between prompts and across mode switches. Judges, critics and fallbacks start fresh for each prompt and see only the current request, so their reviews stay independent.

Events: `CheckCompleted`, `JudgeResult` (attempt, pass, score, accepted, issues, evidence, recommended_action) and `Escalated` (from, to, reason).

**Checks come from config.** A repository's `.jockey.toml` may define `[cascade] checks`, and they run like any build script in that repository. The commands are listed in the run's events before their output.

## Critique

```
primary draft ─► critics in parallel (each its own focus) ─► merge + rank ─► primary revises ─► … ─► checks
```

1. **Primary** drafts in the real workspace and keeps one session throughout.
2. **Workspace.** Critics read the real workspace in read-only mode; the primary has finished, so they all see the same state. If nothing changed, they get an empty folder instead.
3. **Critics** run concurrently, at most `max_parallel_per_provider` at a time per provider. Each critic is a read-only reviewer with a focus role: `correctness`, `security` or `performance`, cycled by default. Each must reply with:
   ```json
   {"findings": [{"severity": "critical|high|medium|low", "confidence": 0.0-1.0, "description": "...", "evidence": "...", "proposed_fix": "...", "file": "path", "line": 12}]}
   ```
   A critic with no parseable JSON gets one retry. After that, its failure is recorded and the other critics' findings are still used.
4. **Merge.** Duplicates are folded when they're in the same file at nearby lines, or have similar wording. A merged finding keeps the highest severity and confidence and lists every critic that reported it. Findings are ranked by severity, then confidence.
5. **Revision.** The merged findings go back to the primary as suggestions only: they can't change its permissions or the task. With no findings, the round ends without a revision.
6. **Checks** in `[critique] checks` run at the end, and a failing check fails the run. There's one review and revise round by default (`revisions`, capped at 5).

**Choosing what goes back (TUI).** Turn on the setup menu's Review row ("ask me first") or set `[critique] ask = true`, and the critique pauses after each review. You pick **all** findings, **only one critic's**, or **nothing**, which stops without a revision. Esc puts off the choice; Ctrl+S reopens it. Each finding in the supervisor pane lists the critics that reported it.

**Reading critiques.** In multi-agent modes the critic results appear in the right-hand supervisor pane. The mouse wheel scrolls whichever pane it's over, and Alt+PgUp/PgDn also scrolls the supervisor pane. Ctrl+Z cycles zoom through primary only, supervisor only and both, Ctrl+\ stacks the panes, and Ctrl+O shows each finding's evidence and proposed fix.

**Critics default to the primary's model** (`count` copies, default 3). To change that, use `--critics a,b,c`, `[critique] critics = [...]` or, in the TUI, the setup menu's Critics row ("Same as primary" or any lab › model › thinking).

```sh
jockey run --mode critique --agent claude:sonnet "implement the parser"
jockey run --mode critique --agent codex --critics claude:haiku,grok:grok-4.6@low "review this implementation"
```

```toml
[critique]
critics = []            # empty = `count` × the primary's model
count = 3
roles = ["correctness", "security", "performance"]
revisions = 1
checks = ["cargo test"]
max_parallel_per_provider = 3
ask = false             # TUI: pause after each review to pick whose findings go back
```

Events: `ReviewTriggered`, `CritiqueReceived` (per critic: findings or error), `FindingsMerged` (round, ranked findings, duplicates).

## Choosing modes and models in the TUI

`/mode` (or Enter on it in the `/` menu) opens a mode menu. Choosing **cascade** or **critique** opens that mode's **setup menu**, which lists every role with its current model:

- cascade: Primary, Judge, Fallback, Done
- critique: Primary, Critics (default "3× same as primary"), Done
- autosteer: Primary, Critics (default "1× same as primary"), Count, Policy, Done (critique also has Count)

Picking a role goes through lab › model › thinking and returns to the setup menu. Done checks that required roles are set (cascade's fallback is optional; Enter on its row turns it on or off). In a multi-model mode, Ctrl+L reopens the setup menu. A slash command typed without an argument opens its menu: `/judge`, `/fallback` and `/critics` open their role's lab picker, and `/model`, `/effort` and `/profile` open theirs. With an argument, they set the value directly (`/judge claude:haiku`). The footer shows the mode with its roles, for example `critique (3× same as primary)` or `cascade (judge claude-haiku-5-5, fallback codex:gpt-6.1-sol)`.

## Autosteer

Critics review the primary's progress at checkpoints while it works, and a steering controller decides, per policy, whether to observe, suggest, steer live, queue, or reject each finding. The manual `suggest` policy leaves every correction for you to send (Ctrl+S). See [autosteer.md](autosteer.md) for checkpoints, the controller's rules, policies and a worked example.
