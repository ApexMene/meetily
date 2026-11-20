'use client';

// Fork: list, select and remove user-added models for one engine.
import React, { useCallback, useEffect, useState } from 'react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { AddCustomModelDialog } from './AddCustomModelDialog';
import { CustomModel, CustomModelEngine, CustomModelsAPI, formatBytes } from '@/lib/custom-models';

interface CustomModelsSectionProps {
  engine: CustomModelEngine;
  selectedModel?: string;
  /** Built-in model the backend selects when the selected custom model is removed. */
  defaultModelId: string;
  /** Select through the parent manager, so saving and toasts behave like built-in models. */
  onSelect: (modelId: string) => void | Promise<void>;
  /** Called after a model was added or removed, so the parent refreshes its engine list. */
  onChanged?: () => void | Promise<void>;
}

export function CustomModelsSection({ engine, selectedModel, defaultModelId, onSelect, onChanged }: CustomModelsSectionProps) {
  const [models, setModels] = useState<CustomModel[]>([]);
  const [dialogOpen, setDialogOpen] = useState(false);
  const [removing, setRemoving] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      const all = await CustomModelsAPI.list();
      setModels(all.filter(model => model.engine === engine));
    } catch (err) {
      console.error('Failed to list custom models:', err);
    }
  }, [engine]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const remove = async (model: CustomModel) => {
    const wasSelected = selectedModel === model.id;
    const confirmed = window.confirm(
      `Remove "${model.displayName}" and delete its files (${formatBytes(model.sizeBytes)})?` +
        (wasSelected ? '\n\nIt is the selected model: the default model will be selected instead.' : '')
    );
    if (!confirmed) return;
    setRemoving(model.id);
    try {
      await CustomModelsAPI.remove(model.id);
      toast.success(`${model.displayName} removed`);
      await refresh();
      await onChanged?.();
      // The backend already switched the saved selection; keep the UI in sync with it.
      if (wasSelected) await onSelect(defaultModelId);
    } catch (err) {
      toast.error(`Failed to remove ${model.displayName}`, {
        description: err instanceof Error ? err.message : String(err),
      });
    } finally {
      setRemoving(null);
    }
  };

  return (
    <div className="space-y-3 pt-2">
      <div className="flex items-center justify-between">
        <h4 className="text-sm font-semibold text-gray-700">Custom models</h4>
        <div className="flex gap-2">
          {models.length > 0 && (
            <Button variant="ghost" size="sm" onClick={() => CustomModelsAPI.openFolder().catch(() => {})}>
              Open folder
            </Button>
          )}
          <Button variant="outline" size="sm" onClick={() => setDialogOpen(true)}>
            + Add model
          </Button>
        </div>
      </div>

      {models.length === 0 && (
        <p className="text-xs text-gray-500">
          Add a model from Hugging Face or from local files (e.g. a fine-tuned{' '}
          {engine === 'parakeet'
            ? 'Parakeet ONNX export'
            : engine === 'whisper'
              ? 'whisper.cpp ggml model'
              : 'GGUF instruct model'}
          ).
        </p>
      )}

      {models.map(model => {
        const isSelected = selectedModel === model.id;
        const usable = !model.problem;
        return (
          <div
            key={model.id}
            onClick={() => usable && !isSelected && onSelect(model.id)}
            className={`rounded-lg border-2 p-4 transition-all ${
              isSelected && usable
                ? 'border-blue-500 bg-blue-50'
                : usable
                  ? 'border-gray-200 bg-white hover:border-gray-300 cursor-pointer'
                  : 'border-red-200 bg-red-50'
            }`}
          >
            <div className="flex items-start justify-between gap-3">
              <div className="min-w-0">
                <div className="flex items-center gap-2">
                  <span className="text-xl">🧩</span>
                  <h3 className="font-semibold text-gray-900 break-all">{model.displayName}</h3>
                  {isSelected && usable && (
                    <span className="bg-blue-600 text-white px-2 py-0.5 rounded-full text-xs font-medium">✓</span>
                  )}
                  <span className="rounded bg-purple-100 px-1.5 py-0.5 text-[10px] font-medium text-purple-700">
                    Custom
                  </span>
                </div>
                <p className="text-xs text-gray-600 ml-8 break-all">
                  {model.origin}
                  {model.hfSubdir ? ` · ${model.hfSubdir}` : ''}
                  {model.quantization ? ` · ${model.quantization}` : ''} · {formatBytes(model.sizeBytes)}
                  {model.license ? ` · ${model.license}` : ''}
                  {model.summary ? ` · ${model.summary.architecture} · ${model.summary.contextSize} ctx` : ''}
                </p>
                {model.problem && (
                  <p className="text-xs text-red-700 ml-8 mt-1">
                    Files changed or missing ({model.problem}). Remove and add the model again.
                  </p>
                )}
              </div>
              <Button
                variant="ghost"
                size="sm"
                className="text-gray-500 hover:text-red-600"
                disabled={removing === model.id}
                onClick={event => {
                  event.stopPropagation();
                  remove(model);
                }}
              >
                {removing === model.id ? 'Removing…' : 'Remove'}
              </Button>
            </div>
          </div>
        );
      })}

      <AddCustomModelDialog
        engine={engine}
        open={dialogOpen}
        onOpenChange={setDialogOpen}
        onInstalled={async modelId => {
          await refresh();
          await onChanged?.();
          await onSelect(modelId);
        }}
      />
    </div>
  );
}
