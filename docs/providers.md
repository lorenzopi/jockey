# Providers

Phase 0 research ran on 2026-10-28 against the installed CLIs, using subscription sign-ins only. Each provider's details, real JSON samples and command lines are in [research/](research/).

**V** = verified by running it. **D** = documented only. **✗** = unsupported. **~** = partial.

| | Codex 0.165.0 | Claude Code 2.1.312 | Grok 1.0.70 | Gemini 0.46.0 |
|---|---|---|---|---|
| Interface jockey uses | `codex app-server` (JSON-RPC) | `claude -p --input-format stream-json --output-format stream-json` + control requests | `grok agent stdio` (ACP) | **unusable**: sign-in rejected |
| Stream events | V | V | V | D |
| Mid-run input | V `turn/steer` | V stdin user message, delivered at the next tool boundary | V `_x.ai/interject` | ✗ (a second prompt cancels the turn) |
| Steering ack | V response + `userMessage` item carrying our client id | V `command_lifecycle` queued/started/completed (undocumented) | ~ `{"status":"queued"}`; no delivered event, and the message is lost if the turn ends first | ✗ |
| Interrupt | D `turn/interrupt` | V `interrupt` control request | V `session/cancel` | D `session/cancel` |
| Approvals to jockey | D server requests | V `--permission-prompt-tool stdio` → `can_use_tool` | V `session/request_permission` (rejecting ends the turn) | D |
| Read-only mode | D `sandbox: read-only` | V tool allowlist + `dontAsk` + `--safe-mode` | ~ only via a `--tools` allowlist; `dontAsk` still writes; `--sandbox` breaks on OrbStack | D (plan mode can switch itself to YOLO) |
| Resume | D `thread/resume` | V `--resume`/`--fork-session` | V `session/load`, `-r` | D |
| Model list | V `model/list` | V via `initialize` only | V `grok models` | D |
| Effort | V per turn | D | V `--effort`, `set_config_option` | settings only |
| Usage / quota | V `account/rateLimits/*` | V `rate_limit_event`, `get_usage` | V usage events, `_x.ai/billing` | D (no remaining-quota readout) |
| Native commands | review, compact, fork | compact, todos, subagents | compact, context, deep-research, goal, … | D |
| Paid-fallback risk | **real**: billed credits silently when the window was exhausted (see codex.md) | overage switches back on if credits are topped up; Fable models and fast mode can draw credits | pay-as-you-go or prepaid credits, or `XAI_API_KEY` | Google One AI credits (`billing.overageStrategy`) |
| Adapter in this build | **yes** | **yes** | **yes** | blocked |

## Steering in this build

| | Acknowledged when | Applied when | Missed the turn |
|---|---|---|---|
| Codex | `turn/steer` returns | a `userMessage` item carrying our client id appears | turn completes without that item |
| Claude Code | `command_lifecycle: queued` | `command_lifecycle: started` (or the replayed echo) | the turn's `result` arrives and the message never started |
| Grok | `_x.ai/interject` returns `queued` | **inferred**: the next completed tool call (Grok emits no live delivery event) | the turn ends with no tool boundary after the ack |

In every case, a correction that missed its turn is reported as `SteeringRejected` with the `turn ended before it was applied` reason. The TUI then re-sends it as the next prompt.

Live checks on 2026-10-28:
- **Grok:** a correction sent 4 s into a three-read turn was acknowledged, marked applied after the first read, and followed ("BANANA").
- **Claude Code:** the run finished before the correction arrived, so it went in as a follow-up. Mid-turn delivery is covered by replaying Claude's real recorded trace.
- **Codex:** verified in the Phase 0 probe.

Paid-usage guards, enforced before each session or turn:
- **Codex:** subscription window exhausted means refuse; a falling credit balance during a turn means interrupt.
- **Claude Code:** `extra_usage`/`spend` enabled means refuse; overage reported during a turn means interrupt; Fable models require `--allow-credits`.
- **Grok:** a pay-as-you-go cap, prepaid balance, auto top-up or monthly limit means refuse.

All three adapters strip API-key environment variables from the child process.

## Known provider limits

- **Gemini CLI.** Google ended Gemini CLI sign-in for individual and AI Pro/Ultra accounts on 2026-06-18; every call fails with `IneligibleTierError`, and the Homebrew formula is disabled on 2026-12-18. The suggested replacement is Antigravity CLI (`agy` 1.4.4, installed), which offers `--input-format stream-json`, `--effort`, `--mode plan` and `agy models`. It hasn't been researched yet, including its terms.
- **Grok approvals.** `~/.grok/config.toml` sets always-approve, and ACP sessions inherit it. The adapter must pass `_meta: {"yoloMode": false}` on `session/new`.
- **Claude Code steering label.** Critic messages must not carry `origin: {kind: "human"}`.

## Terms (summary; see each research file for quotes and links)

- **Claude Code:** an end user may sign in to the unmodified binary with their own subscription. Third parties must not offer claude.ai login or handle users' credentials. Local personal use fits; distributing jockey commercially would need API keys or Anthropic's approval.
- **Grok:** xAI's docs advertise headless and ACP use from other apps. The Acceptable Use Policy's "unauthorized automated" clause couldn't be fetched, so you should read it yourself.
- **Codex:** the app-server is the documented integration surface for IDE clients. The ChatGPT sign-in stays inside the Codex process and jockey never touches the tokens. (Jockey answers `account/chatgptAuthTokens/refresh` with an error.)
