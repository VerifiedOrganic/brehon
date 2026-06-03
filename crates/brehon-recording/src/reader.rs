//! Recording reader for Brehon Factory terminal playback.
//!
//! The [`RecordingReader`] provides fast seeking and event iteration
//! for recorded terminal sessions.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use memmap2::Mmap;
use tracing::debug;

use crate::format::{FormatError, KeyframeEntry, KeyframeIndex, RecordingEvent, RecordingHeader};

const MAX_DECOMPRESSED_RECORDING_BYTES: usize = 512 * 1024 * 1024;

struct ParsedRecording {
    header: RecordingHeader,
    index: KeyframeIndex,
    events_start_offset: usize,
    index_offset: usize,
    skip_regions: Vec<(usize, usize)>,
}

/// Reader for recorded terminal sessions.
///
/// Provides fast seeking via keyframe index and sequential event reading.
/// The recording is decompressed on open for efficient random access.
///
/// # Example
///
/// ```ignore
/// let reader = RecordingReader::open("recording.rec")?;
///
/// // Get recording metadata
/// println!("Duration: {}ms", reader.duration_ms());
/// println!("Events: {}", reader.total_events());
///
/// // Seek to a specific timestamp
/// let position = reader.seek_to(45_000)?; // 45 seconds
///
/// // Read events from that position
/// for event in reader.read_events_from(position)? {
///     match event? {
///         RecordingEvent::Output { timestamp_ms, data } => {
///             // Process output...
///         }
///         _ => {}
///     }
/// }
/// ```
pub struct RecordingReader {
    /// Decompressed recording data
    data: RecordingData,
    /// Parsed recording header
    header: RecordingHeader,
    /// Keyframe index for fast seeking
    index: KeyframeIndex,
    /// Offset where events start (after header)
    events_start_offset: usize,
    /// Offset where keyframe index starts
    index_offset: usize,
    /// Snapshot regions to skip while iterating event bytes
    skip_regions: Vec<(usize, usize)>,
}

/// Storage for decompressed recording data.
enum RecordingData {
    /// In-memory decompressed data (for smaller recordings)
    Memory(Vec<u8>),
    /// Memory-mapped decompressed temp file (for large recordings)
    Mmap(Mmap),
}

impl RecordingData {
    fn as_slice(&self) -> &[u8] {
        match self {
            RecordingData::Memory(v) => v,
            RecordingData::Mmap(m) => m,
        }
    }
}

/// Position within the recording for iteration.
#[derive(Debug, Clone, Copy)]
pub struct ReadPosition {
    /// Byte offset within decompressed data
    offset: usize,
    /// Current timestamp in milliseconds
    pub timestamp_ms: u64,
}

impl RecordingReader {
    /// Open a recording file for reading.
    ///
    /// Decompresses the file and parses the header and keyframe index.
    /// Returns an error if the file is corrupted or invalid.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, FormatError> {
        let path = path.as_ref();
        debug!("Opening recording: {:?}", path);

        // Read and decompress file
        let file = File::open(path)?;
        let file_size = file.metadata()?.len();
        let reader = BufReader::new(file);
        let mut decoder = zstd::stream::Decoder::new(reader)?;

        let decompressed =
            decompress_to_vec_with_limit(&mut decoder, MAX_DECOMPRESSED_RECORDING_BYTES)?;

        debug!(
            "Decompressed {} bytes -> {} bytes",
            file_size,
            decompressed.len()
        );

