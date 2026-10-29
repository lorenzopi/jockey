# Provider research: Claude Code CLI

Phase 0 research for jockey. Provider: `claude` (Claude Code CLI), installed **v2.1.312**, macOS, subscription sign-in (`claude auth status` reports `"authMethod": "claude.ai"`, `"apiKeySource": "none"`).

Date: 2026-10-28. Nine live prompts were run, all on `--model haiku` (`claude-haiku-5-5`), in a scratch directory. Raw logs (`t1.jsonl` to `t9.jsonl`) were kept in that directory. No `bypassPermissions` or `--dangerously-*` flag was used.

Legend: **VERIFIED** = ran it on v2.1.312. **DOCUMENTED** = official docs only (code.claude.com/docs). **UNDOCUMENTED** = works, or is in the binary/SDK, but has no public doc. **UNSUPPORTED** = not available.

## Billing guard (read first)

Nothing in these runs was billed beyond the subscription. Every `rate_limit_event` showed:

```json
"overageStatus": "rejected", "overageDisabledReason": "out_of_credits", "isUsingOverage": false
```

The `get_usage` control request returned `extra_usage.is_enabled: false`, `disabled_reason: "out_of_credits"`, `can_purchase_credits: false`, but also `credits_ever_enabled: true`, . **If the user tops up credits, paid overage can switch back on.** Jockey must block paid fallback itself:

1. **Preflight**: send the control request `{"subtype":"get_usage"}` and refuse to start if `response.rate_limits.extra_usage.is_enabled == true` (or `spend.enabled == true`). The only way to override is an explicit user opt-in.
2. **Per event**: on every `rate_limit_event`, abort the run (interrupt, then close stdin) if `rate_limit_info.isUsingOverage == true`, or `overageStatus` is anything other than `"rejected"`, or `status == "rejected"`.
3. **Over-limit shape (DOCUMENTED)**: `rate_limit_info.status` is `"allowed" | "allowed_warning" | "rejected"`. When the subscription is exhausted, a rejection carries `errorCode: "credits_required"`, plus `canUserPurchaseCredits` and `hasChargeableSavedPaymentMethod`. Retries surface as `system/api_retry` with `error: "rate_limit" | "billing_error"`.
4. **Models and modes that bill credits**: per the docs, "Depending on your plan and seat tier, Fable usage can bill to usage credits instead of drawing on your plan's included limits", and fast mode spend also goes to usage credits. Jockey should not default to `fable`, and should never enable fast mode (`fast_mode_state: "off"`, `fast_mode_disabled_reason: "sdk_opt_in_required"` by default in `-p`).
5. `total_cost_usd` / `costUSD` in `result` are **notional list-price estimates** (`"costBasis": "list"`), not charges. Do not use them as a billing signal.

## Capability summary

