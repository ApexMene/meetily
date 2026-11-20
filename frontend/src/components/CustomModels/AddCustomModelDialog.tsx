'use client';

// Fork: add a transcription or summary model from Hugging Face or from local files.
import React, { useEffect, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { toast } from 'sonner';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import {
  CUSTOM_MODEL_EVENTS,
  CustomModelEngine,
  CustomModelsAPI,
  InspectResult,
  InstallCompleteEvent,
  InstallErrorEvent,
  InstallProgressEvent,
  ModelVariant,
  formatBytes,
  suggestDisplayName,
} from '@/lib/custom-models';

interface AddCustomModelDialogProps {
  engine: CustomModelEngine;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onInstalled: (modelId: string) => void;
}

const ENGINE_LABEL: Record<CustomModelEngine, string> = {
  parakeet: 'Parakeet',
  whisper: 'Whisper',
  summary: 'summary',
};

const PHASE_LABEL: Record<InstallProgressEvent['phase'], string> = {
  downloading: 'Downloading and verifying',
  copying: 'Copying and hashing',
  testing: 'Testing the model',
};

export function AddCustomModelDialog({ engine, open, onOpenChange, onInstalled }: AddCustomModelDialogProps) {
  const [tab, setTab] = useState<'huggingface' | 'local'>('huggingface');
  const [reference, setReference] = useState('');
  const [inspecting, setInspecting] = useState(false);
  const [inspection, setInspection] = useState<InspectResult | null>(null);
  const [variantKey, setVariantKey] = useState<string>('');
  const [displayName, setDisplayName] = useState('');
  const [licenseAccepted, setLicenseAccepted] = useState(false);
  const [localPath, setLocalPath] = useState<string | null>(null);
  const [jobId, setJobId] = useState<string | null>(null);
  const [progress, setProgress] = useState<InstallProgressEvent | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [hasToken, setHasToken] = useState(false);
  const [tokenInput, setTokenInput] = useState('');
  const [showToken, setShowToken] = useState(false);

  // The ref is set synchronously when a job starts, so events are matched even before React
  // re-renders. Terminal events that arrive before the install command returned its id are
  // kept in `earlyEvents` and replayed once the id is known.
  const jobIdRef = useRef<string | null>(null);
  const startingRef = useRef(false);
  const earlyEvents = useRef(new Map<string, () => void>());
  const onInstalledRef = useRef(onInstalled);
  useEffect(() => {
    onInstalledRef.current = onInstalled;
  }, [onInstalled]);

  const busy = jobId !== null;
  const engineVariants = (inspection?.variants ?? []).filter(v => v.engine === engine);
  const otherEngineCount = (inspection?.variants.length ?? 0) - engineVariants.length;
  const selectedVariant: ModelVariant | undefined = engineVariants.find(v => v.key === variantKey);

  useEffect(() => {
    if (!open) return;
    CustomModelsAPI.tokenStatus().then(setHasToken).catch(() => setHasToken(false));
  }, [open]);

  useEffect(() => {
    let disposed = false;
    const unlisteners: Array<() => void> = [];
    Promise.all([
      listen<InstallProgressEvent>(CUSTOM_MODEL_EVENTS.progress, event => {
        if (event.payload.jobId === jobIdRef.current) setProgress(event.payload);
      }),
      listen<InstallCompleteEvent>(CUSTOM_MODEL_EVENTS.complete, event => {
        const payload = event.payload;
        routeTerminal(payload.jobId, () => {
          toast.success(`${payload.displayName} is ready`, {
            description: 'Verified, tested and added to your models',
          });
          endJob();
          resetForm();
          onInstalledRef.current(payload.modelId);
          onOpenChange(false);
        });
      }),
      listen<InstallErrorEvent>(CUSTOM_MODEL_EVENTS.error, event => {
        const payload = event.payload;
        routeTerminal(payload.jobId, () => {
          endJob();
          if (payload.cancelled) {
            toast.info('Install cancelled');
          } else {
            setError(payload.error);
          }
        });
      }),
    ]).then(registered => {
      if (disposed) registered.forEach(unlisten => unlisten());
      else unlisteners.push(...registered);
    });
    return () => {
      disposed = true;
      unlisteners.forEach(unlisten => unlisten());
    };
    // onOpenChange is stable enough for this dialog; listeners must not re-register mid-install.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  function routeTerminal(eventJobId: string, handle: () => void) {
    if (eventJobId === jobIdRef.current) handle();
    else if (startingRef.current) earlyEvents.current.set(eventJobId, handle);
  }

  function endJob() {
    jobIdRef.current = null;
    setJobId(null);
    setProgress(null);
  }

  /** Start an install command once; returns when the job id is known (events may already be in). */
  const startJob = async (start: () => Promise<string>) => {
    if (startingRef.current || jobIdRef.current) return;
    startingRef.current = true;
    setError(null);
    try {
      const id = await start();
      jobIdRef.current = id;
      setJobId(id);
      const early = earlyEvents.current.get(id);
      if (early) early();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      startingRef.current = false;
      earlyEvents.current.clear();
    }
  };

  function resetForm() {
    setInspection(null);
    setVariantKey('');
    setDisplayName('');
    setLicenseAccepted(false);
    setLocalPath(null);
    setError(null);
  }

  const inspect = async () => {
    if (!reference.trim()) return;
    setInspecting(true);
    resetForm();
    try {
      const result = await CustomModelsAPI.inspect(reference);
      setInspection(result);
      const first = result.variants.find(v => v.engine === engine && !v.blockedReason);
      if (first) {
        setVariantKey(first.key);
        setDisplayName(suggestDisplayName(result.repo, first));
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setInspecting(false);
    }
  };

  const chooseVariant = (variant: ModelVariant) => {
    setVariantKey(variant.key);
    if (inspection) setDisplayName(suggestDisplayName(inspection.repo, variant));
  };

  const installFromHub = async () => {
    if (!inspection || !selectedVariant) return;
    await startJob(() =>
      CustomModelsAPI.install(inspection.repo, inspection.commit, selectedVariant.key, displayName)
    );
  };

  const pickLocal = async () => {
    setError(null);
    try {
      const path = await CustomModelsAPI.pickLocal(engine);
      if (path) {
        setLocalPath(path);
        const base = path.split(/[\\/]/).filter(Boolean).pop() ?? 'custom model';
        setDisplayName(base.replace(/\.bin$/i, '').replace(/^ggml-/i, ''));
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };

  const importLocal = async () => {
    if (!localPath) return;
    await startJob(() => CustomModelsAPI.importLocal(engine, localPath, displayName));
  };

  const saveToken = async () => {
    try {
      await CustomModelsAPI.setToken(tokenInput);
      setTokenInput('');
      setHasToken(true);
      setShowToken(false);
      toast.success('Hugging Face token saved in the system credential store');
    } catch (err) {
      toast.error(err instanceof Error ? err.message : String(err));
    }
  };

  const clearToken = async () => {
    try {
      await CustomModelsAPI.clearToken();
      setHasToken(false);
      toast.success('Hugging Face token removed');
    } catch (err) {
      toast.error(err instanceof Error ? err.message : String(err));
    }
  };

  const needsLicense = tab === 'huggingface' && !!inspection?.license;
  const canInstallHub =
    !!selectedVariant && !selectedVariant.blockedReason && displayName.trim().length > 0 && (!needsLicense || licenseAccepted);

  return (
    <Dialog
      open={open}
      onOpenChange={next => {
        if (busy) return; // keep the dialog while installing; cancel first
        if (!next) resetForm();
        onOpenChange(next);
      }}
    >
      <DialogContent className="max-w-2xl max-h-[85vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>Add a {ENGINE_LABEL[engine]} model</DialogTitle>
          <DialogDescription>
            {engine === 'parakeet'
              ? 'Parakeet TDT models exported to ONNX (encoder, decoder_joint, vocab.txt), like the built-in ones.'
              : engine === 'whisper'
                ? 'Whisper models in whisper.cpp ggml format (.bin).'
                : 'Instruct (chat) LLMs in single-file GGUF format, up to 8 GB. Supported chat formats: Gemma, Qwen/ChatML, Llama 3, Mistral.'}{' '}
            Files are verified with SHA-256 and the model is test-loaded before it can be selected.
          </DialogDescription>
        </DialogHeader>

        <Tabs value={tab} onValueChange={value => { if (!busy) { setTab(value as typeof tab); resetForm(); } }}>
          <TabsList className="grid w-full grid-cols-2">
            <TabsTrigger value="huggingface" disabled={busy}>Hugging Face</TabsTrigger>
            <TabsTrigger value="local" disabled={busy}>Local files</TabsTrigger>
          </TabsList>

          <TabsContent value="huggingface" className="space-y-4 pt-2">
            <div className="flex gap-2">
              <Input
                placeholder="owner/repository or https://huggingface.co/..."
                value={reference}
                onChange={e => setReference(e.target.value)}
                onKeyDown={e => { if (e.key === 'Enter') inspect(); }}
                disabled={busy || inspecting}
              />
              <Button onClick={inspect} disabled={busy || inspecting || !reference.trim()}>
                {inspecting ? 'Checking…' : 'Check'}
              </Button>
            </div>

            {inspection && (
              <div className="space-y-3">
                <div className="text-xs text-gray-600 space-y-0.5">
                  <div>
                    <span className="font-medium">{inspection.repo}</span> @{' '}
                    <code>{inspection.commit.slice(0, 7)}</code> (pinned commit)
                  </div>
                  {inspection.license && <div>License: {inspection.license}</div>}
                  {inspection.gated && <div>Gated repository{inspection.usedToken ? ' (using your token)' : ''}</div>}
                </div>

                {engineVariants.length === 0 ? (
                  <div className="rounded-md border border-amber-200 bg-amber-50 p-3 text-sm text-amber-800">
                    No {ENGINE_LABEL[engine]}-compatible files found in this repository.
                    {otherEngineCount > 0 && ` It has ${otherEngineCount} variant(s) for other engines.`}
                    {' '}Other formats (safetensors, .nemo, GGUF for other runtimes, sherpa-onnx, CoreML) are not supported.
                  </div>
                ) : (
                  <div className="space-y-2">
                    {engineVariants.map(variant => (
                      <label
                        key={variant.key}
                        className={`flex items-start gap-3 rounded-md border p-3 text-sm ${
                          variant.blockedReason ? 'opacity-60 cursor-not-allowed' : 'cursor-pointer hover:border-gray-400'
                        } ${variant.key === variantKey ? 'border-blue-500 bg-blue-50' : 'border-gray-200'}`}
                      >
                        <input
                          type="radio"
                          name="variant"
                          className="mt-1"
                          checked={variant.key === variantKey}
                          disabled={!!variant.blockedReason || busy}
                          onChange={() => chooseVariant(variant)}
                        />
                        <div className="flex-1 min-w-0">
                          <div className="font-medium break-all">{variant.label}</div>
                          <div className="text-xs text-gray-500">
                            {variant.files.length} files · {formatBytes(variant.totalBytes)}
                          </div>
                          {variant.blockedReason && (
                            <div className="text-xs text-red-600 mt-1">{variant.blockedReason}</div>
                          )}
                        </div>
                      </label>
                    ))}
                  </div>
                )}
              </div>
            )}

            <div className="border-t pt-3 text-xs text-gray-600">
              {hasToken ? (
                <div className="flex items-center justify-between">
                  <span>Hugging Face token stored in the system credential store.</span>
                  <Button variant="ghost" size="sm" onClick={clearToken} disabled={busy}>Remove</Button>
                </div>
              ) : showToken ? (
                <div className="flex gap-2">
                  <Input
                    type="password"
                    autoComplete="off"
                    placeholder="hf_… (read-only token)"
                    value={tokenInput}
                    onChange={e => setTokenInput(e.target.value)}
                  />
                  <Button size="sm" onClick={saveToken} disabled={!tokenInput.trim()}>Save</Button>
                </div>
              ) : (
                <button className="underline" onClick={() => setShowToken(true)}>
                  Add a Hugging Face token for private or gated repositories
                </button>
              )}
            </div>
          </TabsContent>

          <TabsContent value="local" className="space-y-3 pt-2">
            <p className="text-sm text-gray-600">
              {engine === 'parakeet'
                ? 'Choose the folder with encoder-model[.int8].onnx, decoder_joint-model[.int8].onnx and vocab.txt. If nemo128.onnx is missing, the standard Parakeet v3 preprocessor is downloaded.'
                : engine === 'whisper'
                  ? 'Choose a whisper.cpp ggml .bin file.'
                  : 'Choose a single-file .gguf instruct model.'}{' '}
              Files are copied into the app; the originals are not modified.
            </p>
            <div className="flex items-center gap-2">
              <Button variant="outline" onClick={pickLocal} disabled={busy}>
                {engine === 'parakeet' ? 'Choose folder…' : 'Choose file…'}
              </Button>
              {localPath && <code className="text-xs break-all">{localPath}</code>}
            </div>
          </TabsContent>
        </Tabs>

        {(selectedVariant || localPath) && (
          <div className="space-y-2">
            <label className="text-sm font-medium">Name shown in the app</label>
            <Input value={displayName} maxLength={80} onChange={e => setDisplayName(e.target.value)} disabled={busy} />
            {needsLicense && (
              <label className="flex items-center gap-2 text-sm">
                <input
                  type="checkbox"
                  checked={licenseAccepted}
                  onChange={e => setLicenseAccepted(e.target.checked)}
                  disabled={busy}
                />
                I have read and accept the model license ({inspection?.license})
              </label>
            )}
          </div>
        )}

        {progress && (
          <div className="space-y-1">
            <div className="flex justify-between text-xs text-gray-600">
              <span>{PHASE_LABEL[progress.phase]}{progress.currentFile ? ` · ${progress.currentFile}` : ''}</span>
              <span>
                {progress.totalBytes > 0 && `${formatBytes(progress.downloadedBytes)} / ${formatBytes(progress.totalBytes)}`}
              </span>
            </div>
            <div className="h-2 w-full overflow-hidden rounded-full bg-gray-200">
              <div
                className="h-full rounded-full bg-blue-600 transition-all"
                style={{ width: `${progress.phase === 'testing' ? 100 : progress.percent}%` }}
              />
            </div>
          </div>
        )}

        {error && (
          <div className="rounded-md border border-red-200 bg-red-50 p-3 text-sm text-red-800 break-words">{error}</div>
        )}

        <DialogFooter>
          {busy ? (
            <Button variant="outline" onClick={() => jobId && CustomModelsAPI.cancel(jobId)}>
              Cancel install
            </Button>
          ) : (
            <>
              <Button variant="outline" onClick={() => onOpenChange(false)}>Close</Button>
              {tab === 'huggingface' ? (
                <Button onClick={installFromHub} disabled={!canInstallHub}>
                  Download and add
                  {selectedVariant ? ` (${formatBytes(selectedVariant.totalBytes)})` : ''}
                </Button>
              ) : (
                <Button onClick={importLocal} disabled={!localPath || !displayName.trim()}>
                  Import
                </Button>
              )}
            </>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
