"use client";

import { Transcript, TranscriptSegmentData } from '@/types';
import { TranscriptView } from '@/components/TranscriptView';
import { VirtualizedTranscriptView } from '@/components/VirtualizedTranscriptView';
import { TranscriptButtonGroup } from './TranscriptButtonGroup';
import { useMemo, useState } from 'react';
import { toast } from 'sonner';

interface TranscriptPanelProps {
  transcripts: Transcript[];
  customPrompt: string;
  onPromptChange: (value: string) => void;
  onCopyTranscript: () => void;
  onOpenMeetingFolder: () => Promise<void>;
  isRecording: boolean;
  disableAutoScroll?: boolean;

  // Optional pagination props (when using virtualization)
  usePagination?: boolean;
  segments?: TranscriptSegmentData[];
  hasMore?: boolean;
  isLoadingMore?: boolean;
  totalCount?: number;
  loadedCount?: number;
  onLoadMore?: () => void;

  // Retranscription props
  meetingId?: string;
  meetingFolderPath?: string | null;
  onRefetchTranscripts?: () => Promise<void>;
}

export function TranscriptPanel({
  transcripts,
  customPrompt,
  onPromptChange,
  onCopyTranscript,
  onOpenMeetingFolder,
  isRecording,
  disableAutoScroll = false,
  usePagination = false,
  segments,
  hasMore,
  isLoadingMore,
  totalCount,
  loadedCount,
  onLoadMore,
  meetingId,
  meetingFolderPath,
  onRefetchTranscripts,
}: TranscriptPanelProps) {
  // Convert transcripts to segments if pagination is not used but we want virtualization
  const convertedSegments = useMemo(() => {
    if (usePagination && segments) {
      return segments;
    }
    // Convert transcripts to segments for virtualization
    return transcripts.map(t => ({
      id: t.id,
      timestamp: t.audio_start_time ?? 0,
      endTime: t.audio_end_time,
      text: t.text,
      confidence: t.confidence,
    }));
  }, [transcripts, usePagination, segments]);

  // Fork: trim mode, select segments and remove them from transcript and audio
  const [trimMode, setTrimMode] = useState(false);
  const [trimSel, setTrimSel] = useState<Set<string>>(new Set());
  const [confirmTrim, setConfirmTrim] = useState(false);
  const [trimming, setTrimming] = useState(false);

  const exitTrim = () => { setTrimMode(false); setTrimSel(new Set()); setConfirmTrim(false); };
  const toggleSeg = (id: string) => {
    setConfirmTrim(false);
    setTrimSel(prev => { const n = new Set(prev); n.has(id) ? n.delete(id) : n.add(id); return n; });
  };
  const fmt = (t: number) => `${Math.floor(t / 60)}:${String(Math.floor(t % 60)).padStart(2, '0')}`;

  const runTrim = async () => {
    if (!meetingId || trimSel.size === 0) return;
    setTrimming(true);
    try {
      const { invoke } = await import('@tauri-apps/api/core');
      const removed = await invoke<number>('api_trim_meeting', { meetingId, transcriptIds: Array.from(trimSel) });
      toast.success(`Removed ${trimSel.size} segments`, {
        description: removed > 0 ? `${Math.round(removed)} s cut from the audio, original kept as audio.original` : 'Transcript only, no audio found',
      });
      exitTrim();
      await onRefetchTranscripts?.();
    } catch (e) {
      toast.error('Could not remove segments', { description: String(e) });
    } finally {
      setTrimming(false);
    }
  };

  return (
    <div className="flex h-full min-w-0 w-full bg-white flex-col relative @container">
      {/* Title area */}
      <div className="p-4 border-b border-gray-200">
        <TranscriptButtonGroup
          transcriptCount={usePagination ? (totalCount ?? convertedSegments.length) : (transcripts?.length || 0)}
          onCopyTranscript={onCopyTranscript}
          onOpenMeetingFolder={onOpenMeetingFolder}
          meetingId={meetingId}
          meetingFolderPath={meetingFolderPath}
          onRefetchTranscripts={onRefetchTranscripts}
        />
        {!isRecording && meetingId && convertedSegments.length > 0 && (
          <div className="flex items-center gap-3 mt-3 text-sm text-gray-600">
            {!trimMode ? (
              <button className="text-gray-500 hover:text-gray-800 transition-colors" onClick={() => setTrimMode(true)}>
                Remove parts…
              </button>
            ) : (
              <>
                <span className="flex-1 tabular-nums">
                  {trimSel.size === 0 ? 'Select the segments to remove' : `${trimSel.size} selected`}
                </span>
                <button className="text-gray-500 hover:text-gray-800 transition-colors" onClick={exitTrim} disabled={trimming}>Cancel</button>
                <button
                  className="font-medium text-red-700 hover:text-red-900 disabled:text-gray-400 transition-colors"
                  disabled={trimSel.size === 0 || trimming}
                  onClick={() => (confirmTrim ? runTrim() : setConfirmTrim(true))}
                >
                  {trimming ? 'Removing…' : confirmTrim ? 'Confirm, cut audio too' : 'Remove'}
                </button>
              </>
            )}
          </div>
        )}
      </div>

      {/* Transcript content - use virtualized view for better performance */}
      {trimMode ? (
        <div className="flex-1 overflow-y-auto pb-4">
          {convertedSegments.map(seg => {
            const on = trimSel.has(seg.id);
            return (
              <label
                key={seg.id}
                className={`flex gap-3 px-4 py-2 text-sm cursor-pointer border-b border-gray-100 transition-colors ${on ? 'bg-red-50 text-gray-400 line-through' : 'hover:bg-gray-50 text-gray-800'}`}
              >
                <input type="checkbox" className="mt-1" checked={on} onChange={() => toggleSeg(seg.id)} />
                <span className="w-12 shrink-0 font-mono text-xs text-gray-500 tabular-nums mt-0.5">{fmt(seg.timestamp)}</span>
                <span className="flex-1">{seg.text}</span>
              </label>
            );
          })}
        </div>
      ) : (
      <div className="flex-1 overflow-hidden pb-4">
        <VirtualizedTranscriptView
          segments={convertedSegments}
          isRecording={isRecording}
          isPaused={false}
          isProcessing={false}
          isStopping={false}
          enableStreaming={false}
          showConfidence={true}
          disableAutoScroll={disableAutoScroll}
          hasMore={hasMore}
          isLoadingMore={isLoadingMore}
          totalCount={totalCount}
          loadedCount={loadedCount}
          onLoadMore={onLoadMore}
        />
      </div>
      )}

      {/* Custom prompt input at bottom of transcript section */}
      {!isRecording && convertedSegments.length > 0 && (
        <div className="p-1 border-t border-gray-200">
          <textarea
            placeholder="Add context for AI summary. For example people involved, meeting overview, objective etc..."
            className="w-full px-3 py-2 border border-gray-200 rounded-md text-sm focus:outline-none focus:ring-1 focus:ring-blue-500 focus:border-blue-500 bg-white shadow-sm min-h-[80px] resize-y"
            value={customPrompt}
            onChange={(e) => onPromptChange(e.target.value)}
          />
        </div>
      )}
    </div>
  );
}
