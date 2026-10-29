# Provider research: xAI Grok Build CLI (`grok`)

Phase 0 research for jockey. Researched 2026-10-28 against the installed binary, `grok 1.0.70 (f50904eaee6c) [stable]`, signed in with a subscription (OIDC login). Test runs happened in `/tmp/work`. The raw transcripts are kept there (`t*.jsonl`, `p*.jsonl`) along with the probe client `acp.py` and the billing check `gate.sh`.

## Is it official?

Yes, this is xAI's own tool.

- `~/.local/bin/grok -> ~/.grok/bin/grok -> ~/.grok/downloads/grok-1.0.70-macos-aarch64` (Mach-O arm64, about 179 MB)
- `codesign`: `Authority=Developer ID Application: X.AI Corporation (5Y6N3AJ54S)`, identifier `xai-grok-pager`
- Installer: `curl -fsSL https://x.ai/cli/install.sh | bash`. Plugin marketplace: `github.com/xai-org/plugin-marketplace`. Official docs: https://docs.x.ai/build/overview
- The full user guide ships locally at `~/.grok/README.md` and `~/.grok/docs/user-guide/*.md`. Most DOCUMENTED entries below cite those files.

## BILLING WARNING (read first)

The account can be billed past its subscription only if pay-as-you-go is switched on. When tested it was off. Jockey should still check for this before every run.

- `_x.ai/billing` (ACP extension, no model call) returned:
  `{"config":{"creditUsagePercent":5.0,"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY",...},"onDemandCap":{"val":0},"onDemandUsed":{"val":0},"prepaidBalance":{"val":0},"isUnifiedBillingUser":true,...},"subscription_tier":"X Premium+"}`
- With `onDemandCap = 0` and `prepaidBalance = 0`, there is no paid credit to fall back to, so running out of quota should produce an error, not a charge. The value read 5.0% both before and after the 9 test prompts. The test prompts cost about USD 0.13 in notional `total_cost_usd`.
- The paid path does exist. Strings in the binary include: "You've hit the credit limit for your plan", "Pay as you go: Enable pay-as-you-go credits for on-demand usage", "You've hit your spending cap. Raise your pay-as-you-go spending cap", "Purchase credits to keep using Grok Build", "You hit your weekly limit.", "status 402", plus an `AutoTopupRule` struct (`topupAmount`, `maxAmountPerMonth`) and `BillingConfig.monthlyLimit`. If the user turns on pay-as-you-go, buys prepaid credits or sets up auto-top-up, prompts past the weekly allowance will probably be billed. We did not exhaust the allowance to check this (UNVERIFIED).
- The second paid path is the API key. If `XAI_API_KEY` is set, it is used for auth, and API-key traffic is billed per token (`apiKeySource: "user"` instead of `"oauth"` in `streaming-messages-json` init). Jockey must unset `XAI_API_KEY` in the child environment and should refuse to run if `init.apiKeySource != "oauth"`.
- Recommended jockey guard: before a run, call `_x.ai/billing` and refuse if `onDemandCap.val > 0`, `prepaidBalance.val > 0`, or there is any `autoTopup`/`monthlyLimit`. During a run, read `creditUsagePercent` and stop at a threshold.
- `total_cost_usd` and `costUsdTicks` are always reported, even on subscription. They are notional token pricing, not a charge.

## Capability summary

