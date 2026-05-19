//! Export functionality for bundling recordings into .brehonrec archives.
//!
//! The `.brehonrec` format is a self-contained archive for sharing factory
//! recordings across machines. It bundles all agent recordings, session
//! metadata, and Brehon events into a single compressed file.
//!
//! # Archive Structure
//!
//! ```text
//! session.brehonrec (tar + zstd compressed)
//! ├── manifest.json         # Version info, session metadata
//! ├── recordings/           # Individual agent recordings
//! │   ├── supervisor.rec
//! │   ├── swift-fox.rec
//! │   └── happy-elephant.rec
//! └── events.json           # Brehon events captured during recording
//! ```

use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::format::FormatError;

/// Magic bytes for .brehonrec archive: "AGARC\x00\x00\x01"
pub const ARCHIVE_MAGIC: [u8; 8] = [b'A', b'G', b'A', b'R', b'C', 0x00, 0x00, 0x01];

/// Legacy magic bytes for backward-compatible reading: "CASARC\x00\x01"
pub const LEGACY_ARCHIVE_MAGIC: [u8; 8] = [b'C', b'A', b'S', b'A', b'R', b'C', 0x00, 0x01];

/// Current archive format version
pub const ARCHIVE_VERSION: u16 = 1;

/// Manifest containing archive metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveManifest {
    /// Archive format version
    pub version: u16,
    /// When the archive was created
    pub created_at: DateTime<Utc>,
    /// Session ID this archive contains
    pub session_id: String,
    /// Session name (human-readable, e.g., "happy-elephant")
    pub session_name: Option<String>,
    /// Project directory where recording was made
    pub project_dir: Option<String>,
    /// List of agents in the recording
    pub agents: Vec<AgentManifest>,
    /// Total duration in milliseconds (max across all agents)
    pub total_duration_ms: u64,
    /// Epic ID if recording was for an epic
    pub epic_id: Option<String>,
    /// Epic title
    pub epic_title: Option<String>,
    /// Brehon version that created this archive
    pub brehon_version: String,
}

/// Manifest entry for a single agent recording.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentManifest {
    /// Agent name
    pub name: String,
    /// Agent role (supervisor, worker)
    pub role: String,
    /// Recording file path within archive
    pub recording_path: String,
    /// Recording duration in milliseconds
    pub duration_ms: u64,
    /// Number of events in recording
    pub event_count: u64,
    /// File size in bytes
    pub file_size: u64,
    /// Initial terminal dimensions
    pub cols: u16,
    pub rows: u16,
}

/// Brehon event captured during recording session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedEvent {
    /// Timestamp when event occurred (ms since session start)
    pub timestamp_ms: u64,
    /// Event type (task_created, task_completed, message, etc.)
    pub event_type: String,
    /// Agent that triggered the event
    pub agent: Option<String>,
    /// Event data (JSON object)
    pub data: serde_json::Value,
}

/// Configuration for archive export.
#[derive(Debug, Clone)]
pub struct ExportConfig {
    /// Compression level (1-22, default: 3)
    pub compression_level: i32,
    /// Include Brehon events in export
    pub include_events: bool,
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            compression_level: 3,
            include_events: true,
        }
    }
}

/// Statistics from an export operation.
#[derive(Debug, Clone)]
pub struct ExportStats {
    /// Number of agent recordings exported
    pub agent_count: usize,
    /// Total uncompressed size
    pub uncompressed_size: u64,
    /// Final compressed size
    pub compressed_size: u64,
    /// Compression ratio
    pub compression_ratio: f64,
    /// Output file path
    pub output_path: PathBuf,
}

/// Errors that can occur during export.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("No recordings found for session: {0}")]
    NoRecordings(String),

    #[error("Recording file not found: {0}")]
    RecordingNotFound(PathBuf),

    #[error("Failed to read recording {path}: {source}")]
    RecordingRead {
        path: PathBuf,
        #[source]
        source: FormatError,
    },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Invalid archive: {0}")]
    InvalidArchive(String),
}

/// Result type for export operations.
pub type ExportResult<T> = Result<T, ExportError>;

fn write_archive_preamble<W: Write>(writer: &mut W) -> ExportResult<()> {
    writer.write_all(&ARCHIVE_MAGIC)?;
    writer.write_all(&ARCHIVE_VERSION.to_le_bytes())?;
    Ok(())
}

