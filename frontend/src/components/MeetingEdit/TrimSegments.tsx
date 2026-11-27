"use client";

// Fork: pick transcript segments (greetings, off-topic talk) and remove them from the meeting;
// the same time ranges are cut from the audio.

import { useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { toast } from 'sonner';
import { TranscriptSegmentData } from '@/types';

interface TrimSegmentsProps {
  meetingId: string;
  segments: TranscriptSegmentData[];
  /** The meeting has a recording folder: its audio is cut too. */
  hasAudio: boolean;
  hasMore?: boolean;
  onLoadMore?: () => void;
  onClose: () => void;
  onTrimmed?: () => Promise<void>;
}

const clock = (seconds: number) => `${Math.floor(seconds / 60)}:${String(Math.floor(seconds % 60)).padStart(2, '0')}`;

export function TrimSegments({ meetingId, segments, hasAudio, hasMore, onLoadMore, onClose, onTrimmed }: TrimSegmentsProps) {
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [confirming, setConfirming] = useState(false);
  const [trimming, setTrimming] = useState(false);

  const toggle = (id: string) => {
    setConfirming(false);
    setSelected(previous => {
      const next = new Set(previous);
      if (!next.delete(id)) next.add(id);
      return next;
    });
  };

  const trim = async () => {
    setTrimming(true);
    try {
      const removed = await invoke<number>('meeting_edit_trim', { meetingId, transcriptIds: Array.from(selected) });
      toast.success(`Removed ${selected.size} segments`, {
        description: removed > 0 ? `${Math.round(removed)} s cut from the audio; the original is kept as audio.original` : 'Transcript only: no audio to cut',
      });
      onClose();
      await onTrimmed?.();
    } catch (error) {
      toast.error('Could not remove the segments', { description: String(error) });
    } finally {
      setTrimming(false);
    }
  };

  return (
    <div className="flex flex-1 min-h-0 flex-col">
      <div className="flex items-center gap-3 px-4 py-2 text-sm text-gray-600 border-b border-gray-200">
        <span className="flex-1 tabular-nums">{selected.size === 0 ? 'Select the segments to remove' : `${selected.size} selected`}</span>
        <button className="text-gray-500 hover:text-gray-800" onClick={onClose} disabled={trimming}>
          Cancel
        </button>
        <button
          className="font-medium text-red-700 hover:text-red-900 disabled:text-gray-400"
          disabled={selected.size === 0 || trimming}
          onClick={() => (confirming ? trim() : setConfirming(true))}
        >
          {trimming ? 'Removing…' : confirming ? (hasAudio ? 'Confirm, cut the audio too' : 'Confirm') : 'Remove'}
        </button>
      </div>
      <div className="flex-1 overflow-y-auto pb-4">
        {segments.map(segment => {
          const on = selected.has(segment.id);
          return (
            <label
              key={segment.id}
              className={`flex gap-3 px-4 py-2 text-sm cursor-pointer border-b border-gray-100 ${on ? 'bg-red-50 text-gray-400 line-through' : 'hover:bg-gray-50 text-gray-800'}`}
            >
              <input type="checkbox" className="mt-1" checked={on} onChange={() => toggle(segment.id)} disabled={trimming} />
              <span className="w-12 shrink-0 font-mono text-xs text-gray-500 tabular-nums mt-0.5">{clock(segment.timestamp)}</span>
              <span className="flex-1">{segment.text}</span>
            </label>
          );
        })}
        {hasMore && onLoadMore && (
          <button className="w-full py-2 text-sm text-gray-500 hover:text-gray-800" onClick={onLoadMore}>
            Load more segments
          </button>
        )}
      </div>
    </div>
  );
}