| Capability | Status | How |
|---|---|---|
| Stream events (headless) | VERIFIED | `grok -p ... --output-format streaming-json` (xAI-native NDJSON) or `streaming-messages-json` (Messages API stream-json shape, with `--include-partial-messages` for deltas). Stdin is read only. |
| Stream events (ACP) | VERIFIED | `grok agent stdio`, standard `session/update` plus many `_x.ai/*` notifications |
| Mid-run input + ack | VERIFIED (ACP only) | `_x.ai/interject {sessionId,text}` mid-turn. The reply is `{"result":{"status":"queued"}}` and an `_x.ai/session/interjection` echo notification arrives. It is delivered at the next tool/model safe gap in the same turn, and the model obeyed it. There is no separate live "delivered" event. Headless `-p` has no stdin channel. |
| Interrupt/cancel | VERIFIED | ACP `session/cancel` notification ends the turn in about 5 ms with `stopReason:"cancelled"` and `cancellationCategory:"MidTurnAbort"`. Headless: SIGINT gives exit 130, SIGTERM gives 143 (DOCUMENTED). |
| Approvals routed to client | VERIFIED (ACP) | `session/request_permission` with options `allow_always`/`allow_once`/`reject_once`. Rejecting ends the whole turn (`cancelled`, `PermissionRejected`). Headless cannot ask: an unapproved call is refused, the turn ends `cancelled`, stderr prints a note, and the exit code is 0. |
| Read-only mode | PARTIAL | Tool allowlist `--tools read_file,grep,list_dir` (DOCUMENTED, flag accepted in a VERIFIED run). `--permission-mode dontAsk` is VERIFIED NOT read-only, because `touch` is auto-approved. `--sandbox read-only` is VERIFIED to refuse to start on this Mac (OrbStack `docker.sock` symlink). |
| Resume | VERIFIED | Headless `-r <id>` / `-c` / `--fork-session`. ACP `session/load` (replays history) and `sessionCapabilities.resume`. A session created over ACP was resumed headless and kept its context. |
| Model list | VERIFIED | `grok models`, ACP `initialize._meta.modelState.availableModels`, `session/new.models`, `configOptions` |
| Effort | VERIFIED | `--reasoning-effort/--effort <none..max>` (headless). ACP `session/set_config_option {configId:"reasoning_effort", value:"low"}` |
| Usage/quota | VERIFIED | Per turn: `usage` events and `end`/`turn_completed` usage. Per session: `grok usage <id>`, `_x.ai/session/usage`. Account quota: `_x.ai/billing` (`creditUsagePercent`, weekly period). |
| Quota exhaustion reporting | DOCUMENTED (from strings/docs only) | 402 / "You hit your weekly limit." / "credit limit" errors. Headless gives `{"type":"error","message":...}` and exit 1. Hooks get `StopFailure` with `error:"rate_limit"` (capacity is also classed as `rate_limit`). Not reproduced. |
| Native commands | VERIFIED (listed) | `available_commands` / `available_commands_update`: `compact`, `always-approve`, `context`, `session-info`, `deep-research`, `workflow`, `goal`, `loop`, plus skills. Callable as `/name` prompt text. |
| Session modes (`session/set_mode`) | UNSUPPORTED (no `modes` advertised) | `session/set_mode` returns `{}` with no modes listed. Plan mode is the `--permission-mode plan` flag or `_x.ai/toggle_plan_mode`. |
| `session/set_model` | VERIFIED | Returns `{"_meta":{"model":{"Ok":"grok-4.5"}}}`. `set_config_option` is preferred. |

## Steering verdict

Grok has real mid-turn steering, with acknowledgement, over ACP. It is the best steering surface tested so far.

- Send `{"jsonrpc":"2.0","id":N,"method":"_x.ai/interject","params":{"sessionId":"...","text":"..."}}` while `session/prompt` is in flight.
- The ack arrives at once: the response `{"result":{"status":"queued"}}` plus the notification `_x.ai/session/interjection {"sessionId","text"}`.
- Delivery: the text is injected at the next safe gap, after the current tool result, as a synthetic user message. The persisted `updates.jsonl` records it as `user_message_chunk` with `"The user sent a message while you were working:\n<user_query>\n...\n</user_query>\nIf the user is asking for a response, address the user first. After replying, complete any unfinished tasks from previous turns."`. That `user_message_chunk` is not emitted live over ACP, so jockey cannot observe the moment of delivery. It can only rely on "queued" and confirm afterwards from transcript or behaviour.
- Verified: the interjection was sent 4 s into a 3-read turn, after the first read. It was delivered after read 1. The final reply ended with the requested `BANANA`, in the same turn (`stopReason:"end_turn"`, one `session/prompt`).
- Caveat: if the turn ends before a safe gap (for example a rejected permission), the queued interjection was not seen to run as a new turn (`_x.ai/queue/changed` showed `entries: []`). Jockey should re-send it as a normal `session/prompt` when it sees `turn_completed` without delivery.
- Extension methods must use the leading underscore (`_x.ai/...`). `x.ai/interject` without it returns `-32601 Method not found`.
- Fallback: `session/cancel` followed by a new `session/prompt` also works (VERIFIED cancel). That matches the TUI's "send now" (cancel and send).
- Also seen in the binary but not tested: `_x.ai/queue/*` (`clear`, `edit`, `remove`, `reorder`, `hold_edit`) for managing queued follow-ups, and `HumanInterjection`/`ParentInterject` variants. The TUI's `[ui] follow_up_behavior = "steer"` is the same mechanism.

