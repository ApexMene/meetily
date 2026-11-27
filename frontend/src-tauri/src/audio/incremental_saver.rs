use std::path::PathBuf;
use anyhow::{Result, anyhow};
use log::{info, warn, error};
use super::encode::encode_single_audio;
use super::recording_state::AudioChunk;
use serde::{Serialize, Deserialize};

use super::ffmpeg::find_ffmpeg_path;
use super::checkpoint_files::{chunk_path, concat_list, part_path, remove_partial_files, valid_checkpoints, CheckpointPolicy};
use std::path::Path;

/// Audio data without device type (we only store mixed audio)
#[derive(Clone)]
struct AudioData {
    data: Vec<f32>,
    // sample_rate: u32,
}

/// Incremental audio saver that writes checkpoints every 30 seconds
/// to minimize memory usage and enable crash recovery
pub struct IncrementalAudioSaver {
    checkpoint_buffer: Vec<AudioData>,
    checkpoint_count: u32,
    checkpoints_dir: PathBuf,
    meeting_folder: PathBuf,
    sample_rate: u32,
    /// When to write, and how to back off while writes fail (disk full...).
    policy: CheckpointPolicy,
    /// Samples discarded because writes kept failing for longer than the memory cap allows.
    dropped_samples: usize,
}

/// While checkpoint writes fail, this many intervals (30 s each) of audio wait in memory
/// (about 5 minutes, 57 MB at 48 kHz mono) before the oldest is discarded.
const MAX_BUFFERED_INTERVALS: usize = 10;

impl IncrementalAudioSaver {
    /// Create a new incremental saver
    ///
    /// # Arguments
    /// * `meeting_folder` - Path to the meeting folder (contains .checkpoints/)
    /// * `sample_rate` - Sample rate of audio (typically 48000)
    pub fn new(meeting_folder: PathBuf, sample_rate: u32) -> Result<Self> {
        let checkpoints_dir = meeting_folder.join(".checkpoints");

        // Verify checkpoints directory exists
        if !checkpoints_dir.exists() {
            return Err(anyhow!("Checkpoints directory does not exist: {}", checkpoints_dir.display()));
        }

        Ok(Self {
            checkpoint_buffer: Vec::new(),
            checkpoint_count: 0,
            checkpoints_dir,
            meeting_folder,
            sample_rate,
            policy: CheckpointPolicy::new(sample_rate as usize * 30, MAX_BUFFERED_INTERVALS), // 30 seconds
            dropped_samples: 0,
        })
    }

    fn buffered_samples(&self) -> usize {
        self.checkpoint_buffer.iter().map(|c| c.data.len()).sum()
    }

    /// Add an audio chunk to the buffer
    /// Automatically saves a checkpoint when buffer reaches 30 seconds.
    /// A failed write (disk full, FFmpeg problem) never fails the recording: the audio stays in
    /// memory, the write is retried one interval later, and the failure is logged.
    pub fn add_chunk(&mut self, chunk: AudioChunk) -> Result<()> {
        self.checkpoint_buffer.push(AudioData { data: chunk.data });

        let total_samples = self.buffered_samples();
        if self.policy.should_save(total_samples) {
            match self.save_checkpoint() {
                Ok(()) => {
                    self.checkpoint_buffer.clear();
                    self.policy.on_success();
                }
                Err(e) => {
                    error!("Checkpoint write failed ({} s of audio kept in memory, will retry): {}",
                           total_samples / self.sample_rate.max(1) as usize, e);
                    self.policy.on_failure(total_samples);
                }
            }
        }
        self.enforce_memory_cap();
        Ok(())
    }

    /// Bounded memory: if writes keep failing, discard the oldest audio beyond the cap.
    fn enforce_memory_cap(&mut self) {
        let mut excess = self.policy.excess(self.buffered_samples());
        if excess == 0 {
            return;
        }
        let mut dropped = 0usize;
        while excess > 0 && self.checkpoint_buffer.len() > 1 {
            let len = self.checkpoint_buffer.remove(0).data.len();
            dropped += len;
            excess = excess.saturating_sub(len);
        }
        if dropped > 0 {
            if self.dropped_samples == 0 {
                error!("Checkpoints cannot be written: discarding the oldest audio to keep memory bounded");
            }
            self.dropped_samples += dropped;
            self.policy.on_dropped(dropped);
        }
    }

