//! BM25 query construction and result handling.

use std::sync::Arc;

use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, QueryParser, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{Index, TantivyDocument};
use tracing::debug;

use brehon_types::SearchResult;

use crate::schema::SearchSchema;
use crate::TantivyError;

pub fn search(
    index: &Index,
    schema: &Arc<SearchSchema>,
    query_str: &str,
    limit: usize,
) -> Result<Vec<SearchResult>, TantivyError> {
    search_with_filter(index, schema, query_str, None, None, limit)
}

pub fn search_with_filter(
    index: &Index,
    schema: &Arc<SearchSchema>,
    query_str: &str,
    category_filter: Option<&str>,
    source_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<SearchResult>, TantivyError> {
    let reader = index
        .reader()
        .map_err(|e| TantivyError::Index(format!("Failed to create reader: {}", e)))?;

    let searcher = reader.searcher();

    let mut queries: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();

    let content_parser = QueryParser::for_index(index, vec![schema.content]);
    let content_query = content_parser
        .parse_query(query_str)
        .map_err(|e| TantivyError::Query(format!("Failed to parse query: {}", e)))?;
    queries.push((Occur::Must, content_query));

    if let Some(category) = category_filter {
        let term = tantivy::Term::from_field_text(schema.category, category);
        let query: Box<dyn tantivy::query::Query> =
            Box::new(TermQuery::new(term, IndexRecordOption::Basic));
        queries.push((Occur::Must, query));
    }

    if let Some(source) = source_filter {
        let term = tantivy::Term::from_field_text(schema.source, source);
        let query: Box<dyn tantivy::query::Query> =
            Box::new(TermQuery::new(term, IndexRecordOption::Basic));
        queries.push((Occur::Must, query));
    }

    if let Some(tag) = query_str.strip_prefix("tag:") {
        let term = tantivy::Term::from_field_text(schema.tags, tag);
        let query: Box<dyn tantivy::query::Query> =
            Box::new(TermQuery::new(term, IndexRecordOption::Basic));
        queries = vec![(Occur::Must, query)];
    }

    let boolean_query = BooleanQuery::new(queries);

    let top_docs = searcher
        .search(&boolean_query, &TopDocs::with_limit(limit))
        .map_err(|e| TantivyError::Query(format!("Search failed: {}", e)))?;

    debug!("Found {} results for query: {}", top_docs.len(), query_str);

    let mut results = Vec::new();
    for (_score, doc_address) in top_docs {
        let doc: TantivyDocument = searcher
            .doc(doc_address)
            .map_err(|e| TantivyError::Query(format!("Failed to get document: {}", e)))?;

        let id = doc
            .get_first(schema.id)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();

        let content = doc
            .get_first(schema.content)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();

        let tags_str = doc
            .get_first(schema.tags)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();

        let matched_tags: Vec<String> =
            tags_str.split_whitespace().map(|s| s.to_string()).collect();

        let snippet = if content.len() > 200 {
            format!("{}...", &content[..200])
        } else {
            content
        };

        results.push(SearchResult {
            id,
            snippet,
            score: 1.0,
            matched_tags,
        });
    }

    Ok(results)
}

#[allow(dead_code)]
pub fn search_by_tag(
    index: &Index,
    schema: &Arc<SearchSchema>,
    tag: &str,
    limit: usize,
) -> Result<Vec<SearchResult>, TantivyError> {
    let reader = index
        .reader()
        .map_err(|e| TantivyError::Index(format!("Failed to create reader: {}", e)))?;

    let searcher = reader.searcher();

    let term = tantivy::Term::from_field_text(schema.tags, tag);
    let query = TermQuery::new(term, IndexRecordOption::Basic);

    let top_docs = searcher
        .search(&query, &TopDocs::with_limit(limit))
        .map_err(|e| TantivyError::Query(format!("Search failed: {}", e)))?;

    let mut results = Vec::new();
    for (_score, doc_address) in top_docs {
        let doc: TantivyDocument = searcher
            .doc(doc_address)
            .map_err(|e| TantivyError::Query(format!("Failed to get document: {}", e)))?;

        let id = doc
            .get_first(schema.id)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();

        let content = doc
            .get_first(schema.content)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();

        let snippet = if content.len() > 200 {
            format!("{}...", &content[..200])
        } else {
            content
        };

        results.push(SearchResult {
            id,
            snippet,
            score: 1.0,
            matched_tags: vec![tag.to_string()],
        });
    }

    Ok(results)
}

#[expect(dead_code)]
pub fn search_all(
    index: &Index,
    schema: &Arc<SearchSchema>,
    limit: usize,
) -> Result<Vec<SearchResult>, TantivyError> {
    let reader = index
        .reader()
        .map_err(|e| TantivyError::Index(format!("Failed to create reader: {}", e)))?;

    let searcher = reader.searcher();

    let query = tantivy::query::AllQuery;

    let top_docs = searcher
        .search(&query, &TopDocs::with_limit(limit))
        .map_err(|e| TantivyError::Query(format!("Search failed: {}", e)))?;

    let mut results = Vec::new();
    for (_score, doc_address) in top_docs {
        let doc: TantivyDocument = searcher
            .doc(doc_address)
            .map_err(|e| TantivyError::Query(format!("Failed to get document: {}", e)))?;

        let id = doc
            .get_first(schema.id)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();

        let content = doc
            .get_first(schema.content)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();

        let snippet = if content.len() > 200 {
            format!("{}...", &content[..200])
        } else {
            content
        };

        results.push(SearchResult {
            id,
            snippet,
            score: 1.0,
            matched_tags: vec![],
        });
    }

    Ok(results)
}