## FOOTGUN: user config leaks always-approve into ACP

A `~/.grok/config.toml` with `[ui] permission_mode = "always-approve"`. A plain `grok agent stdio` session picked this up: `_x.ai/sessions/changed` showed `"yolo": true`, and the write tool ran with no `session/request_permission`. File `acp_test.txt` was created. We did not pass a bypass flag, but the run was effectively unattended always-approve. The only effect was one file written in the scratch directory.

Fix (VERIFIED): pass `"_meta": {"yoloMode": false}` on `session/new`. The session then reports `yolo: false` and permission requests reach the client. Jockey must always send `yoloMode:false` (and `autoMode:false`) explicitly. Headless runs should always pass an explicit `--permission-mode`. With `--permission-mode default`, writes were refused as expected.

Other things ACP sessions inherit from the user's environment: global MCP servers, global hooks (including those in `~/.claude/settings.json`), plugins, skills, memory, and the default model/effort from config (the session showed `reasoningEffort: "xhigh"` even with `--reasoning-effort low` on `grok agent`). For a critic, use `GROK_HOME` isolation or `--plugin-dir`, and set effort with `set_config_option`.

## 1. Headless (`grok -p`)

Flags (from `grok --help`, 1.0.70): `-p/--single <PROMPT>`, `--prompt-file`, `--prompt-json`, `--output-format plain|json|streaming-json|streaming-messages-json`, `--include-partial-messages`, `-m/--model`, `--reasoning-effort/--effort`, `--permission-mode default|acceptEdits|auto|dontAsk|bypassPermissions|plan`, `--tools`, `--disallowed-tools`, `--allow`, `--deny`, `--sandbox`, `--max-turns`, `-r/--resume`, `-c/--continue`, `-s/--session-id <uuid>` (new session only), `--fork-session`, `--cwd`, `--add-dir`, `--rules`, `--system-prompt-override`, `--json-schema`, `--no-subagents`, `--no-plan`, `--disable-web-search`. Headless does not read piped stdin into the prompt.

### streaming-json (VERIFIED, t1)

Command:
```
grok -p "Read calc.py and tell me in one sentence what it does. Then create a file named hello.txt containing the word hi." \
  --output-format streaming-json -m grok-4.6 --effort low --permission-mode default
```
stderr: `permission required: Write `.../hello.txt` was not run, as a headless run cannot ask for approval`. Exit code 0.

