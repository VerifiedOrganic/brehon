//! Recording writer for Brehon Factory terminal sessions.
//!
//! The [`RecordingWriter`] captures PTY output with timestamps and writes
//! to compressed recording files with automatic keyframe generation.

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use tokio::fs::{self, File};
use tokio::sync::mpsc;
use tracing::debug;

use crate::format::{
    DEFAULT_KEYFRAME_INTERVAL_MS, FormatError, KeyframeEntry, KeyframeIndex, RecordingEvent,
    RecordingHeader,
};

/// Configuration for the recording writer.
#[derive(Debug, Clone)]
pub struct WriterConfig {
    /// Base directory for recordings (default: ~/.brehon/recordings)
    pub recordings_dir: PathBuf,
    /// Interval between keyframes in milliseconds
    pub keyframe_interval_ms: u64,
    /// zstd compression level (1-22, default: 3)
    pub compression_level: i32,
    /// Channel buffer size for async writes
    pub buffer_size: usize,
}

impl Default for WriterConfig {
    fn default() -> Self {
        let recordings_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".brehon")
            .join("recordings");

        Self {
            recordings_dir,
            keyframe_interval_ms: DEFAULT_KEYFRAME_INTERVAL_MS,
            compression_level: 3,
            buffer_size: 1024,
        }
    }
}

/// Messages sent to the background writer task.
enum WriteCommand {
    /// Write an event to the recording
    Event(RecordingEvent),
    /// Write a keyframe with snapshot data
    Keyframe {
        timestamp_ms: u64,
        snapshot_data: Vec<u8>,
    },
    /// Close the recording and finalize the file
    Close,
}

/// Writer for recording terminal sessions to compressed files.
///
/// Uses a background task for async I/O to avoid blocking the terminal.
/// Events are sent through a channel and written with zstd compression.
///
/// # Example
///
/// ```ignore
/// let writer = RecordingWriter::new(80, 24, "agent-1", "session-abc", "worker", config).await?;
/// writer.write_output(b"Hello, World!\n").await?;
/// writer.close().await?;
/// ```
pub struct RecordingWriter {
    /// Channel for sending write commands
    command_tx: mpsc::Sender<WriteCommand>,
    /// Handle to the background writer task
    writer_handle: tokio::task::JoinHandle<Result<WriterStats, FormatError>>,
    /// Recording start time for computing timestamps
    start_time: Instant,
    /// Last keyframe timestamp for auto-keyframe generation
    last_keyframe_ms: u64,
    /// Keyframe interval configuration
    keyframe_interval_ms: u64,
    /// Path to the recording file
    file_path: PathBuf,
}

/// Statistics from a completed recording.
#[derive(Debug, Clone)]
pub struct WriterStats {
    /// Total number of events written
    pub total_events: u64,
    /// Total duration in milliseconds
    pub total_duration_ms: u64,
    /// Number of keyframes generated
    pub keyframe_count: usize,
    /// Compressed file size in bytes
    pub file_size: u64,
}

impl RecordingWriter {
    /// Create a new recording writer.
    ///
    /// Creates the recording directory if needed and starts the background
    /// writer task. The recording file will be at:
    /// `{recordings_dir}/{session_id}/{agent_name}.rec`
    pub async fn new(
        cols: u16,
        rows: u16,
        agent_name: impl Into<String>,
        session_id: impl Into<String>,
        agent_role: impl Into<String>,
        config: WriterConfig,
    ) -> Result<Self, FormatError> {
        let agent_name = agent_name.into();
        let session_id = session_id.into();
        let agent_role = agent_role.into();

        // Create recording directory
        let session_dir = config.recordings_dir.join(&session_id);
        fs::create_dir_all(&session_dir).await?;

        let file_path = session_dir.join(format!("{agent_name}.rec"));
        debug!("Creating recording at {:?}", file_path);

        // Create header
        let header = RecordingHeader::new(cols, rows, agent_name, session_id, agent_role);

        // Create channel for async writes
        let (tx, rx) = mpsc::channel(config.buffer_size);

        // Start background writer task
        let writer_handle = tokio::spawn(background_writer(
            file_path.clone(),
            header,
            rx,
            config.compression_level,
        ));

        Ok(Self {
            command_tx: tx,
            writer_handle,
            start_time: Instant::now(),
            last_keyframe_ms: 0,
            keyframe_interval_ms: config.keyframe_interval_ms,
            file_path,
        })
    }

    /// Get the current timestamp in milliseconds since recording start.
    fn current_timestamp_ms(&self) -> u64 {
        self.start_time.elapsed().as_millis() as u64
    }

