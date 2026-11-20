'use client';

// Fork: settings card of the "Claude Code" summary provider.
// The model is part of the regular model config (saved with the modal's Save, like every provider).
// Where Claude Code runs is a property of this computer, so it is saved as soon as it changes.
import React, { useCallback, useEffect, useRef, useState } from 'react';
import { AlertTriangle, CheckCircle2, RefreshCw, XCircle } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import {
  ClaudeCodeAPI,
  ClaudeCodeConfig,
  ClaudeCodeEnvironment,
  ClaudeCodeStatus,
  environmentKey,
  environmentLabel,
} from '@/lib/claude-code';

interface ClaudeCodeSettingsProps {
  model: string;
  onModelChange: (model: string) => void;
}

type Check =
  | { state: 'idle' }
  | { state: 'checking' }
  | { state: 'done'; status: ClaudeCodeStatus }
  | { state: 'failed'; message: string };

function placeOf(status: ClaudeCodeStatus): string {
  const runtime = status.installation.runtime;
  return runtime.kind === 'wsl' ? `WSL (${runtime.distro})` : 'this computer';
}

function capitalize(text: string): string {
  return text.charAt(0).toUpperCase() + text.slice(1);
}

export function ClaudeCodeSettings({ model, onModelChange }: ClaudeCodeSettingsProps) {
  const [config, setConfig] = useState<ClaudeCodeConfig | null>(null);
  const [models, setModels] = useState<string[]>([]);
  const [environments, setEnvironments] = useState<ClaudeCodeEnvironment[]>([]);
  const [pathDraft, setPathDraft] = useState('');
  const [check, setCheck] = useState<Check>({ state: 'idle' });
  // Only the latest check may update the status (a WSL check can outlive an environment change).
  const checkRun = useRef(0);

  const runCheck = useCallback(async (target: ClaudeCodeConfig) => {
    const run = ++checkRun.current;
    setCheck({ state: 'checking' });
    try {
      const status = await ClaudeCodeAPI.check(target);
      if (run === checkRun.current) setCheck({ state: 'done', status });
    } catch (err) {
      if (run === checkRun.current) setCheck({ state: 'failed', message: String(err) });
    }
  }, []);

  useEffect(() => {
    ClaudeCodeAPI.getSettings()
      .then((settings) => {
        setConfig(settings.config);
        setModels(settings.models);
        setEnvironments(settings.environments);
        setPathDraft(settings.config.executable ?? '');
        runCheck(settings.config);
      })
      .catch((err) => setCheck({ state: 'failed', message: String(err) }));
  }, [runCheck]);

  // A model saved for another provider (or none) is not a Claude Code model: default to the first.
  useEffect(() => {
    if (models.length > 0 && !models.includes(model)) onModelChange(models[0]);
  }, [models, model, onModelChange]);

  const save = async (next: ClaudeCodeConfig) => {
    try {
      const saved = await ClaudeCodeAPI.saveConfig(next);
      setConfig(saved);
      setPathDraft(saved.executable ?? '');
      runCheck(saved);
    } catch (err) {
      toast.error('Could not save the Claude Code settings', { description: String(err) });
    }
  };

  const changeEnvironment = (key: string) => {
    const environment = environments.find((candidate) => environmentKey(candidate) === key);
    // A path belongs to one environment, so it is reset with it.
    if (environment) save({ environment, executable: null });
  };

  const commitPath = () => {
    if (!config) return;
    const executable = pathDraft.trim() || null;
    if (executable !== config.executable) save({ ...config, executable });
  };

  if (!config) {
    return <StatusLine check={check} />;
  }

  return (
    <div className="space-y-4 border-t pt-4">
      <div className="grid gap-4 sm:grid-cols-2">
        <div className="space-y-1">
          <Label>Runs in</Label>
          <Select value={environmentKey(config.environment)} onValueChange={changeEnvironment}>
            <SelectTrigger>
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {environments.map((environment) => (
                <SelectItem key={environmentKey(environment)} value={environmentKey(environment)}>
                  {environmentLabel(environment)}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        <div className="space-y-1">
          <Label>Model</Label>
          <Select value={models.includes(model) ? model : undefined} onValueChange={onModelChange}>
            <SelectTrigger>
              <SelectValue placeholder="Select model" />
            </SelectTrigger>
            <SelectContent>
              {models.map((alias) => (
                <SelectItem key={alias} value={alias}>
                  {capitalize(alias)} (latest)
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
      </div>

      {config.environment.kind !== 'auto' && (
        <div className="space-y-1">
          <Label htmlFor="claude-code-path">Path to claude (optional)</Label>
          <Input
            id="claude-code-path"
            value={pathDraft}
            placeholder={config.environment.kind === 'wsl' ? 'Detected automatically, e.g. /home/you/.local/bin/claude' : 'Detected automatically'}
            onChange={(event) => setPathDraft(event.target.value)}
            onBlur={commitPath}
            onKeyDown={(event) => event.key === 'Enter' && commitPath()}
          />
        </div>
      )}

      <div className="flex items-start justify-between gap-3">
        <StatusLine check={check} />
        <Button
          variant="outline"
          size="sm"
          disabled={check.state === 'checking'}
          onClick={() => runCheck(config)}
        >
          <RefreshCw className={`mr-1 h-3.5 w-3.5 ${check.state === 'checking' ? 'animate-spin' : ''}`} />
          Check
        </Button>
      </div>

      <p className="text-xs text-muted-foreground">
        Uses the Claude account Claude Code is logged in with: no API key needed. Transcripts are sent to Anthropic
        under that account&apos;s terms and count toward its usage limits.
      </p>
    </div>
  );
}

function StatusLine({ check }: { check: Check }) {
  switch (check.state) {
    case 'idle':
    case 'checking':
      return (
        <p className="flex items-center gap-2 text-sm text-muted-foreground">
          <RefreshCw className="h-4 w-4 animate-spin" /> Looking for Claude Code…
        </p>
      );
    case 'failed':
      return (
        <p className="flex items-start gap-2 text-sm text-red-600">
          <XCircle className="mt-0.5 h-4 w-4 shrink-0" /> {check.message}
        </p>
      );
    case 'done': {
      const { status } = check;
      const where = `${status.version} in ${placeOf(status)}`;
      return status.loggedIn ? (
        <p className="flex items-start gap-2 text-sm text-green-700">
          <CheckCircle2 className="mt-0.5 h-4 w-4 shrink-0" /> Ready · Claude Code {where}
        </p>
      ) : (
        <p className="flex items-start gap-2 text-sm text-amber-700">
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
          <span>
            Claude Code {where} is not logged in. Open a terminal in {placeOf(status)}, run <code>claude</code> and use{' '}
            <code>/login</code>, then press Check.
          </span>
        </p>
      );
    }
  }
}
