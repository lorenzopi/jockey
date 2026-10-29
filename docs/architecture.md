# Architecture

```
 cli.rs ──► workflows/{single,…} ──► providers::{ProviderAdapter, AgentSession}
   │              │                         │
   │              │                    codex.rs (app-server JSON-RPC) ─► `codex app-server` (own process group)
   │              ▼                         │
   │         RunCtx (config, profile,       │ EventKind via Emitter
   │         approver, live sessions)       ▼
   │                                   events bus (bounded mpsc)
   │                                        │
   │                              sessions::spawn_recorder  (single consumer)
   │                                 │ stamps seq, redacts, writes
   │                                 │   events.jsonl, raw/<agent>.jsonl
   │                                 ▼
   └──────────── render.rs ◄── broadcast (printer now, TUI and autosteer later)
```

## Rules

- **Workflows never name providers.** They branch on `Capabilities` only, and adding a provider means adding one adapter module.
- **One consumer owns ordering.** Emitters send `seq: 0`. The recorder stamps the sequence numbers, so the persisted log is strictly ordered even with many concurrent agents. It writes before it broadcasts, so a slow UI can drop display frames but never persisted events.
- **Nothing from a provider is dropped.** Every provider message is persisted as `Raw` in `raw/<agent>.jsonl`. Normalized events carry the common fields and the optional richer ones: plan, diff, reasoning summary, usage, rate limits.
- **Bounded everything.** The bus holds 4096 events, the RPC inbox 1024, and the output buffer 256. Producers wait under backpressure instead of growing memory.
- **Processes never outlive the run.**
  - Each CLI runs in its own process group.
  - Cancelling sends SIGTERM to the whole group, then SIGKILL, and `Drop` also SIGKILLs the group.
  - `RunCtx` keeps a list of every session it started, and `shutdown()` kills them all, including on Ctrl-C and timeout.
- **`&self` sessions.** `AgentSession` methods take `&self`, so one task can wait on `execute` while another calls `steer` or `interrupt`. This is the hook Autosteer needs.

## Trait mapping (spec names)

| Spec | Here |
|---|---|
| `probe()` | `ProviderAdapter::probe` |
| `get_capabilities()` | `ProviderAdapter::get_capabilities` |
| `start_session()` | `ProviderAdapter::start_session` returns `Arc<dyn AgentSession>` |
| `execute()` | `AgentSession::execute` (one turn, to completion) |
| `stream_events()` | the `Emitter` handed to `start_session`; every event for the session flows through it |
| `cancel()` | `AgentSession::cancel` (kills the process group; does **not** undo file changes) |
| `steer()`, `interrupt()` | `AgentSession`, defaulting to `Unsupported` |
| `resume()` | `SessionSpec::resume_id` on `start_session` |
| `list_models()`, `get_usage()` | `ProviderAdapter`, defaulting to `Unsupported` |

## Run directory

`~/.jockey/runs/<uuid-v7>/` (mode 0700; set `JOCKEY_HOME` to relocate the root):

- `meta.json`: prompt, mode, cwd, agent assignments with provider session ids, effective config, status, final output, last usage and rate limit per agent.
- `events.jsonl`: normalized events.
- `raw/<agent>.jsonl`: untranslated provider traffic.
- `raw/<agent>.stderr.log`: the CLI's stderr.

Every line is passed through `security::redact` before it is written.