        Self::from_decompressed(decompressed)
    }

    /// Create a reader from already decompressed data.
    ///
    /// Useful for testing or when data is already in memory.
    pub fn from_decompressed(data: Vec<u8>) -> Result<Self, FormatError> {
        let parsed = parse_recording(data.as_slice())?;

        debug!(
            "Loaded recording: {} events, {} keyframes, {}ms duration",
            parsed.index.total_events,
            parsed.index.entries.len(),
            parsed.index.total_duration_ms
        );

        Ok(Self {
            data: RecordingData::Memory(data),
            header: parsed.header,
            index: parsed.index,
            events_start_offset: parsed.events_start_offset,
            index_offset: parsed.index_offset,
            skip_regions: parsed.skip_regions,
        })
    }

    /// Open a large recording file using memory-mapped I/O.
    ///
    /// Decompresses to a temporary file and memory-maps it for efficient access.
    /// Recommended for recordings larger than 100MB decompressed.
    pub fn open_mmap<P: AsRef<Path>>(path: P) -> Result<Self, FormatError> {
        let path = path.as_ref();
        debug!("Opening recording with mmap: {:?}", path);

        // Read and decompress file
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut decoder = zstd::stream::Decoder::new(reader)?;

        let temp_file =
            decompress_to_tempfile_with_limit(&mut decoder, MAX_DECOMPRESSED_RECORDING_BYTES)?;

        // Memory-map the temp file
        let mmap = unsafe { Mmap::map(&temp_file)? };
        let parsed = parse_recording(&mmap)?;

        debug!(
            "Loaded recording (mmap): {} events, {} keyframes, {}ms duration",
            parsed.index.total_events,
            parsed.index.entries.len(),
            parsed.index.total_duration_ms
        );

        Ok(Self {
            data: RecordingData::Mmap(mmap),
            header: parsed.header,
            index: parsed.index,
            events_start_offset: parsed.events_start_offset,
            index_offset: parsed.index_offset,
            skip_regions: parsed.skip_regions,
        })
    }

    /// Get the recording header.
    pub fn header(&self) -> &RecordingHeader {
        &self.header
    }

    /// Get the keyframe index.
    pub fn keyframe_index(&self) -> &KeyframeIndex {
        &self.index
    }

    /// Get the total duration in milliseconds.
    pub fn duration_ms(&self) -> u64 {
        self.index.total_duration_ms
    }

    /// Get the total number of events.
    pub fn total_events(&self) -> u64 {
        self.index.total_events
    }

    /// Get the number of keyframes.
    pub fn keyframe_count(&self) -> usize {
        self.index.entries.len()
    }

    /// Seek to the nearest keyframe at or before the given timestamp.
    ///
    /// Returns a [`ReadPosition`] that can be used with [`Self::read_events_from`].
    /// Uses binary search on the keyframe index for O(log n) seeking.
    ///
    /// # Performance
    ///
    /// Seeking is O(log k) where k is the number of keyframes.
    /// For a 2-hour recording with 30-second keyframes (240 keyframes),
    /// this is approximately 8 comparisons.
    pub fn seek_to(&self, timestamp_ms: u64) -> Result<ReadPosition, FormatError> {
        // Clamp to recording duration
        let timestamp_ms = timestamp_ms.min(self.index.total_duration_ms);

        // Find the best keyframe using binary search
        if let Some(keyframe) = self.index.find_keyframe(timestamp_ms) {
            Ok(ReadPosition {
                offset: keyframe.event_offset as usize,
                timestamp_ms: keyframe.timestamp_ms,
            })
        } else {
            // No keyframes, start from the beginning
            Ok(ReadPosition {
                offset: self.events_start_offset,
                timestamp_ms: 0,
            })
        }
    }

    /// Seek to the beginning of the recording.
    pub fn seek_to_start(&self) -> ReadPosition {
        ReadPosition {
            offset: self.events_start_offset,
            timestamp_ms: 0,
        }
    }

    /// Read the snapshot data for a keyframe.
    ///
    /// Returns the raw snapshot bytes that can be deserialized into terminal state.
    pub fn read_snapshot(&self, keyframe: &KeyframeEntry) -> Result<Vec<u8>, FormatError> {
        let data = self.data.as_slice();
        let offset = keyframe.snapshot_offset as usize;

        if offset + 4 > data.len() {
            return Err(FormatError::UnexpectedEof);
        }

        let snapshot_len = u32::from_le_bytes(
            data[offset..offset + 4]
                .try_into()
                .map_err(|_| FormatError::UnexpectedEof)?,
        ) as usize;

        let data_start = offset + 4;
        if data_start + snapshot_len > data.len() {
            return Err(FormatError::UnexpectedEof);
        }

        Ok(data[data_start..data_start + snapshot_len].to_vec())
    }

    /// Create an iterator over events starting from the given position.
    ///
    /// Events are yielded in chronological order until the end of the recording
    /// or the keyframe index is reached.
    pub fn read_events_from(&self, position: ReadPosition) -> EventIterator<'_> {
        EventIterator {
            data: self.data.as_slice(),
            offset: position.offset,
            end_offset: self.index_offset,
            skip_regions: self.skip_regions.clone(),
            terminal: false,
        }
    }

    /// Read all events in the recording.
    pub fn read_all_events(&self) -> EventIterator<'_> {
        self.read_events_from(self.seek_to_start())
    }

    /// Read events within a timestamp range.
    ///
    /// Seeks to the keyframe before `start_ms` and iterates until `end_ms`.
    pub fn read_events_in_range(
        &self,
        start_ms: u64,
        end_ms: u64,
    ) -> Result<impl Iterator<Item = Result<RecordingEvent, FormatError>> + '_, FormatError> {
        let position = self.seek_to(start_ms)?;
        Ok(self
            .read_events_from(position)
            .take_while(move |result| match result {
                Ok(event) => event.timestamp_ms() <= end_ms,
                Err(_) => true, // Propagate errors
            }))
    }
}