Real lines (trimmed):
```json
{"type":"available_commands","tools":["run_terminal_command","read_file","search_replace","list_dir","grep",...,"write"],"commands":["compact","always-approve","context",...]}
{"type":"text","data":"I'll"}
{"type":"text","data":" read"}
{"type":"usage","usage":{"input_tokens":14663,"output_tokens":115,"cache_read_input_tokens":6016,"cache_creation_input_tokens":0,"reasoning_tokens":35},"signature":"x4C8..."}
{"type":"tool_call","toolCallId":"call-44e3...-1","title":"read_file","kind":"read","status":"pending","toolName":"read_file","rawInput":{"target_file":"/tmp/work/calc.py"},"content":[],"locations":[]}
{"type":"tool_call_update","toolCallId":"call-44e3...-1","status":"completed","content":[{"type":"content","content":{"type":"text","text":"1→def add(a, b):\n    return a + b\n"}}],"rawOutput":{"type":"ReadFile","FileContent":{...}},"locations":[]}
{"type":"tool_call","toolCallId":"call-3d79...-2","title":"write","kind":"write","status":"pending","toolName":"write","rawInput":{"file_path":".../hello.txt","content":"hi\n"},"content":[],"locations":[]}
{"type":"tool_call_update","toolCallId":"call-3d79...-2","status":null,"content":[{"type":"diff","path":".../hello.txt","oldText":"","newText":"hi\n"}],"rawOutput":null,"locations":[...]}
{"type":"tool_call_update","toolCallId":"call-3d79...-2","status":"failed","content":[{"type":"content","content":{"type":"text","text":"User cancelled the execution for tool `write`"}}],"rawOutput":null,"locations":[]}
{"type":"end","stopReason":"cancelled","sessionId":"01a184be-cb0f-7a03-8e88-2129f656fa75","requestId":"59cb57c9-...","cwd":"/tmp/work","usage":{"input_tokens":14929,"cache_read_input_tokens":26624,"cache_creation_input_tokens":0,"output_tokens":160,"reasoning_tokens":46,"total_tokens":41713},"num_turns":2,"total_cost_usd":0.0150042,"total_cost_usd_ticks":150042000,"modelUsage":{"grok-4.6-build":{"inputTokens":14929,"outputTokens":160,"cacheReadInputTokens":26624,"cacheCreationInputTokens":0,"modelCalls":2,"costUSD":0.0150042}}}
```
Event types: `text`, `thought`, `tool_call`, `tool_call_update`, `usage` (one per model response), `plan`, `available_commands`, `end` (always last), `error`. The docs also mention `max_turns_reached` and `auto_compact_*`. Text deltas arrive at token granularity.

Note: a refused tool call ends the whole headless turn with `stopReason: "cancelled"`. The model does not get a chance to continue.

### streaming-messages-json (VERIFIED, t2)

```
grok -p "Run exactly this shell command and nothing else: touch hello2.txt   Then run: echo hi > hello3.txt ..." \
  --output-format streaming-messages-json -m grok-4.6 --effort low --permission-mode dontAsk --no-subagents
```
```json
{"type":"system","subtype":"init","session_id":"01a184bf-f3f6-...","apiKeySource":"oauth","model":"grok-4.6","cwd":"...","permissionMode":"dontAsk","tools":[...],"slash_commands":[...],"mcp_servers":[{"name":"figma","status":"pending"},...],"skills":[...]}
{"type":"assistant","message":{"id":"msg_0","type":"message","role":"assistant","model":"grok-4.6","content":[{"type":"thinking","thinking":"...","signature":"..."},{"type":"text","text":"I'll run those two commands now..."},{"type":"tool_use","id":"call-3a05...-0","name":"run_terminal_command","input":{"command":"touch hello2.txt","description":"..."}},{"type":"tool_use","id":"call-3a05...-1","name":"run_terminal_command","input":{"command":"echo hi > hello3.txt","description":"..."}}],"stop_reason":"tool_use","stop_sequence":null,"usage":{...}},"parent_tool_use_id":null,"session_id":"...","uuid":"..."}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-3a05...-0","content":"{\"type\":\"Bash\",...\"exit_code\":0,\"command\":\"touch hello2.txt\"...}","is_error":false},{"type":"tool_result","tool_use_id":"call-3a05...-1","content":"[...\"User cancelled the execution for tool `run_terminal_command`\"...]","is_error":true}]},...}
{"type":"result","subtype":"error_during_execution","is_error":true,"duration_ms":3928,"duration_api_ms":2829,"num_turns":1,"stop_reason":"cancelled","total_cost_usd":0.01364352,"usage":{...},"modelUsage":{...},"errors":["cancelled"],"session_id":"...","uuid":"..."}
```
Result: `hello2.txt` was created under `dontAsk`, because `mkdir`/`touch` are on the built-in read-only list. `echo > file` was refused.

### json

A single object: `text`, `stopReason`, `sessionId`, `requestId`, `cwd`, `usage`, `num_turns`, `modelUsage`, `total_cost_usd(_ticks)` (VERIFIED, t9).