fn validate_archive_preamble<R: Read>(reader: &mut R) -> ExportResult<()> {
    let mut magic = [0u8; 8];
    read_archive_bytes(reader, &mut magic)?;
    if magic != ARCHIVE_MAGIC && magic != LEGACY_ARCHIVE_MAGIC {
        return Err(ExportError::InvalidArchive(
            "Invalid archive magic".to_string(),
        ));
    }

    let mut version_bytes = [0u8; 2];
    read_archive_bytes(reader, &mut version_bytes)?;
    let version = u16::from_le_bytes(version_bytes);
    if version != ARCHIVE_VERSION {
        return Err(ExportError::InvalidArchive(format!(
            "Unsupported archive version: {version}"
        )));
    }

    Ok(())
}

fn read_archive_bytes<R: Read>(reader: &mut R, buf: &mut [u8]) -> ExportResult<()> {
    match reader.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Err(
            ExportError::InvalidArchive("Archive preamble truncated".to_string()),
        ),
        Err(err) => Err(ExportError::Io(err)),
    }
}

/// Export a session's recordings to a .brehonrec archive.
///
/// # Arguments
///
/// * `session_id` - Session ID to export
/// * `recordings_dir` - Base directory containing recordings (~/.brehon/recordings)
/// * `output_path` - Path for the output .brehonrec file
/// * `config` - Export configuration
/// * `events` - Optional list of Brehon events to include
///
/// # Returns
///
/// Export statistics including file sizes and compression ratio.
pub fn export_session(
    session_id: &str,
    recordings_dir: &Path,
    output_path: &Path,
    config: &ExportConfig,
    manifest_extra: Option<ManifestExtra>,
    events: Option<Vec<RecordedEvent>>,
) -> ExportResult<ExportStats> {
    // Find the session directory
    let session_dir = recordings_dir.join(session_id);
    if !session_dir.exists() {
        return Err(ExportError::SessionNotFound(session_id.to_string()));
    }

    // Find all .rec files in the session directory
    let rec_files: Vec<_> = std::fs::read_dir(&session_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "rec"))
        .map(|e| e.path())
        .collect();

    if rec_files.is_empty() {
        return Err(ExportError::NoRecordings(session_id.to_string()));
    }

    // Build agent manifests by reading each recording header
    let mut agents = Vec::new();
    let mut max_duration = 0u64;
    let mut total_uncompressed = 0u64;

    for rec_path in &rec_files {
        let agent_manifest = read_agent_manifest(rec_path)?;
        max_duration = max_duration.max(agent_manifest.duration_ms);
        total_uncompressed += agent_manifest.file_size;
        agents.push(agent_manifest);
    }

    // Build the main manifest
    let manifest = ArchiveManifest {
        version: ARCHIVE_VERSION,
        created_at: Utc::now(),
        session_id: session_id.to_string(),
        session_name: manifest_extra.as_ref().and_then(|e| e.session_name.clone()),
        project_dir: manifest_extra.as_ref().and_then(|e| e.project_dir.clone()),
        agents,
        total_duration_ms: max_duration,
        epic_id: manifest_extra.as_ref().and_then(|e| e.epic_id.clone()),
        epic_title: manifest_extra.as_ref().and_then(|e| e.epic_title.clone()),
        brehon_version: env!("CARGO_PKG_VERSION").to_string(),
    };

    // Create the archive
    let mut output_file = File::create(output_path)?;
    write_archive_preamble(&mut output_file)?;
    let encoder = zstd::stream::write::Encoder::new(output_file, config.compression_level)?;
    let mut builder = tar::Builder::new(encoder);

    // Add manifest.json
    let manifest_json = serde_json::to_vec_pretty(&manifest)?;
    add_file_to_tar(&mut builder, "manifest.json", &manifest_json)?;

    // Add recording files
    for rec_path in &rec_files {
        let file_name = rec_path
            .file_name()
            .ok_or_else(|| {
                ExportError::InvalidArchive(format!(
                    "Recording path has no filename: {}",
                    rec_path.display()
                ))
            })?
            .to_string_lossy();
        let archive_path = format!("recordings/{file_name}");
        add_path_to_tar(&mut builder, &archive_path, rec_path)?;
    }

    // Add events if provided
    if config.include_events {
        if let Some(events) = events {
            let events_json = serde_json::to_vec_pretty(&events)?;
            add_file_to_tar(&mut builder, "events.json", &events_json)?;
        }
    }

    // Finalize the archive
    let encoder = builder.into_inner()?;
    encoder.finish()?;

    // Get the final file size
    let compressed_size = std::fs::metadata(output_path)?.len();
    let compression_ratio = if total_uncompressed > 0 {
        compressed_size as f64 / total_uncompressed as f64
    } else {
        1.0
    };

    Ok(ExportStats {
        agent_count: manifest.agents.len(),
        uncompressed_size: total_uncompressed,
        compressed_size,
        compression_ratio,
        output_path: output_path.to_path_buf(),
    })
}

