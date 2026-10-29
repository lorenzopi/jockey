# Codex CLI research (Phase 0)

Installed: `codex-cli 0.165.0`, signed in with ChatGPT. Checked on 2026-10-28.

## Interface choice: `codex app-server`

`codex exec --json` is one-shot: it has no way to send input mid-run, approvals can't reach the client, and it exposes no model list or rate limits. `codex app-server` (JSON-RPC 2.0, line-delimited, stdio) has all of these. The schema comes from `codex app-server generate-json-schema --out DIR` (106 client requests, 85 server notifications, 10 server requests).

## Capabilities

| Capability | Status | How |
|---|---|---|
| Stream events | VERIFIED | `item/started`, `item/completed`, `item/agentMessage/delta`, `item/reasoning/summaryTextDelta`, `item/commandExecution/outputDelta`, `turn/plan/updated`, `turn/diff/updated`, `thread/tokenUsage/updated`, `account/rateLimits/updated`, `turn/completed` |
| Mid-run input + ack | VERIFIED | `turn/steer {threadId, expectedTurnId, input, clientUserMessageId}` returns `{turnId}` (ack). Next, a `userMessage` item carrying our `clientId` appears in the turn (applied). The model acted on it: it wrote FINISHED instead of DONE. |
| Interrupt | DOCUMENTED | `turn/interrupt {threadId, turnId}`; the turn ends with `status: interrupted` |
| Approvals to client | DOCUMENTED | Server requests `item/commandExecution/requestApproval`, `item/fileChange/requestApproval`, answered with `{decision: accept, acceptForSession, decline, or cancel}` |
| Read-only mode | DOCUMENTED | `thread/start {sandbox: "read-only", approvalPolicy: "never"}` (Codex's own OS sandbox) |
| Resume | DOCUMENTED | `thread/resume {threadId}`; the thread persists unless `ephemeral: true` |
| Model list + effort | VERIFIED | `model/list` gives `supportedReasoningEfforts` per model; `turn/start {effort}` |
| Usage / quota | VERIFIED | `account/rateLimits/read`: `ordinaryUsageAllowed`, `primary` (5h) and `secondary` (weekly) `usedPercent`, `credits.balance`, `rateLimitReachedType` |
| Native commands | DOCUMENTED | `review/start`, `thread/compact/start`, `thread/fork`, `thread/revert`, `thread/shellCommand` |

## Billing hazard (VERIFIED)

When the 5-hour window is at 100%, `account/rateLimits/read` reports `ordinaryUsageAllowed: false` and `rateLimitReachedType: "rate_limit_reached"`. A turn still runs, but it draws paid credits. During the turn, `rateLimitReachedType` reads `null` and only `credits.balance` falls.

The one probe turn cost about 3.7 credits. Jockey therefore:

1. refuses `turn/start` when the window is exhausted, unless `--allow-credits` is passed (or `providers.codex.allow_credits` is set in the global config), and
2. interrupts a turn as soon as `credits.balance` falls below its starting value.

## Approval inheritance

The app-server loads `~/.codex/config.toml`, hooks and MCP servers, so the user's own Codex setup applies. Jockey sets `sandbox`, `approvalPolicy` and `approvalsReviewer` on every `thread/start`, so a permissive user config can't widen a jockey profile.

## Sample (trimmed)

```json
{"method":"item/completed","params":{"item":{"type":"commandExecution","command":"/bin/zsh -lc ls","status":"completed","aggregatedOutput":"a.txt\n","exitCode":0}}}
{"method":"thread/tokenUsage/updated","params":{"tokenUsage":{"total":{"inputTokens":19751,"cachedInputTokens":13184,"outputTokens":49},"modelContextWindow":258400}}}
{"id":6,"result":{"turnId":"01a184bd-4176-..."}}
```

The full replay fixture is `tests/fixtures/codex_turn.jsonl`.