    /// Write PTY output bytes to the recording.
    ///
    /// This is non-blocking - the data is sent to a background task for writing.
    pub async fn write_output(&self, data: &[u8]) -> Result<(), FormatError> {
        if data.is_empty() {
            return Ok(());
        }

        let timestamp_ms = self.current_timestamp_ms();
        let event = RecordingEvent::output(timestamp_ms, data.to_vec());

        self.command_tx
            .send(WriteCommand::Event(event))
            .await
            .map_err(|_| {
                FormatError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "Writer channel closed",
                ))
            })?;

        Ok(())
    }

    /// Write a terminal resize event to the recording.
    pub async fn write_resize(&mut self, cols: u16, rows: u16) -> Result<(), FormatError> {
        let timestamp_ms = self.current_timestamp_ms();
        let event = RecordingEvent::resize(timestamp_ms, cols, rows);

        self.command_tx
            .send(WriteCommand::Event(event))
            .await
            .map_err(|_| {
                FormatError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "Writer channel closed",
                ))
            })?;

        Ok(())
    }

    /// Write a keyframe with terminal snapshot data.
    ///
    /// Keyframes enable fast seeking during playback. Call this periodically
    /// (e.g., every 30 seconds) with a serialized terminal state snapshot.
    pub async fn write_keyframe(&mut self, snapshot_data: Vec<u8>) -> Result<(), FormatError> {
        let timestamp_ms = self.current_timestamp_ms();
        self.last_keyframe_ms = timestamp_ms;

        self.command_tx
            .send(WriteCommand::Keyframe {
                timestamp_ms,
                snapshot_data,
            })
            .await
            .map_err(|_| {
                FormatError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "Writer channel closed",
                ))
            })?;

        Ok(())
    }

    /// Check if a keyframe should be generated based on elapsed time.
    ///
    /// Returns true if `keyframe_interval_ms` has passed since the last keyframe.
    pub fn should_generate_keyframe(&self) -> bool {
        let current_ms = self.current_timestamp_ms();
        current_ms - self.last_keyframe_ms >= self.keyframe_interval_ms
    }

    /// Close the recording and finalize the file.
    ///
    /// This writes the keyframe index and flushes all buffered data.
    /// Returns statistics about the completed recording.
    pub async fn close(self) -> Result<WriterStats, FormatError> {
        // Send close command
        let _ = self.command_tx.send(WriteCommand::Close).await;

        // Wait for background task to complete
        match self.writer_handle.await {
            Ok(result) => result,
            Err(e) => Err(FormatError::Io(std::io::Error::other(format!(
                "Writer task panicked: {e}"
            )))),
        }
    }

    /// Get the path to the recording file.
    pub fn file_path(&self) -> &PathBuf {
        &self.file_path
    }
}

/// Background task that handles actual file I/O.
async fn background_writer(
    file_path: PathBuf,
    header: RecordingHeader,
    mut rx: mpsc::Receiver<WriteCommand>,
    compression_level: i32,
) -> Result<WriterStats, FormatError> {
    // Create file
    let file = File::create(&file_path).await?;
    let std_file = file.into_std().await;

    // Create zstd encoder wrapping the file
    let mut encoder = zstd::stream::Encoder::new(std_file, compression_level)?;

    // Write header
    let header_bytes = bincode::serialize(&header)?;
    let header_len = header_bytes.len() as u32;
    encoder.write_all(&header_len.to_le_bytes())?;
    encoder.write_all(&header_bytes)?;

    // Track state
    let mut keyframe_index = KeyframeIndex::new();
    let mut event_count: u64 = 0;
    let mut last_timestamp_ms: u64 = 0;
    let mut current_offset: u64 = 4 + header_bytes.len() as u64; // header_len + header

    // Process commands
    while let Some(cmd) = rx.recv().await {
        match cmd {
            WriteCommand::Event(event) => {
                last_timestamp_ms = event.timestamp_ms();

                let event_bytes = bincode::serialize(&event)?;
                let event_len = event_bytes.len() as u32;
                encoder.write_all(&event_len.to_le_bytes())?;
                encoder.write_all(&event_bytes)?;

                current_offset += 4 + event_bytes.len() as u64;
                event_count += 1;
            }

            WriteCommand::Keyframe {
                timestamp_ms,
                snapshot_data,
            } => {
                // Write snapshot data first
                let snapshot_offset = current_offset;
                let snapshot_len = snapshot_data.len() as u32;
                encoder.write_all(&snapshot_len.to_le_bytes())?;
                encoder.write_all(&snapshot_data)?;
                current_offset += 4 + snapshot_data.len() as u64;

                // Write keyframe event
                let event_offset = current_offset;
                let event = RecordingEvent::keyframe(timestamp_ms, snapshot_offset, snapshot_len);
                let event_bytes = bincode::serialize(&event)?;
                let event_len = event_bytes.len() as u32;
                encoder.write_all(&event_len.to_le_bytes())?;
                encoder.write_all(&event_bytes)?;
                current_offset += 4 + event_bytes.len() as u64;

                // Add to index
                keyframe_index.add_keyframe(KeyframeEntry {
                    timestamp_ms,
                    event_offset,
                    snapshot_offset,
                });

                last_timestamp_ms = timestamp_ms;
                event_count += 1;
            }

            WriteCommand::Close => {
                break;
            }
        }
    }

    // Finalize index
    keyframe_index.total_duration_ms = last_timestamp_ms;
    keyframe_index.total_events = event_count;

    // Write keyframe index at current position
    let index_offset = current_offset;
    let index_bytes = bincode::serialize(&keyframe_index)?;
    let index_len = index_bytes.len() as u32;
    encoder.write_all(&index_len.to_le_bytes())?;
    encoder.write_all(&index_bytes)?;

    // Write index offset as last 8 bytes
    encoder.write_all(&index_offset.to_le_bytes())?;

    // Finish compression and flush
    let std_file = encoder.finish()?;
    std_file.sync_all()?;

    // Get final file size
    let metadata = std::fs::metadata(&file_path)?;
    let file_size = metadata.len();

    debug!(
        "Recording closed: {} events, {} keyframes, {} bytes",
        event_count,
        keyframe_index.len(),
        file_size
    );

    Ok(WriterStats {
        total_events: event_count,
        total_duration_ms: last_timestamp_ms,
        keyframe_count: keyframe_index.len(),
        file_size,
    })
}