/// Extra metadata for the manifest (provided by caller).
#[derive(Debug, Clone, Default)]
pub struct ManifestExtra {
    pub session_name: Option<String>,
    pub project_dir: Option<String>,
    pub epic_id: Option<String>,
    pub epic_title: Option<String>,
}

/// Read agent manifest from a recording file.
fn read_agent_manifest(rec_path: &Path) -> ExportResult<AgentManifest> {
    use crate::reader::RecordingReader;

    let reader = match RecordingReader::open(rec_path) {
        Ok(reader) => reader,
        Err(FormatError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ExportError::RecordingNotFound(rec_path.to_path_buf()));
        }
        Err(source) => {
            return Err(ExportError::RecordingRead {
                path: rec_path.to_path_buf(),
                source,
            });
        }
    };

    let header = reader.header();
    let file_size = std::fs::metadata(rec_path)?.len();

    // Use role from header, fallback to "worker" for older recordings
    let role = if header.agent_role.is_empty() {
        "worker".to_string()
    } else {
        header.agent_role.clone()
    };

    Ok(AgentManifest {
        name: header.agent_name.clone(),
        role,
        recording_path: format!("recordings/{}.rec", header.agent_name),
        duration_ms: reader.duration_ms(),
        event_count: reader.total_events(),
        file_size,
        cols: header.cols,
        rows: header.rows,
    })
}

/// Add a byte slice as a file to the tar archive.
fn add_file_to_tar<W: Write>(
    builder: &mut tar::Builder<W>,
    path: &str,
    data: &[u8],
) -> ExportResult<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    );
    header.set_cksum();

    builder.append_data(&mut header, path, data)?;
    Ok(())
}

/// Add a file from disk to the tar archive.
fn add_path_to_tar<W: Write>(
    builder: &mut tar::Builder<W>,
    archive_path: &str,
    disk_path: &Path,
) -> ExportResult<()> {
    let mut file = File::open(disk_path)?;
    let metadata = file.metadata()?;

    let mut header = tar::Header::new_gnu();
    header.set_size(metadata.len());
    header.set_mode(0o644);
    header.set_mtime(
        metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );
    header.set_cksum();

    builder.append_data(&mut header, archive_path, &mut file)?;
    Ok(())
}

/// Import a .brehonrec archive to a destination directory.
///
/// # Arguments
///
/// * `archive_path` - Path to the .brehonrec file
/// * `dest_dir` - Destination directory for extracted files
///
/// # Returns
///
/// The archive manifest with session information.
pub fn import_archive(archive_path: &Path, dest_dir: &Path) -> ExportResult<ArchiveManifest> {
    let mut file = File::open(archive_path)?;
    validate_archive_preamble(&mut file)?;
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);

    // Create destination directory
    std::fs::create_dir_all(dest_dir)?;

    // Extract all files
    archive.unpack(dest_dir)?;

    // Read and return the manifest
    let manifest_path = dest_dir.join("manifest.json");
    let manifest_file = File::open(&manifest_path)?;
    let manifest: ArchiveManifest = serde_json::from_reader(BufReader::new(manifest_file))?;

    Ok(manifest)
}