    /// Save current buffer as a checkpoint file (written as `.part`, then renamed, so a crash or a
    /// full disk never leaves a truncated chunk behind)
    fn save_checkpoint(&mut self) -> Result<()> {
        // Concatenate all chunks in buffer
        let audio_data: Vec<f32> = self.checkpoint_buffer
            .iter()
            .flat_map(|c| &c.data)
            .cloned()
            .collect();

        if audio_data.is_empty() {
            warn!("Attempted to save empty checkpoint, skipping");
            return Ok(());
        }

        let checkpoint_path = chunk_path(&self.checkpoints_dir, self.checkpoint_count);
        let part = part_path(&checkpoint_path);

        let written = encode_single_audio(
            bytemuck::cast_slice(&audio_data),
            self.sample_rate,
            1,  // mono
            &part,
        )
        .map_err(anyhow::Error::from)
        .and_then(|_| std::fs::rename(&part, &checkpoint_path).map_err(anyhow::Error::from));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }

        let duration_seconds = audio_data.len() as f32 / self.sample_rate.max(1) as f32;
        self.checkpoint_count += 1;

        info!("Saved checkpoint {}: {:.2}s of audio ({} samples)",
              self.checkpoint_count,
              duration_seconds,
              audio_data.len());

        Ok(())
    }

    /// Finalize the recording: save final checkpoint, merge all checkpoints, cleanup
    ///
    /// Returns the path to the final merged audio.mp4 file
    pub async fn finalize(&mut self) -> Result<PathBuf> {
        info!("Finalizing incremental recording...");

        // Save final buffer if not empty. If this last write fails the earlier checkpoints are
        // still merged: the recording keeps everything that reached the disk.
        if !self.checkpoint_buffer.is_empty() {
            info!("Saving final checkpoint with remaining {} chunks", self.checkpoint_buffer.len());
            match self.save_checkpoint() {
                Ok(()) => self.checkpoint_buffer.clear(),
                Err(e) => error!("Final checkpoint could not be written ({} s of audio lost): {}",
                                 self.buffered_samples() / self.sample_rate.max(1) as usize, e),
            }
        }
        if self.dropped_samples > 0 {
            error!("{} s of audio were discarded during the recording because checkpoints could not be written",
                   self.dropped_samples / self.sample_rate.max(1) as usize);
        }

        let folder = self.meeting_folder.clone();
        let merged = tokio::task::spawn_blocking(move || merge_checkpoint_dir(&folder))
            .await
            .map_err(|e| anyhow!("Merge task failed: {}", e))??;
        let final_audio_path = self.meeting_folder.join("audio.mp4");

        // Clean up checkpoints directory (only after a successful merge)
        info!("Cleaning up {} checkpoint files", merged);
        if std::fs::remove_dir_all(&self.checkpoints_dir).is_err() {
            warn!("Failed to clean up checkpoints");
            // Non-fatal - user can manually delete
        }

        info!("Finalized recording");

        Ok(final_audio_path)
    }

    /// Get the meeting folder path
    pub fn get_meeting_folder(&self) -> &PathBuf {
        &self.meeting_folder
    }

    /// Get current checkpoint count
    pub fn get_checkpoint_count(&self) -> u32 {
        self.checkpoint_count
    }
}