#[cfg(test)]
mod tests {
    use crate::writer::*;
    use tempfile::TempDir;

    fn test_config(dir: &TempDir) -> WriterConfig {
        WriterConfig {
            recordings_dir: dir.path().to_path_buf(),
            keyframe_interval_ms: 1000, // 1 second for testing
            compression_level: 1,
            buffer_size: 16,
        }
    }

    #[tokio::test]
    async fn test_writer_creates_file() {
        let dir = TempDir::new().unwrap();
        let config = test_config(&dir);

        let writer = RecordingWriter::new(80, 24, "agent-1", "session-123", "worker", config)
            .await
            .unwrap();

        let file_path = writer.file_path().clone();
        let stats = writer.close().await.unwrap();

        assert!(file_path.exists());
        assert_eq!(stats.total_events, 0);
        assert_eq!(stats.keyframe_count, 0);
    }

    #[tokio::test]
    async fn test_writer_output_events() {
        let dir = TempDir::new().unwrap();
        let config = test_config(&dir);

        let writer = RecordingWriter::new(80, 24, "agent-1", "session-123", "worker", config)
            .await
            .unwrap();

        writer.write_output(b"Hello, ").await.unwrap();
        writer.write_output(b"World!\n").await.unwrap();

        let stats = writer.close().await.unwrap();

        assert_eq!(stats.total_events, 2);
        assert!(stats.file_size > 0);
    }

    #[tokio::test]
    async fn test_writer_resize_events() {
        let dir = TempDir::new().unwrap();
        let config = test_config(&dir);

        let mut writer = RecordingWriter::new(80, 24, "agent-1", "session-123", "worker", config)
            .await
            .unwrap();

        writer.write_output(b"test").await.unwrap();
        writer.write_resize(120, 40).await.unwrap();
        writer.write_output(b"test2").await.unwrap();

        let stats = writer.close().await.unwrap();

        assert_eq!(stats.total_events, 3);
    }

    #[tokio::test]
    async fn test_writer_keyframes() {
        let dir = TempDir::new().unwrap();
        let config = test_config(&dir);

        let mut writer = RecordingWriter::new(80, 24, "agent-1", "session-123", "worker", config)
            .await
            .unwrap();

        writer.write_output(b"before keyframe").await.unwrap();
        writer
            .write_keyframe(b"snapshot data here".to_vec())
            .await
            .unwrap();
        writer.write_output(b"after keyframe").await.unwrap();

        let stats = writer.close().await.unwrap();

        assert_eq!(stats.total_events, 3); // 2 outputs + 1 keyframe
        assert_eq!(stats.keyframe_count, 1);
    }

    #[tokio::test]
    async fn test_should_generate_keyframe() {
        let dir = TempDir::new().unwrap();
        let mut config = test_config(&dir);
        config.keyframe_interval_ms = 10; // Very short interval for testing

        let writer = RecordingWriter::new(80, 24, "agent-1", "session-123", "worker", config)
            .await
            .unwrap();

        // Initially no keyframe needed (we're at 0ms)
        assert!(!writer.should_generate_keyframe());

        // Wait for interval to pass
        tokio::time::sleep(tokio::time::Duration::from_millis(15)).await;

        assert!(writer.should_generate_keyframe());

        writer.close().await.unwrap();
    }

    #[tokio::test]
    async fn test_empty_output_ignored() {
        let dir = TempDir::new().unwrap();
        let config = test_config(&dir);

        let writer = RecordingWriter::new(80, 24, "agent-1", "session-123", "worker", config)
            .await
            .unwrap();

        writer.write_output(b"").await.unwrap();
        writer.write_output(b"real data").await.unwrap();
        writer.write_output(b"").await.unwrap();

        let stats = writer.close().await.unwrap();

        assert_eq!(stats.total_events, 1); // Only the non-empty output
    }

    #[tokio::test]
    async fn test_file_path_structure() {
        let dir = TempDir::new().unwrap();
        let config = test_config(&dir);

        let writer = RecordingWriter::new(80, 24, "swift-fox", "sess-abc123", "worker", config)
            .await
            .unwrap();

        let expected_path = dir.path().join("sess-abc123").join("swift-fox.rec");

        assert_eq!(writer.file_path(), &expected_path);

        writer.close().await.unwrap();
    }
}