## 2. ACP (`grok agent stdio`)

Command: `grok agent [--no-leader] [-m MODEL] [--reasoning-effort E] [--plugin-dir DIR] stdio`. Agent options go before `stdio`. Never use `--always-approve`/`--yolo` there. `--no-leader` avoids attaching to a shared leader process. JSON-RPC 2.0, newline-delimited, on stdin/stdout. Stderr carries logs (for example an MCP spawn failure).

### initialize (VERIFIED)
Request: `{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false},"clientInfo":{...}}`
Response (trimmed):
```json
{"protocolVersion":1,"agentCapabilities":{"loadSession":true,"promptCapabilities":{"image":true,"audio":false,"embeddedContext":true},"mcpCapabilities":{"http":true,"sse":true},"sessionCapabilities":{"list":{},"resume":{},"close":{}},"auth":{},"_meta":{"x.ai/fs_notify":true,"x.ai/hooks":{"blockingEvents":["pre_tool_use","stop","subagent_stop"],"decisions":["deny","block"],"stopSignals":["continue","stopReason","additionalContext"]},"x.ai/capabilities":{...}}},
 "authMethods":[{"id":"cached_token",...},{"id":"grok.com","name":"Grok",...}],
 "_meta":{"agentVersion":"1.0.70","modelState":{"currentModelId":"grok-4.7-build-fast","availableModels":[{"modelId":"grok-4.7","name":"Grok 4.7","_meta":{"totalContextTokens":256000,"supportsReasoningEffort":true,"reasoningEffort":"high","reasoningEfforts":[{"id":"xhigh",...},{"id":"high",...,"default":true},{"id":"medium",...},{"id":"low",...}],"contextWindows":[256000,500000]}}, ...]},"availableCommands":[...],"cancelRewind":true,...}}
```

### session/new (VERIFIED)
Params: `{"cwd":"...","mcpServers":[],"_meta":{"yoloMode":false}}`. Other `_meta` keys (DOCUMENTED): `rules`, `systemPromptOverride`, `agentProfile`, `autoMode`, `pluginDirs`.
Result keys: `sessionId`, `models` (unstable ACP model state), `configOptions` (`model`, `reasoning_effort`), `_meta` (`x.ai/sessionConfig`, `x.ai/sessionDetail`, git info). There is no `modes` key.

### Methods tested
| Method | Result |
|---|---|
| `session/prompt` | VERIFIED. Result `{"stopReason":"end_turn","_meta":{"promptId","usage":{inputTokens,outputTokens,cachedReadTokens,reasoningTokens,modelCalls,apiDurationMs,costUsdTicks,modelUsage,numTurns},...}}` |
| `session/cancel` (notification) | VERIFIED. `stopReason:"cancelled"`, `_meta.cancellationCategory:"MidTurnAbort"` |
| `session/load` | VERIFIED. Replays history as `session/update` (36 updates for a 1-turn session) |
| `session/set_config_option` | VERIFIED. `{"sessionId","configId":"reasoning_effort","value":"low"}` returns the full `configOptions`. A plain string value works. The docs show `{"value":{"value":"high"}}`. |
| `session/set_model` | VERIFIED. `{"_meta":{"model":{"Ok":"grok-4.5"}}}` |
| `session/set_mode` | Returns `{}`. No modes are advertised, so treat as UNSUPPORTED |
| `_x.ai/interject` | VERIFIED. See the steering verdict |
| `_x.ai/billing` | VERIFIED. Account quota (see the billing warning) |
| `_x.ai/session/usage` | VERIFIED. `{"usage":{inputTokens,outputTokens,totalTokens,cachedReadTokens,cacheCreationTokens,reasoningTokens,modelCalls,apiDurationMs,numTurns}}` |
| `_x.ai/session/info` | VERIFIED. Model, context usage breakdown, tool count |
| `_x.ai/session/state` | VERIFIED. Needs `cwd`. Returns `sandbox_profile` and `reasoning_effort` |
| `_x.ai/sessions/list` | VERIFIED. The roster includes `yolo`, `activity`, `reasoningEffort` |