| Capability | Status | How |
|---|---|---|
| Stream events (NDJSON) | VERIFIED | `-p --output-format stream-json --verbose`. Events seen: `system/hook_started`, `system/hook_response`, `system/init`, `assistant`, `user` (tool_result), `rate_limit_event`, `system/thinking_tokens`, `system/task_started`, `system/task_updated`, `system/task_notification`, `system/background_tasks_changed`, `system/permission_denied`, `system/status`, `system/compact_boundary`, `command_lifecycle`, `control_request`, `control_response`, `result`. Token deltas via `--include-partial-messages` (DOCUMENTED, `stream_event`). |
| Mid-run input | VERIFIED | `--input-format stream-json`: write `{"type":"user",...}` lines to stdin at any time. The default priority (`next`) is delivered at the **next tool boundary in the same turn**. The model obeyed a steering note injected during a `sleep 3` tool call. |
| Ack that input was received/consumed | VERIFIED (UNDOCUMENTED event) | `command_lifecycle` events keyed by the client-supplied `uuid`: `queued` (received), `started` (taken into the model context), `completed` / `cancelled`. Emitted without any extra flag. `--replay-user-messages` (DOCUMENTED) also echoes the message with `isReplay: true` at the moment it is consumed. |
| Urgent steering (`priority`) | VERIFIED | `"priority":"now"` interrupts the turn. Without `origin`, it waited for the running Bash call to finish (17 s), then ended the turn (`terminal_reason: "aborted_tools"`) and started a new turn with the message. With `"origin":{"kind":"human"}`, it moved the running Bash call to the background within about 1 s and delivered the message (2 s total). `"later"` holds the message until the turn ends (DOCUMENTED). |
| Interrupt | VERIFIED | Send the control request `{"type":"control_request","request_id":"x","request":{"subtype":"interrupt"}}`. The reply is a `control_response` with `{"still_queued":[]}`. The running tool is killed, the result has `subtype: "error_during_execution"` and `terminal_reason: "aborted_tools"`, and the session stays alive for the next message. `cancel_queued:true` and `send_now:true` variants exist (capabilities `interrupt_cancel_queued_v1`, `interrupt_send_now_v1`). SIGINT also ends the turn; SIGTERM exits 143 with no result (DOCUMENTED). |
| Approvals routed to client | VERIFIED | `--permission-prompt-tool stdio` (what the Agent SDK passes; it is not in `--help`, and the docs only describe the MCP-tool form). The CLI emits `control_request` `can_use_tool`, and the client answers `control_response` with `{"behavior":"allow","updatedInput":{...}}` or `{"behavior":"deny","message":"..."}`. The deny message reaches the model verbatim as the tool_result. |
| Other control requests | VERIFIED | `initialize` (returns commands, agents, **models**, account, capabilities), `get_usage`, `get_context_usage`, `set_permission_mode`, `set_model`. About 60 subtypes exist in the SDK bundle (`set_max_thinking_tokens`, `stop_task`, `rewind_files`, `mcp_*`, `apply_flag_settings`, ...). Only the methods on the SDK `Query` object are DOCUMENTED. |
| Read-only mode | VERIFIED | `--tools "Read,Grep,Glob" --permission-mode dontAsk --permission-prompts none`, plus `--strict-mcp-config` and `--safe-mode`. The model saw only `['Glob','Grep','Read']` and could not create a file. `plan` mode is **not** strictly read-only per the docs (it can run shell commands). `--restricted` (DOCUMENTED) also strips code-running tools and ignores user/project settings. |
| Sandbox | DOCUMENTED | `--settings '{"sandbox":{"enabled":true,"allowUnsandboxedCommands":false}}'` (OS-level sandbox for Bash). Not tested. |
| Resume / continue / fork | VERIFIED | `--resume <id> --fork-session` gave a new `session_id` and kept the context (the model recalled `a.txt`). `--continue`, `--session-id <uuid>`, `--no-session-persistence` (DOCUMENTED / in help). Transcripts are in `~/.claude/projects/<cwd-slug>/<id>.jsonl`. |
| Model selection | VERIFIED | `--model haiku\|sonnet\|opus\|fable\|<full id>`, `--fallback-model`. Mid-session: control request `set_model` (VERIFIED). |
| Model list | VERIFIED | The `initialize` control_response `.response.models[]` has `value`, `resolvedModel`, `supportedEffortLevels`, `supportsAutoMode`, and so on. There is no CLI subcommand for it (UNSUPPORTED as a standalone command). |
| Effort | DOCUMENTED | `--effort low\|medium\|high\|xhigh\|max` (plus `ultracode`). Per-model levels come from `models[].supportedEffortLevels`. `--thinking adaptive\|disabled` and `--max-thinking-tokens N` are hidden flags the SDK passes. |
| Usage / rate limits | VERIFIED | `rate_limit_event` (per request: five_hour and seven_day utilization and reset times), `result.usage` and `modelUsage` (tokens), `get_usage` control request (full subscription windows, extra-usage state). |
| Native commands | VERIFIED | `/compact` sent as a stdin user message gives `system/status` `compacting`, then `compact_boundary` (`pre_tokens`, `post_tokens`). DOCUMENTED: `/model x`, `/effort x`, `/context`, `/usage`, `/config k=v`, and skills via `/name`. |
| Todo / task list | DOCUMENTED | `TodoWrite` and `TaskCreate`/`TaskUpdate` are **not** in the default tool set on current models (not in the init `tools` list). Opt in with `CLAUDE_CODE_ENABLE_TODO_TOOLS=1` or by naming them in `--tools`. They then appear as normal `tool_use` events. |
| Subagents | DOCUMENTED | Agent/Task tool. Child events carry `parent_tool_use_id`. `--forward-subagent-text` adds their text and thinking. `result.subagent_stats` (VERIFIED, zero). |
| Hook events | VERIFIED | SessionStart hooks from `~/.claude` run in `-p` and emit `hook_started` / `hook_response`. User plugins inject context into the primary. Use `--safe-mode` or `--setting-sources` to isolate. `--bare` is **unusable** with subscription auth (it never reads OAuth). |

