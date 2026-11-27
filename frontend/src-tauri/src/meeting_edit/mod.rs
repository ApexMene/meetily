//! Fork: editing saved meetings. [`merge`] joins several meetings into a new one; [`trim`]
//! removes parts of a meeting (greetings, off-topic talk) from the transcript and the audio.

pub mod merge;
pub mod trim;
