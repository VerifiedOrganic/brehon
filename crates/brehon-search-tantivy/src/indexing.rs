//! Document indexing logic for TantivySearchIndex.

use std::sync::Arc;

use tantivy::doc;
use tantivy::Index;
use tantivy::TantivyDocument;
use tracing::debug;

use brehon_types::SearchEntry;

use crate::schema::SearchSchema;
use crate::TantivyError;

const MIN_MEMORY_ARENA_BYTES: usize = 15_000_000;

fn get_writer_memory_bytes() -> usize {
    std::thread::available_parallelism()
        .map(|p| std::cmp::max(p.get() * 50_000_000, MIN_MEMORY_ARENA_BYTES))
        .unwrap_or(50_000_000)
}

pub fn index_entry(
    index: &Index,
    schema: &Arc<SearchSchema>,
    entry: SearchEntry,
) -> Result<(), TantivyError> {
    let mut writer: tantivy::IndexWriter<TantivyDocument> = index
        .writer(get_writer_memory_bytes())
        .map_err(|e| TantivyError::Index(format!("Failed to create writer: {}", e)))?;

    let timestamp_str = entry.timestamp.to_rfc3339();
    let tags_joined = entry.tags.join(" ");

    let document = doc!(
        schema.id => entry.id.clone(),
        schema.content => entry.content.clone(),
        schema.tags => tags_joined,
        schema.source => entry.source.clone(),
        schema.timestamp => timestamp_str,
    );

    writer
        .add_document(document)
        .map_err(|e| TantivyError::Index(format!("Failed to add document: {}", e)))?;

    writer
        .commit()
        .map_err(|e| TantivyError::Index(format!("Failed to commit: {}", e)))?;

    debug!("Indexed entry: {}", entry.id);

    Ok(())
}

pub fn delete_entry(
    index: &Index,
    schema: &Arc<SearchSchema>,
    id: &str,
) -> Result<(), TantivyError> {
    let mut writer: tantivy::IndexWriter<TantivyDocument> = index
        .writer(get_writer_memory_bytes())
        .map_err(|e| TantivyError::Index(format!("Failed to create writer: {}", e)))?;

    writer.delete_term(tantivy::Term::from_field_text(schema.id, id));

    writer
        .commit()
        .map_err(|e| TantivyError::Index(format!("Failed to commit delete: {}", e)))?;

    debug!("Deleted entry: {}", id);

    Ok(())
}

pub fn clear_and_index_batch(
    index: &Index,
    schema: &Arc<SearchSchema>,
    entries: Vec<SearchEntry>,
) -> Result<(), TantivyError> {
    let count = entries.len();

    let mut writer: tantivy::IndexWriter<TantivyDocument> = index
        .writer(get_writer_memory_bytes())
        .map_err(|e| TantivyError::Index(format!("Failed to create writer: {}", e)))?;

    writer
        .delete_all_documents()
        .map_err(|e| TantivyError::Index(format!("Failed to clear index: {}", e)))?;

    for entry in entries {
        let timestamp_str = entry.timestamp.to_rfc3339();
        let tags_joined = entry.tags.join(" ");

        let document = doc!(
            schema.id => entry.id.clone(),
            schema.content => entry.content.clone(),
            schema.tags => tags_joined,
            schema.source => entry.source.clone(),
            schema.timestamp => timestamp_str,
        );

        writer
            .add_document(document)
            .map_err(|e| TantivyError::Index(format!("Failed to add document: {}", e)))?;
    }

    writer
        .commit()
        .map_err(|e| TantivyError::Index(format!("Failed to commit batch: {}", e)))?;

    debug!("Reindexed {} entries", count);

    Ok(())
}