/// Read manifest from a .brehonrec archive without extracting.
pub fn read_manifest(archive_path: &Path) -> ExportResult<ArchiveManifest> {
    let mut file = File::open(archive_path)?;
    validate_archive_preamble(&mut file)?;
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;

        if path.to_string_lossy() == "manifest.json" {
            let mut content = String::new();
            entry.read_to_string(&mut content)?;
            let manifest: ArchiveManifest = serde_json::from_str(&content)?;
            return Ok(manifest);
        }
    }

    Err(ExportError::InvalidArchive(
        "Missing manifest.json".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use crate::export::*;
    use crate::writer::{RecordingWriter, WriterConfig};
    use std::io::Read;
    use tempfile::TempDir;

    async fn create_exportable_session(root: &TempDir, session_id: &str) -> std::path::PathBuf {
        let recordings_dir = root.path().join("recordings");
        let config = WriterConfig {
            recordings_dir: recordings_dir.clone(),
            keyframe_interval_ms: 100,
            compression_level: 1,
            buffer_size: 16,
        };

        let writer = RecordingWriter::new(80, 24, "worker-1", session_id, "worker", config)
            .await
            .unwrap();
        writer.write_output(b"hello").await.unwrap();
        let file_path = writer.file_path().clone();
        writer.close().await.unwrap();
        file_path
    }

    #[test]
    fn test_manifest_serialization() {
        let manifest = ArchiveManifest {
            version: ARCHIVE_VERSION,
            created_at: Utc::now(),
            session_id: "test-session".to_string(),
            session_name: Some("happy-elephant".to_string()),
            project_dir: Some("/path/to/project".to_string()),
            agents: vec![AgentManifest {
                name: "supervisor".to_string(),
                role: "supervisor".to_string(),
                recording_path: "recordings/supervisor.rec".to_string(),
                duration_ms: 60000,
                event_count: 100,
                file_size: 50000,
                cols: 120,
                rows: 40,
            }],
            total_duration_ms: 60000,
            epic_id: Some("cas-1234".to_string()),
            epic_title: Some("Test Epic".to_string()),
            brehon_version: "0.5.0".to_string(),
        };

        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let parsed: ArchiveManifest = serde_json::from_str(&json).unwrap();

        assert_eq!(manifest.session_id, parsed.session_id);
        assert_eq!(manifest.agents.len(), parsed.agents.len());
    }

    #[test]
    fn test_export_config_default() {
        let config = ExportConfig::default();
        assert_eq!(config.compression_level, 3);
        assert!(config.include_events);
    }

    #[test]
    fn test_read_manifest_rejects_missing_archive_preamble() {
        let dir = TempDir::new().unwrap();
        let archive_path = dir.path().join("invalid.brehonrec");
        std::fs::write(&archive_path, b"not-a-valid-archive").unwrap();

        let result = read_manifest(&archive_path);
        assert!(matches!(result, Err(ExportError::InvalidArchive(_))));
    }

    #[test]
    fn test_import_archive_rejects_missing_archive_preamble() {
        let dir = TempDir::new().unwrap();
        let archive_path = dir.path().join("invalid.brehonrec");
        std::fs::write(&archive_path, b"not-a-valid-archive").unwrap();

        let result = import_archive(&archive_path, &dir.path().join("out"));
        assert!(matches!(result, Err(ExportError::InvalidArchive(_))));
    }

    #[tokio::test]
    async fn test_export_session_writes_archive_preamble() {
        let dir = TempDir::new().unwrap();
        create_exportable_session(&dir, "session-1").await;
        let archive_path = dir.path().join("session.brehonrec");

        export_session(
            "session-1",
            &dir.path().join("recordings"),
            &archive_path,
            &ExportConfig::default(),
            None,
            None,
        )
        .unwrap();

        let mut file = std::fs::File::open(&archive_path).unwrap();
        let mut preamble = [0u8; 10];
        file.read_exact(&mut preamble).unwrap();
        assert_eq!(&preamble[..8], &ARCHIVE_MAGIC);
        assert_eq!(
            u16::from_le_bytes([preamble[8], preamble[9]]),
            ARCHIVE_VERSION
        );
    }

    #[test]
    fn test_export_session_preserves_recording_read_error() {
        let dir = TempDir::new().unwrap();
        let session_dir = dir.path().join("recordings").join("session-1");
        std::fs::create_dir_all(&session_dir).unwrap();
        let bad_rec = session_dir.join("broken.rec");
        std::fs::write(&bad_rec, b"corrupt-rec").unwrap();

        let result = export_session(
            "session-1",
            &dir.path().join("recordings"),
            &dir.path().join("out.brehonrec"),
            &ExportConfig::default(),
            None,
            None,
        );

        assert!(matches!(
            result,
            Err(ExportError::RecordingRead { path, .. }) if path == bad_rec
        ));
    }
}
