"use client";

// Fork: bar shown over the meeting list while meetings are Ctrl/Cmd+clicked for merging.

import { useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { toast } from 'sonner';

interface MergeBarProps {
  selection: string[];
  onCancel: () => void;
  /** Called with the id of the new meeting. */
  onMerged: (meetingId: string) => Promise<void> | void;
}

export function MergeBar({ selection, onCancel, onMerged }: MergeBarProps) {
  const [merging, setMerging] = useState(false);

  const merge = async () => {
    setMerging(true);
    try {
      const meetingId = await invoke<string>('meeting_edit_merge', { meetingIds: selection });
      toast.success('Meetings merged', { description: 'The original meetings are unchanged' });
      await onMerged(meetingId);
    } catch (error) {
      toast.error('Could not merge the meetings', { description: String(error) });
    } finally {
      setMerging(false);
    }
  };

  return (
    <div className="flex items-center gap-3 mx-3 my-1 px-3 py-2 text-sm text-gray-600 border-b border-gray-200">
      <span className="flex-1 tabular-nums">{selection.length} selected</span>
      <button className="text-gray-500 hover:text-gray-800" onClick={onCancel} disabled={merging}>
        Cancel
      </button>
      <button
        className="font-medium text-blue-700 hover:text-blue-900 disabled:text-gray-400"
        disabled={selection.length < 2 || merging}
        title={selection.length < 2 ? 'Ctrl+click at least two meetings' : undefined}
        onClick={merge}
      >
        {merging ? 'Merging…' : 'Merge'}
      </button>
    </div>
  );
}
