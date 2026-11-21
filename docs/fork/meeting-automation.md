# Automatic meetings and after-meeting work (fork)

Status: **implemented** · Scope: Windows (automation), all platforms (after-meeting work).
Contributed in LeoRock-13/meetily#1, reorganised during the merge.

Every feature here is **off by default** and turned on in Settings. Nothing changes for a user who
does not open these settings.

## 1. Features

| Setting | Where | What it does |
|---|---|---|
| Record meetings automatically | General › Automatic Meetings | Starts recording when a meeting app (Teams, Zoom, browser calls…) holds the microphone, stops when it lets go |
| Start/stop shortcut | General › Automatic Meetings | Global shortcut, usable from any app |
| Delete recording audio after N days | General › Automatic Meetings | Deletes only the audio files Meetily wrote (`audio.<ext>`, `recording.mp4`); transcripts are kept, and meetings still queued for re-transcription keep their audio. Asks before deleting more |
| Start with Windows | General › Automatic Meetings | `HKCU\…\Run` entry with `--autostart`: Meetily starts hidden in the tray |
| Latin script only (Parakeet) | Transcription | The decoder only emits Latin-script tokens, so the multilingual model cannot drift into Cyrillic or Greek |
| Re-transcribe when the meeting ends | Transcription › After the Meeting | Transcribes the whole recording again with the best local model, optionally a second model, then optionally Claude cleans it |
| Write recap and tasks in Notion | Transcription › After the Meeting | Claude Code writes the recap and the tasks through the user's claude.ai Notion connector |

While a meeting is recorded (automatically, by shortcut or from the tray) a small always-on-top bar
shows a timer, "Open transcript" and "Stop". It can be shrunk to a dot with the timer; its position
and size are remembered.

## 2. Code

| Path | Role |
|---|---|
| `src-tauri/src/meeting_automation/` | `detector` (registry polling of microphone use), `bar`, `shortcut`, `retention`, `autostart`; settings in `meeting-automation.json` |
| `src-tauri/src/post_meeting/` | `queue` (persistent jobs, worker, scan), `retranscribe`, `cleanup` (Claude), `jobs` (status window); settings in `post-meeting/settings.json` |
| `src-tauri/src/notion_agent/` | Recap in Notion: config, `.env`, prompt (`prompt.md` is an example to adapt) |
| `src-tauri/src/health.rs` | Hourly health check while the Notion agent is on (microphone, proxy, Claude Code login, Notion connector, failing jobs) |
| `src-tauri/src/custom_models/second_opinion.rs`, `parakeet_engine/ctc_model.rs` | Optional second model (NeMo CTC, e.g. Italian FastConformer) |
| `src-tauri/src/custom_models/latin_filter.rs` | Latin-only switch (`transcription-fork.json`) |
| `src-tauri/src/fork_store.rs` | Small JSON settings files: read once, written atomically |
| `src/components/ForkSettings.tsx` | The settings cards |
| `public/meeting-bar.*`, `public/post-meeting-jobs.html` | The bar and the status window (plain HTML; their window capabilities are registered at runtime, `tauri.conf.json` is untouched) |

Hooks in upstream files (search `Fork:`): `lib.rs` (modules, setup, commands, single-instance),
`tray.rs` (start/stop helpers, status menu entry), `summary/service.rs` (queue a meeting when its
summary is saved), `audio/recording_commands.rs` (a new recording cancels a re-transcription),
`audio/retranscription.rs` (second model), `parakeet_engine/model.rs` (Latin filter, thread count).

Claude Code runs go through `claude_code::run` (`Tools::None` for the clean-up,
`Tools::Connectors` with only the Notion tools for the recap; built-in tools are denied, user
settings and sessions are not loaded).

## 3. After-meeting queue

- A meeting becomes a job in `<app data>/post-meeting/queue/` when its summary is saved, and a
  periodic scan queues any recent meeting that was missed. Only meetings saved after a step was
  turned on are processed (at most 7 days back).
- One worker processes jobs in order and never while recording; a recording that starts cancels a
  running re-transcription, which is redone afterwards.
- Failures are retried with exponential backoff (1 min … 1 h); the user is notified after three
  failures. Finished jobs move to `done/`.
- The live transcript is saved to `transcripts.live.json` in the meeting folder before it is
  replaced; if re-transcription fails, is cancelled or finds no speech, the live transcript stays.
- "Retry" in the status window redoes only the steps not finished: a transcript already
  re-transcribed is not replaced again.
- Notion pages carry `{"meetily_id": …}` and the prompt updates instead of duplicating, so a retry
  never writes twice.

## 4. Files the user edits

In `<app data>` (`com.meetily.ai`):

- `post-meeting/transcript-cleanup.md`: clean-up instructions;
- `notion-agent/meeting-to-notion.md`: recap instructions (created from the example when the
  agent is turned on);
- `notion-agent/.env`: personal values for the prompt (`{{KEY}}`), `PROXY`/`NO_PROXY`,
  `TIMEZONE`, `PROMPT_FILE`, `REPAIR_TASK`;
- `notion-agent/settings.json`: `proxy`, `no_proxy`, `test_mode`, `min_seconds` (read at start; edits
  apply after a restart).

The first version (from the pull request) kept its settings in `auto-meeting.json` and
`notion-agent/config.json`, with every feature turned on. Those files are ignored, so everything
starts off; its queue and clean-up prompt in `notion-agent/` are moved to `post-meeting/` once.

## 5. Known limits

- The summary shown in Meetily is generated from the live transcript when the meeting ends, before
  the re-transcription. Regenerate it to use the improved text.
- Automatic recording, the shortcut-driven bar, retention scheduling and start with Windows are
  Windows only; the settings card is hidden elsewhere.
- A recording started in the instant a re-transcription is saving its result may not cancel it;
  the re-transcription then finishes while recording (it only competes for CPU).
- Parakeet sessions use at most 4 intra-op threads (as fast as all cores on hybrid laptop CPUs in
  the contributor's benchmark, and the machine stays responsive).

## 6. Tests

Unit tests: `cargo test -p meetily --lib -- meeting_automation post_meeting notion_agent health claude_code custom_models parakeet_engine::model`.
The flows themselves (detection, bar, Claude and Notion runs) need the app and a real meeting.
