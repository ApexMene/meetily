// Fork: user-added transcription and summary models (Hugging Face or local import).
// Backend: frontend/src-tauri/src/custom_models/
import { invoke } from '@tauri-apps/api/core';

export type CustomModelEngine = 'parakeet' | 'whisper' | 'summary';

export const CUSTOM_MODEL_ID_PREFIX = 'custom-';

export function isCustomModelId(name: string | undefined | null): boolean {
  return !!name && name.startsWith(CUSTOM_MODEL_ID_PREFIX);
}

export interface CustomModel {
  id: string;
  displayName: string;
  engine: CustomModelEngine;
  source: 'huggingface' | 'local';
  origin: string;
  hfRepo: string | null;
  hfRevision: string | null;
  hfSubdir: string | null;
  quantization: 'int8' | 'fp32' | null;
  sizeBytes: number;
  license: string | null;
  createdAt: string;
  /** GGUF metadata, for summary models only. */
  summary: { architecture: string; chatTemplate: string; contextSize: number; layerCount: number } | null;
  /** Set when the files on disk no longer match what was verified at install time. */
  problem: string | null;
}

export interface RemoteFile {
  repoPath: string;
  name: string;
  size: number;
  origin: { repo: string; commit: string } | null;
}

export interface ModelVariant {
  key: string;
  engine: CustomModelEngine;
  label: string;
  subdir: string;
  quantization: 'int8' | 'fp32' | null;
  files: RemoteFile[];
  totalBytes: number;
  blockedReason: string | null;
}

export interface InspectResult {
  repo: string;
  commit: string;
  license: string | null;
  gated: boolean;
  usedToken: boolean;
  variants: ModelVariant[];
}

export type InstallPhase = 'downloading' | 'copying' | 'testing';

export interface InstallProgressEvent {
  jobId: string;
  phase: InstallPhase;
  downloadedBytes: number;
  totalBytes: number;
  percent: number;
  currentFile: string;
}

export interface InstallCompleteEvent {
  jobId: string;
  modelId: string;
  engine: CustomModelEngine;
  displayName: string;
}

export interface InstallErrorEvent {
  jobId: string;
  error: string;
  cancelled: boolean;
}

export const CUSTOM_MODEL_EVENTS = {
  progress: 'custom-model-install-progress',
  complete: 'custom-model-install-complete',
  error: 'custom-model-install-error',
} as const;

function toMessage(error: unknown): string {
  if (typeof error === 'string') return error;
  if (error instanceof Error) return error.message;
  return 'Unknown error';
}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(command, args);
  } catch (error) {
    throw new Error(toMessage(error));
  }
}

export const CustomModelsAPI = {
  list: () => call<CustomModel[]>('custom_models_list'),
  inspect: (reference: string) => call<InspectResult>('custom_models_hf_inspect', { reference }),
  install: (repo: string, commit: string, variantKey: string, displayName: string) =>
    call<string>('custom_models_hf_install', { repo, commit, variantKey, displayName }),
  cancel: (jobId: string) => call<boolean>('custom_models_cancel', { jobId }),
  pickLocal: (engine: CustomModelEngine) => call<string | null>('custom_models_pick_local', { engine }),
  importLocal: (engine: CustomModelEngine, path: string, displayName: string) =>
    call<string>('custom_models_import_local', { engine, path, displayName }),
  remove: (id: string) => call<void>('custom_models_delete', { id }),
  tokenStatus: () => call<boolean>('custom_models_token_status'),
  setToken: (token: string) => call<void>('custom_models_set_token', { token }),
  clearToken: () => call<void>('custom_models_clear_token'),
  openFolder: () => call<void>('custom_models_open_folder'),
};

export function formatBytes(bytes: number): string {
  if (bytes >= 1e9) return `${(bytes / 1e9).toFixed(2)} GB`;
  if (bytes >= 1e6) return `${(bytes / 1e6).toFixed(0)} MB`;
  if (bytes >= 1e3) return `${(bytes / 1e3).toFixed(0)} KB`;
  return `${bytes} B`;
}

/** Suggested display name, e.g. "orukeet (int8)" from "oruk/orukeet". */
export function suggestDisplayName(repo: string, variant: ModelVariant): string {
  const base = repo.split('/')[1] ?? repo;
  if (variant.engine === 'whisper') {
    const file = variant.files[0]?.name ?? '';
    return file.replace(/\.bin$/i, '').replace(/^ggml-/i, '') || base;
  }
  if (variant.engine === 'summary') {
    const file = variant.files[0]?.name ?? '';
    return file.replace(/\.gguf$/i, '') || base;
  }
  return variant.quantization ? `${base} (${variant.quantization})` : base;
}
