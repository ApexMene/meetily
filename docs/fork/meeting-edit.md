# Merging meetings and removing parts of a meeting (fork)

Status: **implemented** · Scope: all platforms.
Contributed in LeoRock-13/meetily#4 and LeoRock-13/meetily#5, reworked during the merge.

## 1. Features

| Action | Where | What it does |
|---|---|---|
| Merge meetings | Sidebar: Ctrl/Cmd+click two or more meetings, then **Merge** | Creates a new meeting with the transcripts of the selected ones in chronological order, each block opened by `--- <title> ---`. The sources are not changed. The new meeting has no audio and no summary |
| Remove parts | Meeting page, under the transcript buttons: **Remove parts…** | Tick segments (greetings, off-topic talk) and remove them. The same time ranges are cut from the recording, so the remaining segments stay in sync |

## 2. Behaviour

- **Merge timeline.** The transcript view orders segments by `audio_start_time`, so the copies
  are placed on one continuous timeline: each source keeps its own times, shifted after the
  previous source (1 s left for the title line). Segments of old meetings without times keep
  their order, one second each.
- **Audio cut.** FFmpeg removes the selected ranges (`aselect`), re-encoding MP4/M4A as AAC
  192k and other containers with FFmpeg's default encoder. The first trim keeps the untouched
  recording as `audio.original.<ext>`; audio retention deletes it together with the audio.
- **Consistency.** The cut file is written next to the audio; the transcript changes and the
  replacement of the audio happen together, so a failure leaves both as they were. Later
  segments move back by the removed time, and so does the speaker timeline (`speakers.json`).
- A meeting without audio (merged, imported without folder, or audio already deleted) only loses
  the segments; their times are not changed.
- A meeting still waiting in the after-meeting queue cannot be trimmed: re-transcription would
  read the audio while it is cut.

## 3. Code

| Path | Role |
|---|---|
| `src-tauri/src/meeting_edit/merge.rs` | `meeting_edit_merge` command, timeline placement |
| `src-tauri/src/meeting_edit/trim.rs` | `meeting_edit_trim` command, ranges, FFmpeg cut, speaker timeline cut |
| `src/components/MeetingEdit/MergeBar.tsx` | Bar over the meeting list while meetings are selected |
| `src/components/MeetingEdit/TrimSegments.tsx` | Segment picker that replaces the transcript while removing parts |

Hooks in upstream files (search `Fork:`): `lib.rs` (module, commands),
`audio/retranscription.rs` (`find_audio_file` visible to the crate),
`components/Sidebar/index.tsx` (selection, Ctrl/Cmd+click, bar),
`components/MeetingDetails/TranscriptPanel.tsx` (button and picker).
In fork code: `speakers::write`, and retention also matches `audio.original.<ext>`.

## 4. Known limits

- The summary is not changed: regenerate it after merging or removing parts.
- Only loaded segments can be ticked; with long meetings use "Load more segments" first.
- The cut follows the audio frames, so each cut point is accurate to a few tens of milliseconds.
- The live transcript backup (`transcripts.live.json`) and the second model's text
  (`transcripts.alt.json`) keep the old timeline; they are only used right after the recording.

## 5. Tests

Unit tests: `cargo test -p meetily --lib -- meeting_edit speakers retention`.
Merging and cutting a real recording need the app.
