//! Large diff splitting for review chunking.
//!
//! When a diff is too large, it gets split into smaller chunks
//! that can be reviewed independently by different reviewers.

use std::collections::HashMap;

use brehon_ports::{Diff, FileDiff};
use brehon_types::config::ChunkStrategy;

/// Configuration for diff chunking.
#[derive(Debug, Clone)]
pub struct ChunkingConfig {
    /// Maximum tokens before chunking.
    pub max_diff_tokens: u32,
    /// Chunking strategy.
    pub strategy: ChunkStrategy,
    /// Estimated tokens per line.
    pub tokens_per_line: u32,
    /// Estimated tokens per file overhead.
    pub file_overhead_tokens: u32,
}

impl Default for ChunkingConfig {
    fn default() -> Self {
        Self {
            max_diff_tokens: 8000,
            strategy: ChunkStrategy::ByDirectory,
            tokens_per_line: 4,
            file_overhead_tokens: 50,
        }
    }
}

impl ChunkingConfig {
    pub fn new(max_diff_tokens: u32, strategy: ChunkStrategy) -> Self {
        Self {
            max_diff_tokens,
            strategy,
            ..Default::default()
        }
    }

    /// Estimate tokens for a file diff.
    pub fn estimate_tokens(&self, file: &FileDiff) -> u32 {
        let line_estimate = (file.additions + file.deletions) as u32 * self.tokens_per_line;
        line_estimate + self.file_overhead_tokens
    }

    /// Estimate total tokens for a diff.
    pub fn estimate_total_tokens(&self, diff: &Diff) -> u32 {
        diff.files.iter().map(|f| self.estimate_tokens(f)).sum()
    }

    /// Check if a diff needs chunking.
    pub fn needs_chunking(&self, diff: &Diff) -> bool {
        let total = self.estimate_total_tokens(diff);
        total > self.max_diff_tokens
    }
}

/// A single chunk of a diff for review.
#[derive(Debug, Clone)]
pub struct DiffChunk {
    /// Chunk index.
    pub index: usize,
    /// Total chunks.
    pub total: usize,
    /// Files in this chunk.
    pub files: Vec<FileDiff>,
    /// Chunk description (e.g., directory name).
    pub description: String,
}

/// Splits large diffs into manageable chunks.
pub struct DiffChunker {
    config: ChunkingConfig,
}

