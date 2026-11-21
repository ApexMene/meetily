'use client';

// Fork: settings for automatic meetings (General tab) and the work after the meeting
// (Transcription tab). Backed by automation_settings_* (meeting_automation),
// post_meeting_settings_* (post_meeting) and latin_filter_* (custom_models::latin_filter).
// Every feature is off until turned on here; changes are saved immediately.

import React, { useCallback, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { toast } from 'sonner';
import { Activity, ArrowRight, Download, FolderOpen, Loader2 } from 'lucide-react';
import { Switch } from '@/components/ui/switch';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';

interface AutomationSettings {
  supported: boolean;
  enabled: boolean;
  shortcut: string;
  audioRetentionDays: number;
  startWithWindows: boolean;
}
type Retranscribe = 'off' | 'model' | 'full';
interface PostMeeting {
  retranscribe: Retranscribe;
  model: string;
  secondOpinion: string;
  cleanModel: string;
  notionEnabled: boolean;
  recapModel: string;
}
interface OpinionModel { name: string; label: string; installed: boolean; problem?: string | null }

const AUTO = '__auto__';
const NONE = '__none__';
const OTHER = '__other__';
const PARAKEET = 'parakeet:';
const CARD = 'bg-white rounded-lg border border-gray-200 p-6 shadow-sm';
const CLAUDE_MODELS: [string, string][] = [
  ['haiku', 'Claude Haiku (fastest)'],
  ['sonnet', 'Claude Sonnet'],
  ['opus', 'Claude Opus (most accurate, slower)'],
];
const withoutNote = (label: string) => label.replace(/ \(.*\)$/, '');
const claudeName = (model: string) => withoutNote(CLAUDE_MODELS.find(([v]) => v === model)?.[1] || model);

/** Loads settings through `get` and saves a patch of them through `set`. */
function useSettings<T>(get: string, set: string) {
  const [settings, setSettings] = useState<T | null>(null);
  useEffect(() => {
    invoke<T>(get).then(setSettings).catch((e) => toast.error('Could not load the settings', { description: String(e) }));
  }, [get]);
  const save = useCallback(async (patch: Partial<T>) => {
    try {
      const next = { ...(await invoke<T>(get)), ...patch };
      await invoke(set, { settings: next });
      setSettings(next);
      toast.success('Setting saved');
    } catch (e) {
      toast.error('Not saved', { description: String(e) });
    }
  }, [get, set]);
  return { settings, save };
}

function Row({ title, help, children }: { title: string; help?: React.ReactNode; children: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-6 py-4 border-t border-gray-100 first:border-t-0 first:pt-0">
      <div className="flex-1 min-w-0">
        <div className="font-medium text-gray-900">{title}</div>
        {help && <div className="text-sm text-gray-600 mt-0.5">{help}</div>}
      </div>
      <div className="shrink-0">{children}</div>
    </div>
  );
}

/** Transcription model picker shared by the main and the second model: the same Parakeet list
 *  (built-in and custom models), plus the extra entries each row needs. */
function TranscriptionModelSelect({ value, onChange, parakeet, first, extra = [], exclude }: {
  value: string; onChange: (v: string) => void; parakeet: string[];
  first: [string, string]; extra?: [string, string][]; exclude?: string;
}) {
  const items: [string, string][] = [first, ...parakeet.filter((m) => m !== exclude).map((m) => [m, m] as [string, string]), ...extra];
  const known = items.some(([v]) => v === value);
  return (
    <Select value={value} onValueChange={onChange}>
      <SelectTrigger className="w-80"><SelectValue /></SelectTrigger>
      <SelectContent>
        {items.map(([v, label]) => <SelectItem key={v} value={v}>{label}</SelectItem>)}
        {!known && <SelectItem value={value}>{value} (not found)</SelectItem>}
      </SelectContent>
    </Select>
  );
}

/** Claude model picker: aliases always point at the latest model of that family; "Other model…"
 *  accepts any model id the Claude CLI knows. */
function ClaudeModelSelect({ value, onChange, recommended }: { value: string; onChange: (v: string) => void; recommended: string }) {
  const [custom, setCustom] = useState(false);
  const [text, setText] = useState('');
  const isAlias = CLAUDE_MODELS.some(([v]) => v === value);
  if (custom) {
    const done = () => { const v = text.trim(); setCustom(false); if (v) onChange(v); };
    return (
      <Input autoFocus placeholder="e.g. claude-opus-5-5" value={text} onChange={(e) => setText(e.target.value)} onBlur={done}
        onKeyDown={(e) => { if (e.key === 'Enter') done(); if (e.key === 'Escape') setCustom(false); }} className="w-80" />
    );
  }
  return (
    <Select value={value} onValueChange={(v) => { if (v === OTHER) { setText(isAlias ? '' : value); setCustom(true); } else onChange(v); }}>
      <SelectTrigger className="w-80"><SelectValue /></SelectTrigger>
      <SelectContent>
        {CLAUDE_MODELS.map(([v, label]) => (
          <SelectItem key={v} value={v}>{v === recommended ? withoutNote(label) + ' (recommended)' : label}</SelectItem>
        ))}
        {!isAlias && <SelectItem value={value}>{value}</SelectItem>}
        <SelectItem value={OTHER}>Other model…</SelectItem>
      </SelectContent>
    </Select>
  );
}

/** "Ctrl+Alt+M" from a key press; null when the combination is not usable. */
function shortcutFromKey(e: React.KeyboardEvent): string | null {
  const parts: string[] = [];
  if (e.ctrlKey) parts.push('Ctrl');
  if (e.altKey) parts.push('Alt');
  if (e.shiftKey) parts.push('Shift');
  if (e.metaKey) parts.push('Super');
  if (!parts.length || (parts.length === 1 && parts[0] === 'Shift')) return null;
  parts.push(e.code.replace(/^Key/, '').replace(/^Digit/, ''));
  return parts.join('+');
}

/** General tab: automatic recording, shortcut, audio retention, start with Windows (Windows only). */
export function MeetingAutomationSettings() {
  const { settings, save } = useSettings<AutomationSettings>('automation_settings_get', 'automation_settings_set');
  const [shortcut, setShortcut] = useState('');
  const [days, setDays] = useState('');
  useEffect(() => {
    if (settings) { setShortcut(settings.shortcut); setDays(String(settings.audioRetentionDays)); }
  }, [settings]);
  if (!settings?.supported) return null;

  const onShortcutKey = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Tab') return;
    e.preventDefault();
    if (e.key === 'Backspace' || e.key === 'Delete') { setShortcut(''); save({ shortcut: '' }); return; }
    if (['Control', 'Alt', 'Shift', 'Meta'].includes(e.key)) return;
    const value = shortcutFromKey(e);
    if (!value) { toast.error('Use at least Ctrl or Alt'); return; }
    setShortcut(value);
    save({ shortcut: value });
  };
  const saveDays = () => {
    const n = Math.max(0, Math.min(3650, parseInt(days || '0', 10) || 0));
    // Deleting audio cannot be undone: ask when the setting starts deleting more than before.
    const deletesMore = n > 0 && (settings.audioRetentionDays === 0 || n < settings.audioRetentionDays);
    if (n === settings.audioRetentionDays || (deletesMore && !window.confirm(
      `The audio of meetings older than ${n} days will be deleted permanently (transcripts and summaries are kept). Continue?`))) {
      setDays(String(settings.audioRetentionDays));
      return;
    }
    setDays(String(n));
    save({ audioRetentionDays: n });
  };

  return (
    <div className={CARD + ' mt-6'}>
      <h3 className="text-lg font-semibold text-gray-900 mb-4">Automatic Meetings</h3>
      <Row title="Record meetings automatically"
        help="Starts recording when Teams, Zoom, a browser call or another meeting app uses the microphone, and stops when it lets go. A small bar shows the timer and a stop button.">
        <Switch checked={settings.enabled} onCheckedChange={(v) => save({ enabled: v })} />
      </Row>
      <Row title="Start/stop shortcut" help="Works from any app. Click the field and press the keys; Delete removes it.">
        <Input value={shortcut} readOnly onKeyDown={onShortcutKey} placeholder="none" className="w-40 text-center font-mono" />
      </Row>
      <Row title="Delete recording audio after" help="Only the audio files Meetily wrote; transcripts and summaries are kept. 0 = never.">
        <div className="flex items-center gap-2">
          <Input type="number" min={0} max={3650} value={days} onChange={(e) => setDays(e.target.value)} onBlur={saveDays}
            onKeyDown={(e) => { if (e.key === 'Enter') (e.target as HTMLInputElement).blur(); }} className="w-20 text-right" />
          <span className="text-sm text-gray-600">days</span>
        </div>
      </Row>
      <Row title="Start with Windows" help="Starts hidden, with the icon next to the clock.">
        <Switch checked={settings.startWithWindows} onCheckedChange={(v) => save({ startWithWindows: v })} />
      </Row>
    </div>
  );
}