/// Iterator over recording events.
pub struct EventIterator<'a> {
    data: &'a [u8],
    offset: usize,
    end_offset: usize,
    /// Regions to skip (snapshot data): Vec of (start, end) byte offsets
    skip_regions: Vec<(usize, usize)>,
    /// Structural parse error already returned
    terminal: bool,
}

impl<'a> EventIterator<'a> {
    /// Check if current offset is in a skip region and return the end of that region
    fn in_skip_region(&self) -> Option<usize> {
        for &(start, end) in &self.skip_regions {
            if self.offset >= start && self.offset < end {
                return Some(end);
            }
        }
        None
    }
}

impl<'a> Iterator for EventIterator<'a> {
    type Item = Result<RecordingEvent, FormatError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.terminal {
            return None;
        }

        // Skip over snapshot regions
        while let Some(skip_end) = self.in_skip_region() {
            self.offset = skip_end;
        }

        // Check if we've reached the index
        if self.offset >= self.end_offset {
            return None;
        }

        // Check for enough bytes for length prefix
        if self.offset + 4 > self.data.len() {
            self.terminal = true;
            return Some(Err(FormatError::UnexpectedEof));
        }

        // Read event length
        let event_len = match self.data[self.offset..self.offset + 4].try_into() {
            Ok(bytes) => u32::from_le_bytes(bytes) as usize,
            Err(_) => {
                self.terminal = true;
                return Some(Err(FormatError::UnexpectedEof));
            }
        };

        let event_start = self.offset + 4;
        let Some(event_end) = event_start.checked_add(event_len) else {
            self.terminal = true;
            return Some(Err(FormatError::UnexpectedEof));
        };

        // Check bounds
        if event_end > self.data.len() || event_end > self.end_offset + 4 {
            self.terminal = true;
            return Some(Err(FormatError::UnexpectedEof));
        }

        // Deserialize event
        let event: RecordingEvent = match bincode::deserialize(&self.data[event_start..event_end]) {
            Ok(e) => e,
            Err(e) => {
                self.terminal = true;
                return Some(Err(FormatError::Bincode(e)));
            }
        };

        // Advance offset
        self.offset = event_end;

        Some(Ok(event))
    }
}

fn decompress_to_vec_with_limit<R: Read>(
    reader: &mut R,
    limit: usize,
) -> Result<Vec<u8>, FormatError> {
    let mut limited_reader = reader.take((limit as u64).saturating_add(1));
    let mut decompressed = Vec::new();
    limited_reader.read_to_end(&mut decompressed)?;
    if decompressed.len() > limit {
        return Err(FormatError::DecompressionLimitExceeded { limit });
    }
    Ok(decompressed)
}

fn decompress_to_tempfile_with_limit<R: Read>(
    reader: &mut R,
    limit: usize,
) -> Result<File, FormatError> {
    let mut limited_reader = reader.take((limit as u64).saturating_add(1));
    let mut temp_file = tempfile::tempfile()?;
    let copied = std::io::copy(&mut limited_reader, &mut temp_file)?;
    if copied > limit as u64 {
        return Err(FormatError::DecompressionLimitExceeded { limit });
    }
    Ok(temp_file)
}

