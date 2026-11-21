'use client';

// Fork: settings for automatic meetings (General tab) and the post-meeting pass (Transcription tab).
// Backed by auto_settings_* (auto-meeting.json) and agent_models_* (notion-agent/config.json).

import React, { useCallback, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { toast } from 'sonner';
import { Download, FolderOpen, Loader2, ArrowRight } from 'lucide-react';
import { Switch } from '@/components/ui/switch';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';

interface AutoSettings {
  shortcut: string;
  audio_retention_days: number;
  start_with_windows: boolean;
  second_pass: string;
  second_pass_model: string;
  second_opinion: string;
}
interface AgentModels { recap_model: string; clean_model: string }
interface OpinionModel { name: string; label: string; installed: boolean; problem?: string | null }

const AUTO = '__auto__';
const NONE = '__none__';
const CARD = 'bg-white rounded-lg border border-gray-200 p-6 shadow-sm';

function useAutoSettings() {
  const [settings, setSettings] = useState<AutoSettings | null>(null);
  useEffect(() => {
    invoke<AutoSettings>('auto_settings_get').then(setSettings).catch((e) => toast.error('Could not load settings', { description: String(e) }));
  }, []);
  const save = useCallback(async (patch: Partial<AutoSettings>) => {
    const current = await invoke<AutoSettings>('auto_settings_get');
    const next = { ...current, ...patch };
    try {
      await invoke('auto_settings_set', { settings: next });
      setSettings(next);
      toast.success('Setting saved');
    } catch (e) {
      toast.error('Not saved', { description: String(e) });
    }
  }, []);
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

/** General tab: shortcut, audio retention, start with Windows. */
export function MeetingAutomationSettings() {
  const { settings, save } = useAutoSettings();
  const [shortcut, setShortcut] = useState('');
  const [days, setDays] = useState('');
  useEffect(() => {
    if (settings) { setShortcut(settings.shortcut); setDays(String(settings.audio_retention_days)); }
  }, [settings]);
  if (!settings) return null;

  const onShortcutKey = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Tab') return;
    e.preventDefault();
    if (e.key === 'Backspace' || e.key === 'Delete') { setShortcut(''); save({ shortcut: '' }); return; }
    if (['Control', 'Alt', 'Shift', 'Meta'].includes(e.key)) return;
    const parts: string[] = [];
    if (e.ctrlKey) parts.push('Ctrl');
    if (e.altKey) parts.push('Alt');
    if (e.shiftKey) parts.push('Shift');
    if (e.metaKey) parts.push('Super');
    if (!parts.length || (parts.length === 1 && parts[0] === 'Shift')) { toast.error('Use at least Ctrl or Alt'); return; }
    parts.push(e.code.replace(/^Key/, '').replace(/^Digit/, ''));
    const value = parts.join('+');
    setShortcut(value);
    save({ shortcut: value });
  };
  const saveDays = () => {
    const n = Math.max(0, Math.min(3650, parseInt(days || '0', 10) || 0));
    setDays(String(n));
    if (n !== settings.audio_retention_days) save({ audio_retention_days: n });
  };

  return (
    <div className={CARD}>
      <h3 className="text-lg font-semibold text-gray-900 mb-1">Automatic Meetings</h3>
      <p className="text-sm text-gray-600 mb-4">Meetily starts recording on its own when Teams, Zoom or a browser call uses the microphone.</p>
      <Row title="Start/stop shortcut" help="Click the field and press the keys. Delete removes it.">
        <Input value={shortcut} readOnly onKeyDown={onShortcutKey} placeholder="none" className="w-40 text-center font-mono" />
      </Row>
      <Row title="Delete audio after" help="Transcripts and recaps are kept. 0 = never.">
        <div className="flex items-center gap-2">
          <Input type="number" min={0} max={3650} value={days} onChange={(e) => setDays(e.target.value)} onBlur={saveDays}
            onKeyDown={(e) => { if (e.key === 'Enter') (e.target as HTMLInputElement).blur(); }} className="w-20 text-right" />
          <span className="text-sm text-gray-600">days</span>
        </div>
      </Row>
      <Row title="Start with Windows" help="Starts hidden, with the icon next to the clock.">
        <Switch checked={settings.start_with_windows} onCheckedChange={(v) => save({ start_with_windows: v })} />
      </Row>
    </div>
  );
}

/** Transcription tab: re-transcription after the meeting, second model, Claude. */
export function PostMeetingSettings() {
  const { settings, save } = useAutoSettings();
  const [models, setModels] = useState<AgentModels | null>(null);
  const [parakeet, setParakeet] = useState<string[]>([]);
  const [opinions, setOpinions] = useState<OpinionModel[]>([]);
  const [downloading, setDownloading] = useState<string | null>(null);
  const [showMore, setShowMore] = useState(false);

  const loadOpinions = useCallback(() => invoke<OpinionModel[]>('second_opinion_list').then(setOpinions).catch(() => {}), []);
  useEffect(() => {
    invoke<AgentModels>('agent_models_get').then(setModels).catch(() => setModels({ recap_model: 'sonnet', clean_model: 'haiku' }));
    invoke<{ name: string; status: unknown }[]>('parakeet_get_available_models')
      .then((ms) => setParakeet((ms || []).filter((m) => m.status === 'Available').map((m) => m.name)))
      .catch(() => {});
    loadOpinions();
  }, [loadOpinions]);
  if (!settings || !models) return null;

  const saveModels = async (patch: Partial<AgentModels>) => {
    const next = { ...models, ...patch };
    try { await invoke('agent_models_set', { models: next }); setModels(next); toast.success('Setting saved'); }
    catch (e) { toast.error('Not saved', { description: String(e) }); }
  };
  const download = async (m: OpinionModel) => {
    setDownloading(m.name);
    try {
      await invoke('second_opinion_install', { name: m.name });
      await loadOpinions();
      await save({ second_opinion: m.name });
    } catch (e) {
      toast.error('Download failed', { description: String(e) });
    } finally { setDownloading(null); }
  };

  const mode = settings.second_pass || 'off';
  const on = mode !== 'off';
  const claude = mode === 'full';
  const installed = opinions.filter((m) => m.installed);
  const missing = opinions.filter((m) => !m.installed);
  const short = (s: string) => s.replace(/ \(.*\)$/, '');
  const name = (m: string) => ({ haiku: 'Claude Haiku', sonnet: 'Claude Sonnet', opus: 'Claude Opus' } as Record<string, string>)[m] || m;
  const opinionLabel = installed.find((m) => m.name === settings.second_opinion)?.label;
  const steps: [string, boolean][] = [
    ['Live transcript', true],
    ['Re-transcribe: ' + (settings.second_pass_model ? short(settings.second_pass_model) : 'best model') + (opinionLabel ? ' + ' + short(opinionLabel) : ''), on],
    [name(models.clean_model) + ' cleans up the text', claude],
    [name(models.recap_model) + ' writes recap and tasks in Notion', true],
  ];

  return (
    <div className={CARD + ' mt-6'}>
      <h3 className="text-lg font-semibold text-gray-900 mb-1">After the Meeting</h3>
      <p className="text-sm text-gray-600">
        The live transcript is written on the fly. When the meeting ends, Meetily can transcribe the whole recording again
        in the background for a much cleaner text: about 10-20 minutes per hour of meeting.
      </p>

      <ol className="flex flex-wrap items-center gap-2 my-5">
        {steps.map(([text, active], i) => (
          <li key={i} className="flex items-center gap-2">
            {i > 0 && <ArrowRight className="w-3.5 h-3.5 text-gray-400" />}
            <span className={'text-xs px-2.5 py-1 rounded-full border ' + (active ? 'bg-blue-50 border-blue-200 text-blue-800' : 'bg-gray-50 border-gray-200 text-gray-400 line-through')}>{text}</span>
          </li>
        ))}
      </ol>

      <Row title="Re-transcribe when the meeting ends">
        <Select value={mode} onValueChange={(v) => save({ second_pass: v })}>
          <SelectTrigger className="w-80"><SelectValue /></SelectTrigger>
          <SelectContent>
            <SelectItem value="off">No, keep the live transcript</SelectItem>
            <SelectItem value="model">Yes, with the models below</SelectItem>
            <SelectItem value="full">Yes, then Claude cleans it up (recommended)</SelectItem>
          </SelectContent>
        </Select>
      </Row>

      {on && (
        <>
          <Row title="Main model" help="Automatic picks Parakeet ultra when it is among the custom models above.">
            <Select value={settings.second_pass_model || AUTO} onValueChange={(v) => save({ second_pass_model: v === AUTO ? '' : v })}>
              <SelectTrigger className="w-80"><SelectValue /></SelectTrigger>
              <SelectContent>
                <SelectItem value={AUTO}>Automatic (best installed)</SelectItem>
                {parakeet.map((m) => <SelectItem key={m} value={m}>{m}</SelectItem>)}
                {settings.second_pass_model && !parakeet.includes(settings.second_pass_model) &&
                  <SelectItem value={settings.second_pass_model}>{settings.second_pass_model} (not found)</SelectItem>}
              </SelectContent>
            </Select>
          </Row>
          <Row title="Second model" help="Optional. Specialised in the meeting language; Claude merges the two readings.">
            <Select value={settings.second_opinion || NONE} onValueChange={(v) => save({ second_opinion: v === NONE ? '' : v })}>
              <SelectTrigger className="w-80"><SelectValue /></SelectTrigger>
              <SelectContent>
                <SelectItem value={NONE}>None</SelectItem>
                {installed.map((m) => <SelectItem key={m.name} value={m.name}>{m.label}</SelectItem>)}
              </SelectContent>
            </Select>
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
          {!claude && settings.second_opinion && (
            <div className="p-3 mb-4 rounded-md border border-yellow-200 bg-yellow-50 text-sm text-yellow-800">
              The second model is only used when Claude cleans up the text: choose “Yes, then Claude cleans it up”.
            </div>
          )}
        </>
      )}

      {claude && (
        <Row title="Text clean-up" help="Mechanical work: Haiku is enough and the fastest.">
          <Select value={models.clean_model} onValueChange={(v) => saveModels({ clean_model: v })}>
            <SelectTrigger className="w-80"><SelectValue /></SelectTrigger>
            <SelectContent>
              <SelectItem value="haiku">Claude Haiku (recommended)</SelectItem>
              <SelectItem value="sonnet">Claude Sonnet</SelectItem>
            </SelectContent>
          </Select>
        </Row>
      )}
      <Row title="Recap and tasks in Notion">
        <Select value={models.recap_model} onValueChange={(v) => saveModels({ recap_model: v })}>
          <SelectTrigger className="w-80"><SelectValue /></SelectTrigger>
          <SelectContent>
            <SelectItem value="sonnet">Claude Sonnet (recommended)</SelectItem>
            <SelectItem value="opus">Claude Opus (most accurate, slower)</SelectItem>
            <SelectItem value="haiku">Claude Haiku (fastest)</SelectItem>
          </SelectContent>
        </Select>
      </Row>
      <Row title="Instructions for Claude" help={<>Two editable text files: <code>transcript-cleanup.md</code> and <code>meeting-to-notion.md</code>.</>}>
        <Button variant="outline" size="sm" onClick={() => invoke('prompts_open_folder')}>
          <FolderOpen className="w-4 h-4" />Open folder
        </Button>
      </Row>
    </div>
  );
}
