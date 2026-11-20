# "Claude Code" summary provider (fork)

Status: **implemented** · Scope: meeting summaries.

## 1. Goal

Generate summaries with Claude through the user's own **Claude Code CLI**. The CLI is already logged in to the user's Claude account, so there is no API key to create or paste. It works whether Claude Code is installed natively or, on Windows, inside a WSL distribution.

The upstream "Claude" provider (Anthropic API with an API key) is unchanged and remains the right choice when an API key is preferred.

## 2. User experience

Settings → Summary → **Summarization Model** → "Claude Code (Claude account, no API key)". The card shows:

| Field | Meaning |
|---|---|
| Runs in | `Automatic` (default), `This computer`, or `WSL · <distribution>` (one entry per installed distribution, Windows only) |
| Model | `Sonnet`, `Opus`, `Haiku`: CLI aliases that always resolve to the latest model of the family |
| Path to claude | Optional, for unusual installs; only shown when an environment is chosen explicitly |
| Status | Version, environment and login state, refreshed with **Check** |

- **Where Claude Code runs** describes this computer. It is saved as soon as it changes.
- **The model** is saved with the modal's Save button, like every other provider.
- **Check** finds `claude` and runs `claude --version` and `claude auth status`. It never sends a prompt, so it costs no usage.

## 3. How it works

Code: `frontend/src-tauri/src/claude_code/`.

| File | Role |
|---|---|
| `mod.rs` | types, in-memory state (config + cached detection), `generate()` used by the summary pipeline |
| `config.rs` | persistence in `settings.claudeCodeConfig` (JSON), validation |
| `detect.rs` | finds `claude` natively or in a WSL distribution, lists distributions |
| `cli.rs` | builds the command line, runs it (stdin, timeout, cancellation), parses the output |
| `commands.rs` | Tauri commands `claude_code_get_settings`, `claude_code_save_config`, `claude_code_check` |

### One CLI call, two launchers

The `claude` arguments are the same everywhere. Only the launcher differs:

```text
native:  <claude> --print --output-format json --model sonnet --system-prompt "…" --tools "" …
WSL:     wsl.exe -d <distro> --cd /tmp --exec env -u ANTHROPIC_API_KEY <claude> --print …
```

The summary is run in isolation:

- `--tools ""`, `--strict-mcp-config` and `--disable-slash-commands` disable tools, MCP servers and skills.
- `--setting-sources ""` ignores user and project settings, so their hooks do not run.
- `--no-session-persistence` means no session file is written.
- The working directory is neutral (the temp directory, or `/tmp` in WSL).
- `ANTHROPIC_API_KEY` is removed, so the account login is always used.

**The prompt always goes through stdin.** The system prompt and the transcript are sent as `<instructions>…</instructions>` followed by the user content. The arguments are only short, fixed words. This means:

- transcripts never appear in process listings;
- there is no command-line length limit;
- there are no quoting problems where arguments are re-parsed: by `cmd.exe` for npm's `claude.cmd`, and by `wsl.exe`.

A unit test enforces that the arguments are plain words. A live test checks that they reach WSL intact.

### Detection

Apps started from the desktop do not inherit the PATH of the user's shell, so detection checks several places:

- **Native:** first `which claude`, then the installers' default locations (`~/.local/bin`, `~/.claude/local`, Homebrew and `/usr/local/bin` on macOS and Linux).
- **WSL:** `wsl.exe --list --quiet` (UTF-16 output; Docker's helper distributions are skipped). In the chosen distribution a small `sh -s` script, sent through stdin so nothing needs quoting, checks the default locations. It then falls back to the user's login shell (`$SHELL -lic 'command -v claude'`), which works with bash, zsh, fish, nvm and npm prefixes.
- **Automatic:** native first, then the first WSL distribution that has `claude`.

The result is cached until the configuration changes. If the executable disappears (spawn returns NotFound), detection runs again on the next summary.

### Errors

The final `--output-format json` message is parsed:

- with `is_error: false` the `result` text becomes the summary;
- otherwise the message is shown to the user;
- authentication failures (logged out, expired OAuth session) become "Claude Code is not logged in … in WSL (archlinux). Open a terminal there, run `claude` and use /login."

Timeouts are 10 minutes for a generation and 60 s for `--version` / `auth status` (a stopped distribution has to start first). Cancelling a summary kills the process.

## 4. Changes to upstream files (marked `Fork:`)

- `lib.rs`: `pub mod claude_code`, `claude_code::init` after the database, the three commands.
- `summary/llm_client.rs`: `LLMProvider::ClaudeCode` (`"claude-code"`), handled before the HTTP providers.
- `summary/service.rs`: no API key lookup for `ClaudeCode`. Like the other cloud providers, the transcript is sent in a single pass, without chunking.
- `database/repositories/setting.rs`: `claude-code` has no API key column.
- Migration `20261120000000_add_claude_code_config.sql`: column `settings.claudeCodeConfig`.
- Frontend:
  - `ModelSettingsModal.tsx`: new entry and card;
  - `ConfigContext.tsx`, `configService.ts`: provider type.
  - New files: `src/lib/claude-code.ts` and `src/components/ClaudeCode/ClaudeCodeSettings.tsx`.

## 5. Tests

- Unit tests: `cargo test -p meetily --lib claude_code`. They cover:
  - the same CLI arguments for every environment;
  - plain-word arguments and the prompt sent through stdin;
  - output parsing: success, expired login, empty answer, crash, usage limit;
  - `auth status`;
  - distribution list parsing (UTF-16 and UTF-8);
  - path extraction from noisy shell output;
  - config validation and JSON shape.
- Live tests (`#[ignore]`):
  - `cargo test -p meetily --lib claude_code::detect::live -- --ignored --nocapture`: arguments across the `wsl.exe` boundary, detection in every distribution (Windows with WSL).
  - `cargo test -p meetily --lib claude_code::live -- --ignored --nocapture`: end-to-end `generate()`. When logged in it sends one tiny prompt with Haiku; otherwise it checks the "not logged in" message.

## 6. Notes and limits

- **Data and usage.** Transcripts are sent to Anthropic under the terms of the logged-in account and count toward its usage limits. Built-in AI remains the offline option.
- **Terms.** Each user runs their own Claude Code with their own account. Anthropic does not allow third-party products to offer Claude account login. For shared or commercial use, prefer the API-key "Claude" provider.
- **Startup cost.** Each summary starts the CLI, a few seconds; a stopped WSL distribution adds its boot time.
- **The legacy settings modal** (`app/_components/SettingsModal.tsx`) does not list this provider, just as it does not list "Custom Server (OpenAI)".