fn parse_recording(data: &[u8]) -> Result<ParsedRecording, FormatError> {
    if data.len() < 12 {
        return Err(FormatError::UnexpectedEof);
    }

    let index_offset_pos = data.len() - 8;
    let index_offset = u64::from_le_bytes(
        data[index_offset_pos..index_offset_pos + 8]
            .try_into()
            .map_err(|_| FormatError::CorruptedIndex)?,
    ) as usize;
    if index_offset >= index_offset_pos {
        return Err(FormatError::CorruptedIndex);
    }

    let header_len = u32::from_le_bytes(
        data[0..4]
            .try_into()
            .map_err(|_| FormatError::UnexpectedEof)?,
    ) as usize;
    let events_start_offset = checked_end(4, header_len)?;
    if events_start_offset > data.len() {
        return Err(FormatError::UnexpectedEof);
    }

    let header: RecordingHeader = bincode::deserialize(&data[4..events_start_offset])?;
    header.validate()?;

    if checked_end(index_offset, 4)? > index_offset_pos {
        return Err(FormatError::CorruptedIndex);
    }

    let index_len = u32::from_le_bytes(
        data[index_offset..index_offset + 4]
            .try_into()
            .map_err(|_| FormatError::CorruptedIndex)?,
    ) as usize;
    let index_data_start = checked_end(index_offset, 4)?;
    let index_data_end = checked_end(index_data_start, index_len)?;
    if index_data_end > index_offset_pos {
        return Err(FormatError::CorruptedIndex);
    }

    let index: KeyframeIndex = bincode::deserialize(&data[index_data_start..index_data_end])?;
    let skip_regions = validate_keyframe_index(data, events_start_offset, index_offset, &index)?;

    Ok(ParsedRecording {
        header,
        index,
        events_start_offset,
        index_offset,
        skip_regions,
    })
}

fn validate_keyframe_index(
    data: &[u8],
    events_start_offset: usize,
    index_offset: usize,
    index: &KeyframeIndex,
) -> Result<Vec<(usize, usize)>, FormatError> {
    let mut previous_timestamp_ms = None;
    let mut skip_regions = Vec::with_capacity(index.entries.len());

    for entry in &index.entries {
        if let Some(previous) = previous_timestamp_ms {
            if entry.timestamp_ms < previous {
                return Err(FormatError::CorruptedIndex);
            }
        }
        previous_timestamp_ms = Some(entry.timestamp_ms);

        let snapshot_offset =
            usize::try_from(entry.snapshot_offset).map_err(|_| FormatError::CorruptedIndex)?;
        let event_offset =
            usize::try_from(entry.event_offset).map_err(|_| FormatError::CorruptedIndex)?;

        if snapshot_offset < events_start_offset || event_offset < events_start_offset {
            return Err(FormatError::CorruptedIndex);
        }
        if snapshot_offset >= index_offset || event_offset >= index_offset {
            return Err(FormatError::CorruptedIndex);
        }

        let snapshot_end = snapshot_region_end(data, snapshot_offset, index_offset)?;
        if snapshot_end > event_offset {
            return Err(FormatError::CorruptedIndex);
        }

        let event = read_keyframe_event(data, event_offset, index_offset)?;
        match event {
            RecordingEvent::Keyframe {
                timestamp_ms,
                snapshot_offset: event_snapshot_offset,
                snapshot_size,
            } => {
                if timestamp_ms != entry.timestamp_ms
                    || event_snapshot_offset != entry.snapshot_offset
                    || snapshot_end
                        != checked_end(
                            snapshot_offset,
                            4 + usize::try_from(snapshot_size)
                                .map_err(|_| FormatError::CorruptedIndex)?,
                        )?
                {
                    return Err(FormatError::CorruptedIndex);
                }
            }
            _ => return Err(FormatError::CorruptedIndex),
        }

        skip_regions.push((snapshot_offset, snapshot_end));
    }

    Ok(skip_regions)
}