The binary lists about 280 `x.ai/*` names, for example `subagent/cancel`, `toggle_plan_mode`, `compact_conversation`, `rewind/*`, `session/fork`, `queue/*`, `models/list`.

### Notifications seen (VERIFIED)
- `session/update` kinds: `agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update`, `available_commands_update`, `session_info_update`. `plan` is DOCUMENTED. `user_message_chunk` appears only on `session/load` replay. Every update carries `_meta` with `promptId`, `eventId`, `totalTokens`, `agentTimestampMs`.
- `_x.ai/session_notification` kinds: `turn_completed` (stop_reason + usage + `cancellationCategory`), `response_completed` (per-response usage), `tool_call_delta_chunk` (streamed tool args), `pending_interaction` / `interaction_resolved`, `hook_run_started` / `hook_execution`, `session_summary_generated`, `last_turn_summary`.
- Others: `_x.ai/session/prompt_complete`, `_x.ai/session/interjection`, `_x.ai/queue/changed` (`runningPromptId`, `entries`), `_x.ai/sessions/changed`, `_x.ai/session/setup` (startup phases), `_x.ai/mcp/*`, `_x.ai/models/update`, `_x.ai/settings/update`, `_x.ai/announcements/update`.

### session/request_permission (VERIFIED, p4)
```json
{"jsonrpc":"2.0","id":0,"method":"session/request_permission","params":{"sessionId":"01a184c1-0466-...","toolCall":{"toolCallId":"call-6c86...-0","kind":"edit","title":"Write `/private/tmp/.../acp_test2.txt`","rawInput":{"variant":"Write","file_path":".../acp_test2.txt","content":"hi\n"},"_meta":{"x.ai/tool":{"version":1,"name":"write","kind":"write","namespace":"grok_build","label":"Write","read_only":false,"input":{"path":"..."}},"x.ai/requestingSessionId":"01a184c1-0466-..."}},"options":[{"optionId":"allow-edits-session","name":"Yes, allow all edits during this session","kind":"allow_always"},{"optionId":"allow-once","name":"Yes","kind":"allow_once"},{"optionId":"reject-once","name":"No, and tell Grok what to do differently","kind":"reject_once"}]}}
```
Reply: `{"jsonrpc":"2.0","id":0,"result":{"outcome":{"outcome":"selected","optionId":"reject-once"}}}`
Then: `tool_call_update {"status":"failed","content":[...,"User rejected the execution for tool `write`"],"_meta":{"toolNotRun":"rejected"}}`, the turn ends with `stopReason:"cancelled"` and `cancellationCategory:"PermissionRejected"`, `cancellationContext:{"tool_name":"write","reason":"User rejected the execution"}`.
For shell commands the options are `always-allow` / `allow-once` / `reject-once`. The `x.ai/tool._meta.read_only` flag on every `tool_call` is useful for critic policy.

## 3. Mid-run steering

See the steering verdict above. Commands used:
```
SESSION_META='{"yoloMode": false}' GROK_ARGS='["-m","grok-4.6"]' STEER_METHOD=_x.ai/interject \
STEER_PARAMS='{"sessionId": "{sid}", "text": "{text}"}' APPROVE_SAFE=1 \
python3 acp.py p7.jsonl steer "<3-read task>" "Additional instruction from the user: end your final reply with the exact word BANANA." 4.0
```
Transcript excerpt (times are seconds from start):
```
0.233 -> session/prompt
4.318 -> {"id":4,"method":"_x.ai/interject","params":{"sessionId":"01a184c2-36a8-...","text":"Additional instruction ... BANANA."}}
4.319 <- {"id":4,"result":{"result":{"status":"queued"}}}
4.319 <- {"method":"_x.ai/session/interjection","params":{"sessionId":"...","text":"Additional instruction ... BANANA."}}
4.824 tool_call read_file #1 ... 7.012 read_file #2 ... 13.409 read_file #3
16.748 <- {"id":3,"result":{"stopReason":"end_turn",...}}   final text ends with "BANANA"
```
Headless `-p`: there is no mid-run input. You can only kill it (SIGINT/SIGTERM) and `-r <id>` with a new prompt.