## Steering verdict

Claude Code is the strongest steering target found so far. With `--input-format stream-json`, jockey can:

- **Inject critic corrections mid-turn without killing work.** Use the default priority to land the message at the next tool boundary, in the same turn. This was VERIFIED: the model skipped the remaining `sleep` calls and followed the injected instruction.
- **Get a positive receipt.** `command_lifecycle` `queued` means the CLI received the message. `started` (plus the `isReplay` echo) means it is now in the model context. `completed` / `cancelled` give the outcome. Correlate them by the `uuid` jockey supplies.
- **Escalate.** `priority:"now"` forces the message in at once. The `interrupt` control request hard-stops the turn while keeping the session.
- **Act as the approval gate.** `--permission-prompt-tool stdio` routes every non-preapproved tool call to jockey as `can_use_tool`, so a critic can veto a specific action with a reason the model sees.

Caveats:

- `command_lifecycle`, `priority:"now"` delivery semantics, and the full control-request set are internal or partly documented, and could change. Feature-detect them through `system/init.capabilities` (`msg_lifecycle_v1`, `interrupt_receipt_v1`, `interrupt_send_now_v1`, ...).
- `origin:{kind:"human"}` gives the gentler "move to background" behavior for `now`. The docs say a host should stamp it for **keyboard input**. Jockey critic messages are not human input, so stamping it would mislabel provenance. Default to `next`, or `now` without `origin`.

## Terms (subscription use from another local program)

Official statements:

1. Claude Code docs, Legal and compliance, "Authentication and credential use" (https://code.claude.com/docs/en/legal-and-compliance):
   > "**OAuth authentication** is intended exclusively for purchasers of Claude Free, Pro, Max, Team, and Enterprise subscription plans and is designed to support ordinary use of Claude Code and other native Anthropic applications."

   > "**Developers** building products or services that interact with Claude's capabilities, including those using the Agent SDK, should use API key authentication through Claude Console or a supported cloud provider. Anthropic does not permit third-party developers to offer Claude.ai login into their own applications, or to route requests through Free, Pro, or Max plan credentials on behalf of their users. Moreover, developers may not collect, store, or intermediate Claude.ai credentials or session tokens"

   > "Nor does it prevent an end user from signing in to the unmodified Claude Code binary with their own Claude subscription, including where a platform hosts Claude Code as described under *Can customers offer Claude Code in their products?* above."

   > "Advertised usage limits for Pro and Max plans assume ordinary, individual usage of Claude Code and the Agent SDK."

   The same page, for products that run Claude Code: "The Claude Code binary must not be modified ... customers may not remove, disable, or restrict any authentication method built into it", and "Each end user must authenticate with their own Anthropic API key, Claude subscription plan credentials, or 3P inference provider credential".

2. Agent SDK overview (https://code.claude.com/docs/en/agent-sdk/overview):
   > "Unless previously approved, Anthropic does not allow third party developers to offer claude.ai login or rate limits for their products, including agents built on the Claude Agent SDK."

3. Consumer Terms, effective Oct 8 2025 (https://www.anthropic.com/legal/consumer-terms), §3 prohibits, "Except when you are accessing our Services via an Anthropic API Key or where we otherwise explicitly permit it", accessing "the Services through automated or non-human means, whether through a bot, script, or otherwise." The headless docs (https://code.claude.com/docs/en/headless) document `claude -p` with subscription login as a supported scripting mode, which reads as the explicit permission for the CLI itself.

**Reading for jockey.** A user running jockey locally, which spawns the **unmodified** `claude` binary that the user signed into with their own subscription, fits the explicit carve-out ("an end user ... signing in to the unmodified Claude Code binary with their own Claude subscription"). It also fits ordinary individual use of a documented headless mode. Jockey must never:

- read, store, or proxy the OAuth token (no `~/.claude` credential access, no `CLAUDE_CODE_OAUTH_TOKEN` handling);
- offer its own "log in with Claude" flow;
- serve other users from one subscription;
- be distributed as a product that markets subscription use.

Many parallel critics are a gray area against "ordinary, individual usage" limits. Keep critic fan-out modest, use cheap models, and surface `rate_limit_event` utilization. This is my reading, not legal advice. Commercial distribution would need API keys or Anthropic approval.

## Exact command lines

```bash
# 1. one-shot stream (t1)
claude -p "Run 'ls' once with Bash, read a.txt with Read, then reply 'done'." \
  --output-format stream-json --verbose --model haiku \
  --allowedTools "Bash(ls*)" Read --strict-mcp-config

# 2. bidirectional session used for primary agents (t2-t6, t9)
claude -p --input-format stream-json --output-format stream-json --verbose \
  --model haiku --strict-mcp-config --permission-prompt-tool stdio [--replay-user-messages]

# 3. read-only critic (t7)
claude -p "<prompt>" --tools "Read,Grep,Glob" --permission-mode dontAsk \
  --permission-prompts none --output-format stream-json --verbose --model haiku \
  --strict-mcp-config --safe-mode --no-session-persistence

# 4. fork a session (t8)
claude -p --input-format stream-json --output-format stream-json --verbose --model haiku \
  --strict-mcp-config --resume <session-id> --fork-session
```

The Agent SDK (TS v0.3.284) spawns the CLI with `--output-format stream-json --verbose --input-format stream-json`, plus `--permission-prompt-tool stdio` when a `canUseTool` callback is set. It then sends `{"type":"control_request","request_id":...,"request":{"subtype":"initialize",...}}` first. Other flags the SDK passes: `--effort`, `--max-turns`, `--max-budget-usd`, `--task-budget`, `--thinking`, `--max-thinking-tokens`, `--model`, `--permission-prompts`, `--resume=<id>`, `--continue`, `--allowedTools`.

Permission modes (`--permission-mode`): `default` (alias `manual`; the `-p` default since v2.1.302), `acceptEdits`, `plan`, `auto` (classifier reviews actions; available on Pro/Max with Haiku 5.5, Sonnet/Opus 4.6+, Fable), `dontAsk`, `bypassPermissions` (never use).

Gotchas found:

- Standalone `sleep N` (large N) is blocked by Claude Code itself ("Blocked: standalone sleep 30 ...").
- `ls /tmp` outside the cwd was denied in default mode ("may only list files in the allowed working directories"). Use `--add-dir` for critics that must read the primary's worktree.
- Each new turn in a stream-json session re-emits `system/init`.
- With stream-json input, the process stays alive after `result` until stdin closes.

## Real JSON samples

### Stdin: user message (jockey to CLI)

```json
{"type":"user","message":{"role":"user","content":"STEERING NOTE: skip any remaining sleep commands, and end your final reply with the word BANANA."},"parent_tool_use_id":null,"session_id":"","uuid":"22222222-2222-4222-8222-222222222222"}
```

Optional fields: `"priority":"now"|"next"|"later"`, `"origin":{"kind":"human"}`, `"shouldQuery":false` (append context without triggering a turn), `"client_composed":true` (no `/` or `@` expansion).

### system/init (trimmed lists)

```json
{"type":"system","subtype":"init","cwd":"/tmp/work","session_id":"2276f772-...","tools":["Task","Bash","CronCreate","CronDelete","CronList","DesignSync","Edit","EnterWorktree","ExitWorktree","ListAgents","Monitor","NotebookEdit","PushNotification","Read","RemoteTrigger","ReportFindings","ScheduleWakeup","SendMessage","Skill","TaskStop","ToolSearch","WebFetch","WebSearch","Workflow","Write"],"mcp_servers":[],"model":"claude-haiku-5-5","permissionMode":"default","slash_commands":["..."],"apiKeySource":"none","claude_code_version":"2.1.312","output_style":"default","agents":["claude","Explore","general-purpose","Plan","..."],"capabilities":["interrupt_receipt_v1","interrupt_cancel_queued_v1","interrupt_send_now_v1","interrupt_requeue_taken_v1","msg_lifecycle_v1","request_marker_lists_v1","ask_user_question_unanswered_v1","lifecycle_wakeup_source_v1","sdk_mcp_tools_list_changed","sdk_mcp_manifests","mcp_read_resource_v1","mcp_tool_ui_meta_v1","ui_surface_v1","mcp_call_result_not_for_model_v1"],"memory_paths":{...},"fast_mode_state":"off","fast_mode_disabled_reason":"sdk_opt_in_required","per_turn_effort_active":true,"uuid":"2b75bd94-..."}
```

### assistant (tool_use)

```json
{"type":"assistant","message":{"model":"claude-haiku-5-5","id":"msg_011CgVTspGyZGhJdW9CWmvGn","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_01DPRTBQMy2GPLm4tmoWrqys","name":"Bash","input":{"command":"ls","description":"List files in current directory"},"caller":{"type":"direct"}}],"stop_reason":null,"usage":{"input_tokens":2,"cache_creation_input_tokens":12340,"cache_read_input_tokens":11665,"output_tokens":17,"service_tier":"standard"}},"parent_tool_use_id":null,"session_id":"2276f772-...","uuid":"6bec22b0-...","timestamp":"2026-10-28T22:58:58.921Z","request_id":"req_011CgVTsp9Xv6UTuUpw9nUkr"}
```

Parallel tool calls arrive as separate `assistant` events that share one `message.id`, one block each.

### user (tool_result)

```json
{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01DPRTBQMy2GPLm4tmoWrqys","type":"tool_result","content":"a.txt\nb.txt\nt1.err\nt1.jsonl","is_error":false}]},"parent_tool_use_id":null,"session_id":"2276f772-...","uuid":"0b09d510-...","timestamp":"2026-10-28T22:59:00.010Z","tool_use_result":{"stdout":"a.txt\nb.txt\nt1.err\nt1.jsonl","stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false},"tool_result_meta":[{"id":"toolu_01DPRTBQMy2GPLm4tmoWrqys","permission_decision":{"decision":"accept","source":"config","reason_type":"subcommandResults"}}]}
```

### rate_limit_event

```json
{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1793229600,"rateLimitType":"five_hour","overageStatus":"rejected","overageDisabledReason":"out_of_credits","isUsingOverage":false,"unifiedWindows":{"five_hour":{"utilization":0.05,"resetsAt":1793229600,"status":"allowed"},"seven_day":{"utilization":0.18,"resetsAt":1793538000,"status":"allowed"}}},"uuid":"a276cc14-...","session_id":"2276f772-..."}
```

### result

```json
{"type":"result","subtype":"success","is_error":false,"result":"done","num_turns":3,"stop_reason":"end_turn","terminal_reason":"completed","session_id":"2276f772-...","duration_ms":3198,"duration_api_ms":2133,"total_cost_usd":0.0029483,"usage":{"input_tokens":4,"cache_creation_input_tokens":12591,"cache_read_input_tokens":35670,"output_tokens":146},"modelUsage":{"claude-haiku-5-5":{"inputTokens":4,"outputTokens":146,"cacheReadInputTokens":35670,"cacheCreationInputTokens":12591,"costUSD":0.0029483,"contextWindow":1000000,"maxOutputTokens":128000,"costBasis":"list"}},"permission_denials":[],"subagent_stats":{"spawned":0},"queued_turn_count":0,"result_index":0}
```

`subtype` values seen: `success` and `error_during_execution` (after an interrupt). `terminal_reason` values seen: `completed` and `aborted_tools`.

### Mid-run injection timeline (t2, default priority, `--replay-user-messages`)

```
 0.39 OUT command_lifecycle  {"command_uuid":"1111...","state":"queued"}
 0.40 OUT command_lifecycle  {"command_uuid":"1111...","state":"started"}
 1.52 OUT assistant tool_use  Bash "sleep 3"
 1.52 IN  user "STEERING NOTE: skip any remaining sleep commands, ... BANANA."  uuid=2222...
 1.54 OUT command_lifecycle  {"command_uuid":"2222...","state":"queued"}      <- receipt
 5.43 OUT user tool_result (sleep finished)
 5.47 OUT user (isReplay:true) "STEERING NOTE ..."                             <- consumed
 5.47 OUT command_lifecycle  {"command_uuid":"2222...","state":"started"}
 6.90 OUT assistant tool_use  Bash "ls /tmp"      (sleeps skipped, as steered)
 8.61 OUT assistant text "... done BANANA"
 8.64 OUT command_lifecycle  {"command_uuid":"2222...","state":"completed"}
 8.64 OUT result success (one result: steering folded into the same turn)
 8.64 OUT command_lifecycle  {"command_uuid":"1111...","state":"completed"}
```

Raw lifecycle event:

```json
{"type":"command_lifecycle","command_uuid":"22222222-2222-4222-8222-222222222222","state":"queued","uuid":"8a963d7c-93a3-4b74-b576-f48023e6fd70","session_id":"9c057ea3-3e97-4a1f-ba2b-7888416228c6"}
```

### priority "now" (t6 without origin, t9 with `origin:{kind:"human"}`)

t6: message sent at 5.38 s while `ping -c 20` ran. It was `queued` at once, the ping ran to completion (22.3 s), the turn ended with `{"subtype":"success","terminal_reason":"aborted_tools","result":""}`, the first command was marked `cancelled`, and the "now" message `started` a new turn, which answered `STEERED`.

t9: message sent at 4.68 s. By 5.9 s the CLI had emitted `task_updated {"patch":{"is_backgrounded":true}}` and the tool_result "Command was moved to the background (ID: baxbg7bp8) so that a message that arrived while it was running can reach you; it was not interrupted." The pending parallel `ls` was rejected, the turn ended (`aborted_tools`), and the new turn ran TaskStop on the background ping and replied `STEERED` at 9.85 s.

### Interrupt (t5)

```json
{"type":"control_request","request_id":"int-1","request":{"subtype":"interrupt"}}
{"type":"control_response","response":{"subtype":"success","request_id":"int-1","response":{"still_queued":[]}}}
{"type":"system","subtype":"task_notification","task_id":"bxlliy9zc","tool_use_id":"toolu_01EeXp6AQ3BUZJr4QL6V64gY","status":"stopped","summary":"Ping localhost 25 times in foreground"}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"The user doesn't want to proceed with this tool use. The tool use was rejected ...","is_error":true,"tool_use_id":"toolu_01EeXp6AQ3BUZJr4QL6V64gY"}]},"tool_result_meta":[{"id":"toolu_01EeXp6AQ3BUZJr4QL6V64gY","non_execution_kind":"user-rejected"}]}
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}
{"type":"result","subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_tools","stop_reason":"tool_use","num_turns":3}
{"type":"command_lifecycle","command_uuid":"44444444-...","state":"cancelled"}
```

A follow-up user message then ran normally in the same session.

### Permission request routed to client (t3, `--permission-prompt-tool stdio`)

```json
{"type":"control_request","request_id":"252887eb-dbda-44e0-b0bf-e9b69ea7d3a5","request":{"subtype":"can_use_tool","tool_name":"Bash","display_name":"Bash","input":{"command":"touch x.txt","description":"Create empty file x.txt"},"description":"Create empty file x.txt","permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"touch x.txt"}],"behavior":"allow","destination":"localSettings"},{"type":"setMode","mode":"acceptEdits","destination":"session"}],"blocked_path":"/tmp/work/x.txt","tool_use_id":"toolu_017Yup4zJ9FMUHZ4wSVi9Ndg"}}
```

Client reply (deny) and the resulting tool_result:

```json
{"type":"control_response","response":{"subtype":"success","request_id":"252887eb-dbda-44e0-b0bf-e9b69ea7d3a5","response":{"behavior":"deny","message":"jockey critic: denied by policy (test)"}}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"jockey critic: denied by policy (test)","is_error":true,"tool_use_id":"toolu_017Yup4zJ9FMUHZ4wSVi9Ndg"}]},"tool_result_meta":[{"id":"toolu_017Yup4zJ9FMUHZ4wSVi9Ndg","non_execution_kind":"permission-rule","permission_decision":{"decision":"reject","source":"user_reject"}}]}
```

The allow form is `{"behavior":"allow","updatedInput":<input, optionally edited>}`. Commands on the read-only allowlist (e.g. `ping`, `ls` in cwd) never reach `can_use_tool`.

### Other control requests (t3)

```json
{"type":"control_request","request_id":"mode-1","request":{"subtype":"set_permission_mode","mode":"plan"}}
{"type":"control_response","response":{"subtype":"success","request_id":"mode-1","response":{"mode":"plan"}}}
{"type":"system","subtype":"status","status":null,"permissionMode":"plan"}

{"type":"control_request","request_id":"model-1","request":{"subtype":"set_model","model":"sonnet"}}
{"type":"control_response","response":{"subtype":"success","request_id":"model-1","response":{"model":"claude-sonnet-5-5"}}}
```

`initialize` response `.models[]` entry:

```json
{"value":"haiku","resolvedModel":"claude-haiku-5-5","displayName":"Haiku 5.5","description":"Fastest for quick answers","supportsEffort":true,"supportedEffortLevels":["low","medium","high","xhigh","max"],"supportsAdaptiveThinking":true,"supportsAutoMode":true,"supportsThinkingOff":false}
```

Models listed: `default`, `opus`, `fable`, `sonnet`, `haiku`, `claude-haiku-4-5-20251001`, `claude-sonnet-5`, `claude-opus-5`, `claude-fable-5`, `claude-opus-4-8`, `claude-opus-4-7`, `claude-opus-4-6`, `claude-sonnet-4-6`.

`get_usage` response (trimmed; the SDK method is named `usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET`):

```json
{"session":{"total_cost_usd":0.00571567,"model_usage":{"claude-haiku-5-5":{"inputTokens":4,"outputTokens":676,"costBasis":"list"}}},
 "subscription_type":"max","rate_limits_available":true,
 "rate_limits":{"five_hour":{"utilization":10,"resets_at":"2026-10-28T23:20:00.486229+00:00"},
  "seven_day":{"utilization":19,"resets_at":"2026-11-01T13:00:01.486243+00:00"},
  "extra_usage":{"is_enabled":false,"used_credits":0,"disabled_reason":"out_of_credits","credits_ever_enabled":true},
  "limits":[{"kind":"session","percent":10,"severity":"normal"},{"kind":"weekly_all","percent":19,"severity":"normal"},{"kind":"weekly_scoped","percent":9,"scope":{"model":{"display_name":"Fable"}}}],
  "spend":{"enabled":false,"disabled_reason":"out_of_credits","can_purchase_credits":false}}}
```

### /compact via stdin (t8)

```json
{"type":"system","subtype":"status","status":"compacting"}
{"type":"system","subtype":"status","status":null,"compact_result":"success"}
{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"manual","pre_tokens":24318,"post_tokens":7010,"cumulative_dropped_tokens":17308,"duration_ms":4924}}
{"type":"result","subtype":"success","result":"","num_turns":0}
```

### Long-running Bash task events

```json
{"type":"system","subtype":"task_started","task_id":"b1wc70vbw","tool_use_id":"toolu_01894oZp4wGVhcBQ3ThCkz19","description":"Wait 3 seconds","is_backgrounded":false,"task_type":"local_bash"}
{"type":"system","subtype":"task_notification","task_id":"b1wc70vbw","tool_use_id":"toolu_01894oZp4wGVhcBQ3ThCkz19","status":"completed","summary":"Wait 3 seconds"}
{"type":"system","subtype":"permission_denied","tool_name":"Bash","tool_use_id":"toolu_01PFsFEoR9CfiYnkmT5HDH7R","message":"ls in '/tmp' was blocked. For security, Claude Code may only list files in the allowed working directories for this session: '/tmp/work'."}
{"type":"system","subtype":"thinking_tokens","estimated_tokens":209,"estimated_tokens_delta":159,"user_message_uuid":"11111111-1111-4111-8111-111111111111"}
```

## Sources

- Headless / `-p`: https://code.claude.com/docs/en/headless
- CLI reference: https://code.claude.com/docs/en/cli-reference
- Permission modes: https://code.claude.com/docs/en/permission-modes
- Agent SDK TypeScript reference (`SDKUserMessage.priority`, `SDKRateLimitEvent`, `Query` methods): https://code.claude.com/docs/en/agent-sdk/typescript
- Streaming input: https://code.claude.com/docs/en/agent-sdk/streaming-vs-single-mode
- Sandboxing: https://code.claude.com/docs/en/sandboxing
- Model config (Fable billing note): https://code.claude.com/docs/en/model-config
- Costs / usage credits: https://code.claude.com/docs/en/costs
- Legal and compliance: https://code.claude.com/docs/en/legal-and-compliance
- Agent SDK overview: https://code.claude.com/docs/en/agent-sdk/overview
- Consumer Terms: https://www.anthropic.com/legal/consumer-terms