/// Merges the complete chunks of `<meeting_folder>/.checkpoints` into `<meeting_folder>/audio.mp4`
/// with an FFmpeg concat (no re-encoding) and returns how many chunks were merged. A missing or
/// empty chunk is skipped, so a damaged recording still yields everything that was written. The
/// output is written as `audio.mp4.part` and renamed, and the chunks are never deleted here.
pub fn merge_checkpoint_dir(meeting_folder: &Path) -> Result<u32> {
    let dir = meeting_folder.join(".checkpoints");
    remove_partial_files(&dir);
    let files = valid_checkpoints(&dir)
        .map_err(|e| anyhow!("Cannot read the checkpoints directory {}: {}", dir.display(), e))?;
    if files.is_empty() {
        return Err(anyhow!("No audio checkpoints to merge - recording may have failed"));
    }
    info!("Merging {} checkpoints into final audio file...", files.len());

    // Absolute paths: ffmpeg's safe mode refuses relative ones.
    let absolute: Vec<PathBuf> = files.iter().map(|p| p.canonicalize().unwrap_or_else(|_| p.clone())).collect();
    let list_file = dir.join("concat_list.txt");
    std::fs::write(&list_file, concat_list(&absolute))
        .map_err(|e| anyhow!("Cannot write the concat list: {}", e))?;

    let ffmpeg_path = find_ffmpeg_path()
        .ok_or_else(|| anyhow!("FFmpeg not found. Please install FFmpeg to finalize recordings."))?;

    let output = meeting_folder.join("audio.mp4");
    let part = part_path(&output);

    let mut command = std::process::Command::new(ffmpeg_path);
    command
        .args(["-f", "concat", "-safe", "0", "-i"])
        .arg(&list_file)
        .args(["-c", "copy", "-f", "mp4", "-y"])
        .arg(&part);

    // Hide console window on Windows to prevent CMD popup during finalization
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let result = command.output().map_err(|e| anyhow!("Failed to run FFmpeg: {}", e))?;
    if !result.status.success() {
        let _ = std::fs::remove_file(&part);
        error!("FFmpeg merge failed");
        return Err(anyhow!("FFmpeg concat failed: {}", String::from_utf8_lossy(&result.stderr)));
    }
    let produced = std::fs::metadata(&part).map(|m| m.len() > 0).unwrap_or(false);
    if !produced {
        let _ = std::fs::remove_file(&part);
        return Err(anyhow!("Merged audio file was not created: {}", output.display()));
    }
    std::fs::rename(&part, &output).map_err(|e| anyhow!("Cannot move the merged audio into place: {}", e))?;
    let _ = std::fs::remove_file(&list_file);

    info!("Successfully merged {} checkpoints", files.len());
    Ok(files.len() as u32)
}

/// Audio recovery status for transcript recovery feature
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioRecoveryStatus {
    pub status: String, // "success" | "partial" | "failed" | "none"
    pub chunk_count: u32,
    pub estimated_duration_seconds: f64,
    pub audio_file_path: Option<String>,
    pub message: String,
}

/// Recover audio from checkpoint files
/// This is called by the transcript recovery system to merge audio chunks after a crash
#[tauri::command]
pub async fn recover_audio_from_checkpoints(
    meeting_folder: String,
    _sample_rate: u32
) -> Result<AudioRecoveryStatus, String> {
    info!("Starting audio recovery");

    let folder_path = PathBuf::from(&meeting_folder);
    let checkpoints_dir = folder_path.join(".checkpoints");

    let none = |message: &str| AudioRecoveryStatus {
        status: "none".to_string(),
        chunk_count: 0,
        estimated_duration_seconds: 0.0,
        audio_file_path: None,
        message: message.to_string(),
    };

    if !checkpoints_dir.exists() {
        info!("No checkpoints directory found");
        return Ok(none("No audio checkpoints found"));
    }
    let chunk_count = match valid_checkpoints(&checkpoints_dir) {
        Ok(files) => files.len() as u32,
        Err(e) => return Err(format!("Failed to read checkpoints directory: {}", e)),
    };
    if chunk_count == 0 {
        info!("No checkpoint files found");
        return Ok(none("No audio checkpoint files found"));
    }
    let estimated_duration = (chunk_count as f64) * 30.0; // 30 seconds per chunk
    info!("Found {} checkpoint files, estimated duration: {:.2}s", chunk_count, estimated_duration);

    let folder = folder_path.clone();
    let merged = tokio::task::spawn_blocking(move || merge_checkpoint_dir(&folder))
        .await
        .map_err(|e| format!("Recovery task failed: {}", e))?;

    Ok(match merged {
        Ok(count) => {
            info!("Successfully recovered audio from {} checkpoints", count);
            AudioRecoveryStatus {
                status: "success".to_string(),
                chunk_count: count,
                estimated_duration_seconds: count as f64 * 30.0,
                audio_file_path: Some(folder_path.join("audio.mp4").to_string_lossy().into_owned()),
                message: format!("Successfully recovered {} audio chunks", count),
            }
        }
        Err(e) => {
            error!("Audio recovery failed: {}", e);
            AudioRecoveryStatus {
                status: "failed".to_string(),
                chunk_count,
                estimated_duration_seconds: estimated_duration,
                audio_file_path: None,
                message: format!("Audio recovery failed: {}", e),
            }
        }
    })
}