## 4. Permissions, sandbox, read-only critic recipe

Modes: `default` (ask; read-only tools and read-only shell commands auto-run), `acceptEdits`, `plan`, `auto` (classifier), `dontAsk` (only pre-approved plus the built-in read-only list), `bypassPermissions` (= always-approve, never use). Rules: `--allow`/`--deny` with `Bash(...)`, `Edit(...)`, `Write(...)`, `Read(...)`, `Grep(...)`, `WebFetch(...)`, `MCPTool(...)`. Deny wins.

Findings:
- `dontAsk` is not read-only: `touch`/`mkdir` are auto-approved in every mode (VERIFIED: `hello2.txt` created).
- `--sandbox read-only` (Seatbelt on macOS; reads anywhere, writes only to `~/.grok`) refused to start on a Mac with OrbStack installed: `error: could not apply the 'read-only' sandbox profile ... could not resolve runtime-socket deny path /var/run/docker.sock: endpoint is a symlink to a path outside the deny list` (OrbStack). It fails closed, which is good, but it means the sandbox cannot be used on this machine as configured. Docs say built-in profiles otherwise "warn and continue without enforcement" on unsupported platforms, so check `_x.ai/session/state.sandbox_profile`. Sandbox with `--leader` is refused.
- Recommended critic, headless:
  `grok -p "<review prompt>" --output-format streaming-json --permission-mode dontAsk --tools read_file,grep,list_dir --disallowed-tools Agent --no-subagents --disable-web-search -m <model> --effort <e>`
  plus `--sandbox read-only` where it works, an unset `XAI_API_KEY`, and ideally an isolated `GROK_HOME` (holding only `auth.json`) so user hooks, MCP servers and the always-approve config do not load.
- Recommended critic, ACP: `session/new` with `_meta:{"yoloMode":false,"autoMode":false}`. The client rejects any `session/request_permission` (note that rejecting ends the turn). Prefer to also limit tools. `grok agent` has no `--tools` flag, so use a dedicated `--agent-profile` or an isolated `GROK_HOME` config (not tested).

## 5. Models and effort (VERIFIED)

`grok models`:
```
You are logged in with grok.com.
Default model: grok-4.7-build-fast
Available models:
  - grok-4.7
  * grok-4.7-build-fast (default)
  - grok-4.6
  - grok-4.5
```
Efforts per model, from `reasoningEfforts`: grok-4.7 / 4.7-build-fast / 4.6 support `xhigh|high|medium|low` (default `high`). grok-4.5 supports `high|medium|low`. The CLI also accepts `none|minimal|max` but a model rejects levels it does not advertise. `grok-4.7-build-fast` is described as "Fast variant. 2x the price." The served model id shows up as `grok-4.6-build` in `modelUsage`. Context windows are 256000 or 500000.

## 6. Sessions, resume, usage

- Session ids are UUIDv7-like (`01a184c2-36a8-7a01-989a-3434e72b0ba6`). They are returned in `end.sessionId`, `init.session_id`, and ACP `session/new`.
- Resume VERIFIED: `grok -p "... what word did you end your previous reply with?" -r 01a184c2-36a8-... --output-format json ...` returned `"text": "BANANA"`. That session had been created over ACP.
- Storage: `~/.grok/sessions/<urlencoded-cwd>/<id>/` (`updates.jsonl`, `chat_history.jsonl`, `usage.json`, `signals.json`, ...). `grok sessions list|search|delete`, `grok export`.
- `grok usage <id> [turn]` gives JSON with `session` totals and per-`turns` `inputTokens/outputTokens/cachedReadTokens/reasoningTokens/modelCalls/costUsdTicks` (VERIFIED).
- Account quota: `_x.ai/billing` (weekly period, `creditUsagePercent`). The TUI has `/usage`.
- Quota exhaustion (DOCUMENTED from strings; not reproduced): the HTTP 402 messages "You hit your weekly limit.", "You've hit the credit limit for your plan", "You hit your free usage limit.", with upsells to SuperGrok tiers or pay-as-you-go. Headless gives `{"type":"error","message":"..."}` and exit 1. ACP: the prompt fails or returns an error. Hook `StopFailure` gets `error: "rate_limit"`. Jockey should match on 402, `credit limit`, `weekly limit` and `rate_limit`, and should poll `_x.ai/billing` rather than depend on error text.