fn snapshot_region_end(
    data: &[u8],
    snapshot_offset: usize,
    index_offset: usize,
) -> Result<usize, FormatError> {
    if checked_end(snapshot_offset, 4)? > index_offset {
        return Err(FormatError::CorruptedIndex);
    }

    let snapshot_len = u32::from_le_bytes(
        data[snapshot_offset..snapshot_offset + 4]
            .try_into()
            .map_err(|_| FormatError::CorruptedIndex)?,
    ) as usize;
    let snapshot_end = checked_end(snapshot_offset, 4 + snapshot_len)?;
    if snapshot_end > index_offset {
        return Err(FormatError::CorruptedIndex);
    }
    Ok(snapshot_end)
}

fn read_keyframe_event(
    data: &[u8],
    event_offset: usize,
    index_offset: usize,
) -> Result<RecordingEvent, FormatError> {
    if checked_end(event_offset, 4)? > index_offset {
        return Err(FormatError::CorruptedIndex);
    }

    let event_len = u32::from_le_bytes(
        data[event_offset..event_offset + 4]
            .try_into()
            .map_err(|_| FormatError::CorruptedIndex)?,
    ) as usize;
    let event_start = checked_end(event_offset, 4)?;
    let event_end = checked_end(event_start, event_len)?;
    if event_end > index_offset {
        return Err(FormatError::CorruptedIndex);
    }

    bincode::deserialize(&data[event_start..event_end]).map_err(|_| FormatError::CorruptedIndex)
}

fn checked_end(start: usize, len: usize) -> Result<usize, FormatError> {
    start.checked_add(len).ok_or(FormatError::CorruptedIndex)
}

#[cfg(test)]
mod tests {
    use crate::reader::*;
    use crate::writer::{RecordingWriter, WriterConfig};
    use std::fs::File;
    use std::io::Cursor;
    use tempfile::TempDir;

    async fn create_test_recording(dir: &TempDir) -> std::path::PathBuf {
        let config = WriterConfig {
            recordings_dir: dir.path().to_path_buf(),
            keyframe_interval_ms: 100,
            compression_level: 1,
            buffer_size: 16,
        };

        let mut writer =
            RecordingWriter::new(80, 24, "test-agent", "test-session", "worker", config)
                .await
                .unwrap();

        // Write some events
        writer.write_output(b"Hello, ").await.unwrap();
        writer.write_output(b"World!\n").await.unwrap();
        writer.write_resize(120, 40).await.unwrap();
        writer
            .write_keyframe(b"snapshot-data-here".to_vec())
            .await
            .unwrap();
        writer.write_output(b"After keyframe").await.unwrap();

        let file_path = writer.file_path().clone();
        writer.close().await.unwrap();

        file_path
    }

    fn read_decompressed_recording(path: &std::path::Path) -> Vec<u8> {
        zstd::decode_all(File::open(path).unwrap()).unwrap()
    }

    fn rewrite_keyframe_index(data: &mut [u8], rewrite: impl FnOnce(&mut KeyframeIndex)) {
        let index_offset_pos = data.len() - 8;
        let index_offset =
            u64::from_le_bytes(data[index_offset_pos..].try_into().unwrap()) as usize;
        let index_len =
            u32::from_le_bytes(data[index_offset..index_offset + 4].try_into().unwrap()) as usize;
        let index_data_start = index_offset + 4;
        let index_data_end = index_data_start + index_len;

        let mut index: KeyframeIndex =
            bincode::deserialize(&data[index_data_start..index_data_end]).unwrap();
        rewrite(&mut index);
        let encoded = bincode::serialize(&index).unwrap();
        assert_eq!(encoded.len(), index_len);
        data[index_data_start..index_data_end].copy_from_slice(&encoded);
    }

    #[tokio::test]
    async fn test_reader_opens_file() {
        let dir = TempDir::new().unwrap();
        let path = create_test_recording(&dir).await;

        let reader = RecordingReader::open(&path).unwrap();

        assert_eq!(reader.header().cols, 80);
        assert_eq!(reader.header().rows, 24);
        assert_eq!(reader.header().agent_name, "test-agent");
        // 5 events: 2 outputs + resize + keyframe + 1 output after keyframe
        assert_eq!(reader.total_events(), 5);
        assert_eq!(reader.keyframe_count(), 1);
    }

