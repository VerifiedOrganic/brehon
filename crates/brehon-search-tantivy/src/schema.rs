//! Tantivy schema definition for search entries.

use tantivy::schema::{Field, Schema, STORED, STRING, TEXT};

pub(crate) struct SearchSchema {
    pub(crate) schema: Schema,
    pub(crate) id: Field,
    pub(crate) content: Field,
    pub(crate) tags: Field,
    pub(crate) source: Field,
    pub(crate) category: Field,
    pub(crate) timestamp: Field,
}

impl SearchSchema {
    pub fn new() -> Self {
        let mut schema_builder = Schema::builder();

        let id = schema_builder.add_text_field("id", STRING | STORED);

        let content = schema_builder.add_text_field("content", TEXT | STORED);

        let tags = schema_builder.add_text_field("tags", TEXT | STORED);

        let source = schema_builder.add_text_field("source", STRING | STORED);

        let category = schema_builder.add_text_field("category", STRING | STORED);

        let timestamp = schema_builder.add_text_field("timestamp", STRING | STORED);

        let schema = schema_builder.build();

        Self {
            schema,
            id,
            content,
            tags,
            source,
            category,
            timestamp,
        }
    }
}

impl Default for SearchSchema {
    fn default() -> Self {
        Self::new()
    }
}