## 7. Native features worth exposing

| Feature | Headless | ACP |
|---|---|---|
| `/compact`, `/context`, `/session-info` | as prompt text | as prompt text; also `_x.ai/compact_conversation`, `_x.ai/session/info` |
| `/goal` (autonomous objective with budget) | yes | yes |
| `/deep-research`, `/workflow` (background workflows) | yes (waits up to 10 min) | yes |
| Subagents (`spawn_subagent`) | `--agents`, `--no-subagents` | `_x.ai/subagent/cancel` |
| Plan mode | `--permission-mode plan` | `_x.ai/toggle_plan_mode` |
| Worktrees | `-w/--worktree`, `--worktree-ref` | `_x.ai/git/worktree/*` |
| Structured output | `--json-schema` | n/a |
| Rewind / restore code | no | `_x.ai/rewind/*`, `--restore-code` |
| Hooks (Claude-compatible) | yes | `x.ai/hooks` capability |
| Usage | `end` event, `grok usage` | `_x.ai/session/usage`, `_x.ai/billing` |
| WebSocket server | n/a | `grok agent serve --bind ... --secret ...` |

## 8. Terms

- Official docs explicitly support programmatic and embedded use. https://docs.x.ai/build/overview says: "Grok Build is a powerful and extensible coding agent. Use it via an interactive TUI, headlessly in scripts or bots, or through the Agent Client Protocol (ACP) in other apps." The bundled README (`~/.grok/README.md`, line 5) says: "Use it interactively as a TUI, or integrate it into your own apps via headless mode and the Agent Client Protocol (ACP)." The bundled docs (`15-agent-mode.md`) recommend ACP "from IDEs, SDKs, eval harnesses, and custom apps".
- Neither page separates subscription login from API-key auth for this use. The headless docs suggest `XAI_API_KEY` for CI. We found no statement that limits subscription logins to interactive use.
- The xAI Acceptable Use Policy (https://x.ai/legal/acceptable-use-policy) reportedly prohibits accessing the services "through unauthorized automated or non-human means, such as bots or scripts". This is from a search-index excerpt only. x.ai returned HTTP 403 to fetches, so the wording was NOT verified verbatim. Using the official CLI's documented headless/ACP interfaces looks like the authorized path, but the user should read the AUP and the consumer Terms (https://x.ai/legal/terms-of-service) in a browser before relying on this.
- Verdict: driving the official `grok` binary locally through its documented headless/ACP interfaces is explicitly advertised by xAI. No subscription-specific prohibition was found. The AUP bot clause is the one open item to confirm by hand.

## Test log (9 live prompts, all grok-4.6, notional total about USD 0.13)

| # | File | What |
|---|---|---|
| 1 | t1.jsonl | headless streaming-json, `--permission-mode default`, write refused |
| 2 | t2.jsonl | headless streaming-messages-json, `dontAsk`, touch allowed / echo refused (an earlier `--sandbox read-only` attempt failed at startup, no prompt sent) |
| 3 | p3.jsonl | ACP prompt with no yoloMode: config always-approve leaked, write executed |
| 4 | p4.jsonl | ACP with `yoloMode:false`: `session/request_permission` captured, rejected |
| 5 | p5.jsonl | wasted: interject sent without the `_` prefix, got Method not found |
| 6 | p6.jsonl | interject queued, but the turn ended on a rejected shell permission |
| 7 | p7.jsonl | interject delivered mid-turn, obeyed (BANANA) |
| 8 | p8.jsonl | `session/cancel` mid-turn |
| 9 | t9.json | headless `-r` resume of the ACP session |

Before prompts 2 to 9, the billing check (`gate.sh`) confirmed `onDemandCap=0` and `prepaidBalance=0`.