    #[tokio::test]
    async fn test_reader_mmap_opens_file() {
        let dir = TempDir::new().unwrap();
        let path = create_test_recording(&dir).await;

        let reader = RecordingReader::open_mmap(&path).unwrap();

        assert_eq!(reader.header().cols, 80);
        assert_eq!(reader.total_events(), 5);
    }

    #[tokio::test]
    async fn test_reader_iterates_events() {
        let dir = TempDir::new().unwrap();
        let path = create_test_recording(&dir).await;

        let reader = RecordingReader::open(&path).unwrap();
        let events: Vec<_> = reader.read_all_events().collect();

        // 5 events: Output, Output, Resize, Keyframe, Output
        assert_eq!(events.len(), 5);

        // Check first event
        match &events[0] {
            Ok(RecordingEvent::Output { data, .. }) => {
                assert_eq!(data, b"Hello, ");
            }
            _ => panic!("Expected Output event"),
        }

        // Check resize event (index 2)
        match &events[2] {
            Ok(RecordingEvent::Resize { cols, rows, .. }) => {
                assert_eq!(*cols, 120);
                assert_eq!(*rows, 40);
            }
            _ => panic!("Expected Resize event"),
        }

        // Check keyframe event (index 3)
        match &events[3] {
            Ok(RecordingEvent::Keyframe { .. }) => {}
            _ => panic!("Expected Keyframe event"),
        }

        // Check last output event (index 4)
        match &events[4] {
            Ok(RecordingEvent::Output { data, .. }) => {
                assert_eq!(data, b"After keyframe");
            }
            _ => panic!("Expected Output event after keyframe"),
        }
    }

    #[tokio::test]
    async fn test_reader_seek_to_keyframe() {
        let dir = TempDir::new().unwrap();
        let path = create_test_recording(&dir).await;

        let reader = RecordingReader::open(&path).unwrap();

        // Seek to a time after the keyframe should find the keyframe
        let position = reader.seek_to(50).unwrap();

        // Should be at or near the keyframe timestamp
        assert!(position.timestamp_ms <= 50 || reader.keyframe_count() == 0);
    }

    #[tokio::test]
    async fn test_reader_read_snapshot() {
        let dir = TempDir::new().unwrap();
        let path = create_test_recording(&dir).await;

        let reader = RecordingReader::open(&path).unwrap();

        if let Some(keyframe) = reader.keyframe_index().entries.first() {
            let snapshot = reader.read_snapshot(keyframe).unwrap();
            assert_eq!(snapshot, b"snapshot-data-here");
        }
    }

    #[tokio::test]
    async fn test_reader_handles_empty_recording() {
        let dir = TempDir::new().unwrap();
        let config = WriterConfig {
            recordings_dir: dir.path().to_path_buf(),
            keyframe_interval_ms: 30000,
            compression_level: 1,
            buffer_size: 16,
        };

        let writer = RecordingWriter::new(80, 24, "empty-agent", "empty-session", "worker", config)
            .await
            .unwrap();

        let file_path = writer.file_path().clone();
        writer.close().await.unwrap();

        let reader = RecordingReader::open(&file_path).unwrap();

        assert_eq!(reader.total_events(), 0);
        assert_eq!(reader.keyframe_count(), 0);
        assert_eq!(reader.duration_ms(), 0);

        let events: Vec<_> = reader.read_all_events().collect();
        assert!(events.is_empty());
    }

    #[test]
    fn test_reader_handles_truncated_file() {
        // Create truncated data (too short)
        let result = RecordingReader::from_decompressed(vec![0, 1, 2, 3]);
        assert!(result.is_err());
    }

    #[test]
    fn test_reader_handles_invalid_index_offset() {
        // Create data with invalid index offset (pointing past end)
        let mut data = vec![0u8; 100];
        // Set index_offset to something huge
        let invalid_offset: u64 = 999999;
        data[92..100].copy_from_slice(&invalid_offset.to_le_bytes());

        let result = RecordingReader::from_decompressed(data);
        assert!(matches!(result, Err(FormatError::CorruptedIndex)));
    }

