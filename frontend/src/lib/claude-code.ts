// Fork: "Claude Code" summary provider (the user's Claude Code CLI, Claude account login).
// Backend: frontend/src-tauri/src/claude_code/
import { invoke } from '@tauri-apps/api/core';

export const CLAUDE_CODE_PROVIDER = 'claude-code';

export type ClaudeCodeEnvironment =
  | { kind: 'auto' }
  | { kind: 'native' }
  | { kind: 'wsl'; distro: string };

export interface ClaudeCodeConfig {
  environment: ClaudeCodeEnvironment;
  /** Absolute path of `claude` inside the environment; detected when null. */
  executable: string | null;
}

export interface ClaudeCodeSettings {
  config: ClaudeCodeConfig;
  /** Model aliases accepted by `claude --model` (e.g. "sonnet"). */
  models: string[];
  /** Environments available on this machine. */
  environments: ClaudeCodeEnvironment[];
}

export interface ClaudeCodeStatus {
  installation: { runtime: { kind: 'native' } | { kind: 'wsl'; distro: string }; executable: string };
  version: string;
  loggedIn: boolean;
  authMethod: string | null;
}

/** Stable key for a <Select> value. */
export function environmentKey(environment: ClaudeCodeEnvironment): string {
  return environment.kind === 'wsl' ? `wsl:${environment.distro}` : environment.kind;
}

export function environmentLabel(environment: ClaudeCodeEnvironment): string {
  switch (environment.kind) {
    case 'auto':
      return 'Automatic';
    case 'native':
      return 'This computer';
    case 'wsl':
      return `WSL · ${environment.distro}`;
  }
}

export const ClaudeCodeAPI = {
  getSettings: () => invoke<ClaudeCodeSettings>('claude_code_get_settings'),
  saveConfig: (config: ClaudeCodeConfig) => invoke<ClaudeCodeConfig>('claude_code_save_config', { config }),
  check: (config: ClaudeCodeConfig) => invoke<ClaudeCodeStatus>('claude_code_check', { config }),
};