/// Clean up checkpoint files after successful recording or recovery
/// This command is called by the frontend after successful save to clean up checkpoint files
#[tauri::command]
pub async fn cleanup_checkpoints(meeting_folder: String) -> Result<(), String> {
    info!("Cleaning up checkpoints");

    let folder_path = PathBuf::from(&meeting_folder);
    let checkpoints_dir = folder_path.join(".checkpoints");

    if checkpoints_dir.exists() {
        std::fs::remove_dir_all(&checkpoints_dir)
            .map_err(|e| format!("Failed to remove checkpoints directory: {}", e))?;
        info!("Successfully cleaned up checkpoints directory");
    } else {
        info!("No checkpoints directory to clean up");
    }

    Ok(())
}

/// Check if a meeting folder has audio checkpoint files
/// Returns true if .checkpoints/ directory exists and contains .mp4 files
#[tauri::command]
pub async fn has_audio_checkpoints(meeting_folder: String) -> Result<bool, String> {
    let folder_path = PathBuf::from(&meeting_folder);
    let checkpoints_dir = folder_path.join(".checkpoints");

    // Check if checkpoints directory exists
    if !checkpoints_dir.exists() {
        return Ok(false);
    }

    let has_mp4_files = !valid_checkpoints(&checkpoints_dir)
        .map_err(|e| format!("Failed to read checkpoints directory: {}", e))?
        .is_empty();

    Ok(has_mp4_files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use super::super::recording_state::DeviceType;

    #[tokio::test]
    async fn test_checkpoint_creation() {
        // Create temp meeting folder
        let temp_dir = tempdir().unwrap();
        let meeting_folder = temp_dir.path().join("Test_Meeting");
        std::fs::create_dir_all(&meeting_folder).unwrap();
        std::fs::create_dir_all(meeting_folder.join(".checkpoints")).unwrap();

        let mut saver = IncrementalAudioSaver::new(
            meeting_folder.clone(),
            48000
        ).unwrap();

        // Add 60 seconds worth of audio (should create 2 checkpoints)
        for i in 0..120 {  // 120 chunks of 0.5s each
            let chunk = AudioChunk {
                data: vec![0.5f32; 24000],  // 0.5s at 48kHz
                sample_rate: 48000,
                timestamp: i as f64 * 0.5,  // timestamp in seconds
                chunk_id: i as u64,
                device_type: DeviceType::Microphone,
            };
            saver.add_chunk(chunk).unwrap();
        }

        // Verify 2 checkpoints created
        assert_eq!(saver.checkpoint_count, 2);

        // Finalize and verify merge
        let final_path = saver.finalize().await.unwrap();
        assert!(final_path.exists());

        // Verify checkpoints directory deleted
        assert!(!meeting_folder.join(".checkpoints").exists());
    }

    #[tokio::test]
    async fn test_empty_recording() {
        let temp_dir = tempdir().unwrap();
        let meeting_folder = temp_dir.path().join("Empty_Test");
        std::fs::create_dir_all(&meeting_folder).unwrap();
        std::fs::create_dir_all(meeting_folder.join(".checkpoints")).unwrap();

        let mut saver = IncrementalAudioSaver::new(
            meeting_folder.clone(),
            48000
        ).unwrap();

        // Try to finalize without adding any chunks
        let result = saver.finalize().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No audio checkpoints"));
    }
}