/** Transcription tab: Latin-only output, then the work after the meeting. */
export function PostMeetingSettings() {
  const { settings, save } = useSettings<PostMeeting>('post_meeting_settings_get', 'post_meeting_settings_set');
  const [latinOnly, setLatinOnly] = useState<boolean | null>(null);
  const [parakeet, setParakeet] = useState<string[]>([]);
  const [opinions, setOpinions] = useState<OpinionModel[]>([]);
  const [downloading, setDownloading] = useState<string | null>(null);
  const [showMore, setShowMore] = useState(false);

  const loadOpinions = useCallback(() => invoke<OpinionModel[]>('second_opinion_list').then(setOpinions).catch(() => {}), []);
  useEffect(() => {
    invoke<boolean>('latin_filter_get').then(setLatinOnly).catch(() => setLatinOnly(false));
    invoke<{ name: string; status: unknown }[]>('parakeet_get_available_models')
      .then((ms) => setParakeet((ms || []).filter((m) => m.status === 'Available').map((m) => m.name)))
      .catch(() => {});
    loadOpinions();
  }, [loadOpinions]);
  if (!settings || latinOnly === null) return null;

  const saveLatin = async (enabled: boolean) => {
    try { await invoke('latin_filter_set', { enabled }); setLatinOnly(enabled); toast.success('Setting saved'); }
    catch (e) { toast.error('Not saved', { description: String(e) }); }
  };
  const download = async (m: OpinionModel) => {
    setDownloading(m.name);
    try {
      await invoke('second_opinion_install', { name: m.name });
      await loadOpinions();
      await save({ secondOpinion: m.name });
    } catch (e) {
      toast.error('Download failed', { description: String(e) });
    } finally { setDownloading(null); }
  };
  const openFolder = (which: 'cleanup' | 'notion') =>
    invoke('post_meeting_open_folder', { which }).catch((e) => toast.error('Cannot open the folder', { description: String(e) }));

  const on = settings.retranscribe !== 'off';
  const claude = settings.retranscribe === 'full';
  const installed = opinions.filter((m) => m.installed);
  const missing = opinions.filter((m) => !m.installed);
  const opinion = settings.secondOpinion;
  const secondValue = !opinion ? NONE : opinion.startsWith(PARAKEET) ? opinion.slice(PARAKEET.length) : opinion;
  const mainResolved = settings.model || parakeet.find((m) => m.toLowerCase().includes('ultra'));
  const opinionLabel = opinion.startsWith(PARAKEET) ? secondValue : installed.find((m) => m.name === opinion)?.label;
  const steps: [string, boolean][] = [
    ['Live transcript', true],
    ['Re-transcribe: ' + (settings.model ? withoutNote(settings.model) : 'best model') + (claude && opinionLabel ? ' + ' + withoutNote(opinionLabel) : ''), on],
    [claudeName(settings.cleanModel) + ' cleans up the text', claude],
    [claudeName(settings.recapModel) + ' writes recap and tasks in Notion', settings.notionEnabled],
  ];

  return (
    <>
      <div className={CARD + ' mt-6'}>
        <Row title="Latin script only (Parakeet)"
          help="Stops the multilingual model from drifting into Cyrillic or Greek on short or noisy chunks. Leave it off for meetings in other scripts.">
          <Switch checked={latinOnly} onCheckedChange={saveLatin} />
        </Row>
      </div>

      <div className={CARD + ' mt-6'}>
        <h3 className="text-lg font-semibold text-gray-900 mb-1">After the Meeting</h3>
        <p className="text-sm text-gray-600">
          Optional work done in the background once a meeting is saved, one meeting at a time and never while recording.
          Re-transcribing takes about 10-20 minutes per hour of meeting. The summary shown in Meetily is generated from the
          live transcript; regenerate it afterwards to use the improved text.
        </p>

        <ol className="flex flex-wrap items-center gap-2 my-5">
          {steps.map(([text, active], i) => (
            <li key={i} className="flex items-center gap-2">
              {i > 0 && <ArrowRight className="w-3.5 h-3.5 text-gray-400" />}
              <span className={'text-xs px-2.5 py-1 rounded-full border ' + (active ? 'bg-blue-50 border-blue-200 text-blue-800' : 'bg-gray-50 border-gray-200 text-gray-400 line-through')}>{text}</span>
            </li>
          ))}
        </ol>

        <Row title="Re-transcribe when the meeting ends" help="Only meetings recorded after you turn it on.">
          <Select value={settings.retranscribe} onValueChange={(v) => save({ retranscribe: v as Retranscribe })}>
            <SelectTrigger className="w-80"><SelectValue /></SelectTrigger>
            <SelectContent>
              <SelectItem value="off">No, keep the live transcript</SelectItem>
              <SelectItem value="model">Yes, with a local model</SelectItem>
              <SelectItem value="full">Yes, then Claude cleans it up</SelectItem>
            </SelectContent>
          </Select>
        </Row>

        {on && (
          <Row title="Main model" help="Automatic picks an installed Parakeet model with “ultra” in its name, otherwise the live model.">
            <TranscriptionModelSelect value={settings.model || AUTO} parakeet={parakeet}
              first={[AUTO, 'Automatic (best installed)']}
              onChange={(v) => save({ model: v === AUTO ? '' : v })} />
          </Row>
        )}

        {claude && (
          <>
            <Row title="Second model" help="Optional: a second reading, for instance a model specialised in the meeting language. Claude merges the two.">
              <TranscriptionModelSelect value={secondValue} parakeet={parakeet} exclude={mainResolved}
                first={[NONE, 'None']}
                extra={installed.map((m) => [m.name, m.label] as [string, string])}
                onChange={(v) => save({ secondOpinion: v === NONE ? '' : installed.some((m) => m.name === v) ? v : PARAKEET + v })} />
            </Row>
            {missing.length > 0 && installed.length > 0 && !showMore && (
              <div className="flex justify-end pb-4 -mt-2">
                <button className="text-xs text-blue-600 hover:underline" onClick={() => setShowMore(true)}>Other models…</button>
              </div>
            )}
            {missing.length > 0 && (installed.length === 0 || showMore) && (
              <div className="flex flex-wrap justify-end gap-2 pb-4">
                {missing.map((m) => (
                  <Button key={m.name} variant="outline" size="sm" disabled={!!downloading} onClick={() => download(m)}>
                    {downloading === m.name ? <Loader2 className="w-4 h-4 animate-spin" /> : <Download className="w-4 h-4" />}
                    {downloading === m.name ? 'Downloading…' : 'Download ' + m.label}
                  </Button>
                ))}
              </div>
            )}
            <Row title="Text clean-up with Claude Code" help={<>Mechanical work: Haiku is enough. Instructions: <code>transcript-cleanup.md</code>.</>}>
              <div className="flex items-center gap-2">
                <ClaudeModelSelect value={settings.cleanModel} recommended="haiku" onChange={(v) => save({ cleanModel: v })} />
                <Button variant="outline" size="icon" title="Open the instructions folder" onClick={() => openFolder('cleanup')}>
                  <FolderOpen className="w-4 h-4" />
                </Button>
              </div>
            </Row>
          </>
        )}

        <Row title="Write recap and tasks in Notion"
          help={<>Claude Code uses your claude.ai Notion connector. Fill in <code>.env</code> and adapt <code>meeting-to-notion.md</code> in the folder first.</>}>
          <Switch checked={settings.notionEnabled} onCheckedChange={(v) => save({ notionEnabled: v })} />
        </Row>
        {settings.notionEnabled && (
          <Row title="Recap model" help="Thinking work: Sonnet is the right balance.">
            <div className="flex items-center gap-2">
              <ClaudeModelSelect value={settings.recapModel} recommended="sonnet" onChange={(v) => save({ recapModel: v })} />
              <Button variant="outline" size="icon" title="Open the Notion agent folder" onClick={() => openFolder('notion')}>
                <FolderOpen className="w-4 h-4" />
              </Button>
            </div>
          </Row>
        )}
        {(on || settings.notionEnabled) && (
          <Row title="Status" help="Progress of each meeting, retries and the health check.">
            <Button variant="outline" size="sm" onClick={() => invoke('post_meeting_open_jobs')}>
              <Activity className="w-4 h-4" />Open
            </Button>
          </Row>
        )}
      </div>
    </>
  );
}