    #[test]
    fn test_event_iterator_stops_after_structural_error() {
        let mut iter = EventIterator {
            data: &[0x01, 0x00, 0x00],
            offset: 0,
            end_offset: 3,
            skip_regions: Vec::new(),
            terminal: false,
        };

        assert!(matches!(iter.next(), Some(Err(FormatError::UnexpectedEof))));
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_decompress_with_limit_rejects_oversized_output() {
        let compressed = zstd::encode_all(Cursor::new(vec![0u8; 32]), 1).unwrap();
        let mut decoder = zstd::stream::Decoder::new(Cursor::new(compressed)).unwrap();

        let result = decompress_to_vec_with_limit(&mut decoder, 16);
        assert!(matches!(
            result,
            Err(FormatError::DecompressionLimitExceeded { limit: 16 })
        ));
    }

    #[test]
    fn test_decompress_to_temp_with_limit_rejects_oversized_output() {
        let compressed = zstd::encode_all(Cursor::new(vec![0u8; 32]), 1).unwrap();
        let mut decoder = zstd::stream::Decoder::new(Cursor::new(compressed)).unwrap();

        let result = decompress_to_tempfile_with_limit(&mut decoder, 16);
        assert!(matches!(
            result,
            Err(FormatError::DecompressionLimitExceeded { limit: 16 })
        ));
    }

    #[tokio::test]
    async fn test_reader_rejects_non_monotonic_keyframe_index() {
        let dir = TempDir::new().unwrap();
        let config = WriterConfig {
            recordings_dir: dir.path().to_path_buf(),
            keyframe_interval_ms: 100,
            compression_level: 1,
            buffer_size: 16,
        };
        let mut writer =
            RecordingWriter::new(80, 24, "test-agent", "test-session", "worker", config)
                .await
                .unwrap();
        writer.write_output(b"first").await.unwrap();
        writer
            .write_keyframe(b"snapshot-one".to_vec())
            .await
            .unwrap();
        writer.write_output(b"second").await.unwrap();
        writer
            .write_keyframe(b"snapshot-two".to_vec())
            .await
            .unwrap();
        let path = writer.file_path().clone();
        writer.close().await.unwrap();
        let mut data = read_decompressed_recording(&path);

        rewrite_keyframe_index(&mut data, |index| {
            index.entries[0].timestamp_ms = index.entries[1].timestamp_ms.saturating_add(1);
        });

        let result = RecordingReader::from_decompressed(data);
        assert!(matches!(result, Err(FormatError::CorruptedIndex)));
    }

    #[tokio::test]
    async fn test_reader_rejects_out_of_bounds_keyframe_offsets() {
        let dir = TempDir::new().unwrap();
        let path = create_test_recording(&dir).await;
        let mut data = read_decompressed_recording(&path);
        let invalid_snapshot_offset = (data.len() - 4) as u64;

        rewrite_keyframe_index(&mut data, |index| {
            index.entries[0].snapshot_offset = invalid_snapshot_offset;
        });

        let result = RecordingReader::from_decompressed(data);
        assert!(matches!(result, Err(FormatError::CorruptedIndex)));
    }

    #[tokio::test]
    async fn test_seek_performance() {
        // This test verifies that seeking is fast (not a strict benchmark)
        let dir = TempDir::new().unwrap();
        let config = WriterConfig {
            recordings_dir: dir.path().to_path_buf(),
            keyframe_interval_ms: 100,
            compression_level: 1,
            buffer_size: 256,
        };

        let mut writer =
            RecordingWriter::new(80, 24, "perf-agent", "perf-session", "worker", config)
                .await
                .unwrap();

        // Write many events with keyframes
        for i in 0..100 {
            writer
                .write_output(format!("Event {i}\n").as_bytes())
                .await
                .unwrap();
            if i % 10 == 0 {
                writer
                    .write_keyframe(format!("Snapshot {i}").into_bytes())
                    .await
                    .unwrap();
            }
        }

        let file_path = writer.file_path().clone();
        writer.close().await.unwrap();

        let reader = RecordingReader::open(&file_path).unwrap();

        // Measure seek time
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            let _ = reader.seek_to(50);
        }
        let elapsed = start.elapsed();

        // 1000 seeks should complete in well under 100ms
        assert!(
            elapsed.as_millis() < 100,
            "1000 seeks took {}ms, expected <100ms",
            elapsed.as_millis()
        );
    }
}
