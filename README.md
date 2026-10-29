<div align="center">

<img src="docs/logo.svg" alt="jockey: a racehorse and jockey at full gallop, in ASCII" width="320">

# jockey

**one saddle, many coding agents**

A terminal app that rides the official coding-agent CLIs and makes them work together.  
One agent works while others check, judge and steer it.

[Website](https://lorenzopi.github.io/jockey/) · [Docs](docs/) · [Install](#install)

</div>

---

## Why

Each coding agent is good on its own. jockey puts them behind one interface and gives you four ways to combine them.

| Mode | What happens |
|---|---|
| **single** | One agent, one conversation. Type while it works to steer it. |
| **cascade** | The agent attempts, your checks run, a judge reviews. If it's rejected, it retries with the feedback. |
| **critique** | The agent drafts, critics review in parallel, and the agent revises with their findings. |
| **autosteer** | Critics watch the agent while it works and correct it mid-turn. |

jockey drives the CLIs you already use and are signed in to. It never sees your credentials and never uses API keys.

## Install

You need at least one of `codex`, `claude` or `grok`, installed and signed in.

```sh
brew install lorenzopi/tap/jockey
```

Without Homebrew, on macOS (installs Rust if it's missing):

```sh
curl -fsSL https://raw.githubusercontent.com/lorenzopi/jockey/main/install.sh | sh
```

Anywhere with Rust 1.85+:

```sh
cargo install --locked --git https://github.com/lorenzopi/jockey
```

Then:

```sh
jockey doctor          # checks the CLIs, config and quota; uses no model
jockey                 # opens the TUI in the current folder
jockey --agent mock    # tries the UI with a built-in fake agent
```

jockey shows on its home screen when an update is out. `jockey update` installs it the same way you installed jockey.

## The TUI

The conversation scrolls, the prompt stays at the bottom, and the footer shows the folder, mode, model, tokens, context and plan limits.

In multi-agent modes the screen splits: the agent on one side, the critics' reviews and findings on the other.

Every conversation is saved, and you can reopen and continue it.

| Key | Action |
|---|---|
| Enter | send; while the agent works, steer it |
| Alt+Enter | queue a follow-up |
| Esc | interrupt (file changes stay) |
| Shift+Enter | newline |
| Shift+Tab | cycle permissions: workspace-write, auto, yolo, read-only |
| Ctrl+L · Ctrl+Y | pick lab, model and thinking · cycle thinking |
| Ctrl+O · Ctrl+T | tool output and diffs · reasoning |
| Ctrl+Z · Ctrl+\\ | zoom a pane · stack or split panes |
| Ctrl+S | pick critic findings or suggestions |
| `/` · `!cmd` | commands · run a shell command |
| Ctrl+C · Ctrl+D | interrupt or clear · quit |

`/help` lists every key and command.

## Command line

```sh
jockey run --agent codex "add a --verbose flag"
jockey run --agent claude:sonnet@high "fix the failing test"
jockey run --mode critique --agent codex --critics claude,grok "implement the parser"
jockey run --format json "..." | jq .
jockey resume last "now add tests"
```

The answer goes to stdout and activity to stderr, so `> answer.md` captures just the answer.

Exit codes: `0` completed, `1` failed, `130` interrupted.

Ctrl+C stops the agent and everything it started. It doesn't undo file changes.

## Safety

**No surprise bills.** jockey won't run past your subscription into paid credits unless you pass `--allow-credits`.

**Reviewers can't write.** Judges and critics always run read-only, in a snapshot of the workspace.

**Critics can't widen permissions.** Advice to bypass permissions, use `sudo`, pipe to `sh` or add API keys is rejected.

| Profile | The agent may |
|---|---|
| `read-only` | read |
| `workspace-write` (default) | edit the working folder; anything else asks |
| `auto` | the same, with the provider's own reviewer approving routine actions |
| `full` (yolo) | anything, without asking |

On the command line, `full` needs `--dangerously-allow-all`. In the TUI, Shift+Tab switches the running agent's profile immediately.

## Configuration

`~/.jockey/config.toml`, overlaid by a project's `.jockey.toml`. A project's file can't change executable paths, billing or approval rules.

Models you pick in the app are saved as your defaults. The rest of the file is left as you wrote it.

```toml
[defaults]
agent = "codex:gpt-6.1-sol@medium"
profile = "workspace-write"

[critique]
focus = "auto"          # coding, general, or auto
ask = false             # pause after each review to pick findings

[approvals]
allow_commands = ["cargo test", "cargo check"]
```

`jockey config` shows the effective settings and where each one came from.

## Docs

- [Workflows](docs/workflows.md): cascade and critique in detail
- [Autosteer](docs/autosteer.md): checkpoints, the steering controller, policies
- [Architecture](docs/architecture.md): event bus, adapters, sessions, snapshots
- [Providers](docs/providers.md): what each CLI supports, and how it was verified

## Develop

```sh
cargo fmt && cargo clippy --all-targets && cargo test
```

The tests use no network and no model usage. Each adapter is tested against recorded protocol traces.

`JOCKEY_DEMO_MOCK=1` makes `codex`, `claude` and `grok` run the scripted mock agent under their real names. It's for recording screenshots.

## License

[MIT](LICENSE)