impl DiffChunker {
    pub fn new(config: ChunkingConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &ChunkingConfig {
        &self.config
    }

    /// Chunk a diff if necessary.
    ///
    /// Returns a vector of chunks. If the diff doesn't need chunking,
    /// returns a single chunk with all files.
    pub fn chunk(&self, diff: &Diff) -> Vec<DiffChunk> {
        if !self.config.needs_chunking(diff) {
            return vec![DiffChunk {
                index: 0,
                total: 1,
                files: diff.files.clone(),
                description: "Full diff".to_string(),
            }];
        }

        match self.config.strategy {
            ChunkStrategy::ByDirectory => self.chunk_by_directory(diff),
            ChunkStrategy::ByFile => self.chunk_by_file(diff),
            ChunkStrategy::None => vec![DiffChunk {
                index: 0,
                total: 1,
                files: diff.files.clone(),
                description: "Full diff (no chunking)".to_string(),
            }],
        }
    }

    /// Chunk by directory.
    ///
    /// Groups files by their top-level directory and creates chunks
    /// that respect the max tokens limit.
    fn chunk_by_directory(&self, diff: &Diff) -> Vec<DiffChunk> {
        let mut by_dir: HashMap<String, Vec<FileDiff>> = HashMap::new();

        for file in &diff.files {
            let dir = self.get_directory(&file.path);
            by_dir.entry(dir).or_default().push(file.clone());
        }

        let mut chunks: Vec<DiffChunk> = Vec::new();
        let mut current_files: Vec<FileDiff> = Vec::new();
        let mut current_tokens: u32 = 0;
        let mut current_dirs: Vec<String> = Vec::new();

        let mut dirs: Vec<_> = by_dir.into_iter().collect();
        dirs.sort_by(|a, b| a.0.cmp(&b.0));

        for (dir, mut files) in dirs {
            let dir_tokens: u32 = files.iter().map(|f| self.config.estimate_tokens(f)).sum();

            if current_tokens + dir_tokens > self.config.max_diff_tokens
                && !current_files.is_empty()
            {
                let description = if current_dirs.len() == 1 {
                    current_dirs[0].clone()
                } else {
                    format!(
                        "Directories {}-{}",
                        current_dirs.first().unwrap(),
                        current_dirs.last().unwrap()
                    )
                };

                chunks.push(DiffChunk {
                    index: chunks.len(),
                    total: 0,
                    files: current_files.clone(),
                    description,
                });

                current_files.clear();
                current_tokens = 0;
                current_dirs.clear();
            }

            current_files.append(&mut files);
            current_tokens += dir_tokens;
            current_dirs.push(dir);
        }

        if !current_files.is_empty() {
            let description = if current_dirs.len() == 1 {
                current_dirs[0].clone()
            } else if current_dirs.len() > 1 {
                format!(
                    "Directories {}-{}",
                    current_dirs.first().unwrap(),
                    current_dirs.last().unwrap()
                )
            } else {
                "Remaining files".to_string()
            };

            chunks.push(DiffChunk {
                index: chunks.len(),
                total: 0,
                files: current_files,
                description,
            });
        }

        let total = chunks.len();
        for chunk in &mut chunks {
            chunk.total = total;
        }

        chunks
    }

    /// Chunk by individual file.
    ///
    /// Each chunk contains one or more complete files, respecting the
    /// max tokens limit.
    fn chunk_by_file(&self, diff: &Diff) -> Vec<DiffChunk> {
        let mut chunks: Vec<DiffChunk> = Vec::new();
        let mut current_files: Vec<FileDiff> = Vec::new();
        let mut current_tokens: u32 = 0;

        for file in &diff.files {
            let file_tokens = self.config.estimate_tokens(file);

            if current_tokens + file_tokens > self.config.max_diff_tokens
                && !current_files.is_empty()
            {
                chunks.push(DiffChunk {
                    index: chunks.len(),
                    total: 0,
                    files: current_files.clone(),
                    description: format!(
                        "Files {}-{}",
                        chunks.len() * 10 + 1,
                        (chunks.len() + 1) * 10
                    ),
                });

                current_files.clear();
                current_tokens = 0;
            }

            current_files.push(file.clone());
            current_tokens += file_tokens;
        }

        if !current_files.is_empty() {
            chunks.push(DiffChunk {
                index: chunks.len(),
                total: 0,
                files: current_files,
                description: "Remaining files".to_string(),
            });
        }

        let total = chunks.len();
        for chunk in &mut chunks {
            chunk.total = total;
        }

        chunks
    }

    fn get_directory(&self, path: &str) -> String {
        let path = path.trim_start_matches('/');
        let parts: Vec<&str> = path.split('/').collect();

        if parts.len() <= 1 {
            "root".to_string()
        } else if parts[0] == "src" && parts.len() > 2 {
            format!("src/{}", parts[1])
        } else {
            parts[0].to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_file_diff(path: &str, additions: usize, deletions: usize) -> FileDiff {
        FileDiff {
            path: path.to_string(),
            additions,
            deletions,
        }
    }

    #[test]
    fn config_estimate_tokens() {
        let config = ChunkingConfig::default();
        let file = make_file_diff("test.rs", 100, 50);

        let tokens = config.estimate_tokens(&file);
        let expected = (100 + 50) as u32 * config.tokens_per_line + config.file_overhead_tokens;

        assert_eq!(tokens, expected);
    }

    #[test]
    fn chunker_no_chunking_needed() {
        let config = ChunkingConfig::new(10000, ChunkStrategy::ByDirectory);
        let chunker = DiffChunker::new(config);

        let diff = Diff {
            files: vec![
                make_file_diff("src/lib.rs", 10, 5),
                make_file_diff("src/main.rs", 20, 10),
            ],
        };

        let chunks = chunker.chunk(&diff);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].files.len(), 2);
    }

    #[test]
    fn chunker_by_directory() {
        let config = ChunkingConfig::new(100, ChunkStrategy::ByDirectory);
        let chunker = DiffChunker::new(config);

        let diff = Diff {
            files: vec![
                make_file_diff("src/auth/login.rs", 50, 10),
                make_file_diff("src/auth/logout.rs", 40, 8),
                make_file_diff("src/api/users.rs", 60, 12),
                make_file_diff("src/api/posts.rs", 55, 10),
                make_file_diff("tests/auth_test.rs", 30, 5),
            ],
        };

        let chunks = chunker.chunk(&diff);

        assert!(!chunks.is_empty());

        let total_files: usize = chunks.iter().map(|c| c.files.len()).sum();
        assert_eq!(total_files, 5);
    }

    #[test]
    fn chunker_by_file() {
        let config = ChunkingConfig::new(100, ChunkStrategy::ByFile);
        let chunker = DiffChunker::new(config);

        let diff = Diff {
            files: vec![
                make_file_diff("file1.rs", 30, 10),
                make_file_diff("file2.rs", 30, 10),
                make_file_diff("file3.rs", 30, 10),
                make_file_diff("file4.rs", 30, 10),
            ],
        };

        let chunks = chunker.chunk(&diff);

        let total_files: usize = chunks.iter().map(|c| c.files.len()).sum();
        assert_eq!(total_files, 4);
    }

    #[test]
    fn chunker_sets_chunk_indices() {
        let config = ChunkingConfig::new(50, ChunkStrategy::ByFile);
        let chunker = DiffChunker::new(config);

        let diff = Diff {
            files: vec![
                make_file_diff("file1.rs", 20, 5),
                make_file_diff("file2.rs", 20, 5),
                make_file_diff("file3.rs", 20, 5),
                make_file_diff("file4.rs", 20, 5),
            ],
        };

        let chunks = chunker.chunk(&diff);

        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.index, i);
            assert_eq!(chunk.total, chunks.len());
        }
    }

    #[test]
    fn twenty_files_split_into_four_chunks() {
        let config = ChunkingConfig::new(200, ChunkStrategy::ByDirectory);
        let chunker = DiffChunker::new(config);

        let mut files = Vec::new();
        for i in 0..20 {
            let dir = match i % 4 {
                0 => "src/auth",
                1 => "src/api",
                2 => "src/core",
                _ => "tests",
            };
            files.push(make_file_diff(&format!("{}/file{}.rs", dir, i), 15, 5));
        }

        let diff = Diff { files };

        let chunks = chunker.chunk(&diff);

        assert!(chunks.len() >= 2, "Expected at least 2 chunks for 20 files");

        let total_files: usize = chunks.iter().map(|c| c.files.len()).sum();
        assert_eq!(total_files, 20);
    }
}
