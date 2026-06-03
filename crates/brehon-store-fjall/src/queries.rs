//! Query execution for event filtering.
//!
//! Translates `EventFilter` constraints to key range scans for efficient retrieval.

use fjall::PartitionHandle;

use brehon_types::{Event, EventFilter};

use crate::keys::{
    agent_index_prefix, parse_seq_from_index_key, parse_seq_from_log_key, review_index_prefix,
    task_index_prefix, KEY_META_INDEX_START_SEQ,
};
use crate::store::StoreError;

pub struct QueryExecutor {
    events: PartitionHandle,
    meta: PartitionHandle,
}

impl QueryExecutor {
    pub fn new(events: PartitionHandle, meta: PartitionHandle) -> Self {
        Self { events, meta }
    }

    pub fn execute(&self, filter: &EventFilter) -> Result<Vec<Event>, StoreError> {
        let events = if let Some(aggregate_id) = &filter.aggregate_id {
            self.query_by_aggregate(aggregate_id, filter)?
        } else if let Some(task_id) = &filter.task_id {
            self.query_by_task(task_id, filter)?
        } else if let Some(review_id) = &filter.review_id {
            self.query_by_review(review_id, filter)?
        } else if let Some(agent_id) = &filter.agent_id {
            self.query_by_agent(agent_id, filter)?
        } else {
            self.scan_all_events(filter)?
        };

        let events = self.apply_filters(events, filter);

        let events = if let Some(limit) = filter.limit {
            events.into_iter().take(limit).collect()
        } else {
            events
        };

        Ok(events)
    }

    fn query_by_aggregate(
        &self,
        aggregate_id: &str,
        filter: &EventFilter,
    ) -> Result<Vec<Event>, StoreError> {
        let condition = |event: &Event| event.aggregate_id == aggregate_id;
        self.scan_all_events_with_condition(filter, &condition)
    }

    fn query_by_task(&self, task_id: &str, filter: &EventFilter) -> Result<Vec<Event>, StoreError> {
        let index_prefix = task_index_prefix(task_id);
        let index_start_seq = self.index_start_seq()?;

        let condition = |event: &Event| match &event.kind {
            brehon_types::EventKind::TaskCreated { task_id: t } => t == task_id,
            brehon_types::EventKind::TaskAssigned { task_id: t, .. } => t == task_id,
            brehon_types::EventKind::TaskCompleted { task_id: t, .. } => t == task_id,
            brehon_types::EventKind::RunCreated { task_id: t, .. }
            | brehon_types::EventKind::RunClaimed { task_id: t, .. }
            | brehon_types::EventKind::RunClaimRenewed { task_id: t, .. }
            | brehon_types::EventKind::RunStarted { task_id: t, .. }
            | brehon_types::EventKind::RunActivityObserved { task_id: t, .. }
            | brehon_types::EventKind::RunReleased { task_id: t, .. }
            | brehon_types::EventKind::RunRetryQueued { task_id: t, .. }
            | brehon_types::EventKind::RunCompleted { task_id: t, .. }
            | brehon_types::EventKind::RunFailed { task_id: t, .. }
            | brehon_types::EventKind::RunAbandoned { task_id: t, .. }
            | brehon_types::EventKind::StaleRunMutationRejected { task_id: t, .. } => {
                t.as_str() == task_id
            }
            brehon_types::EventKind::ReviewRequested { task_id: t, .. } => t == task_id,
            brehon_types::EventKind::MergePrepared { task_id: t, .. } => t == task_id,
            brehon_types::EventKind::MergeCommitted { task_id: t, .. } => t == task_id,
            brehon_types::EventKind::MergeAborted { task_id: t, .. } => t == task_id,
            _ => false,
        };

        let mut filter_without_limit = filter.clone();
        filter_without_limit.limit = None;

        if index_start_seq == 0 {
            return self.scan_all_events_with_condition(&filter_without_limit, &condition);
        }

        let mut events =
            self.scan_all_events_before_seq(&filter_without_limit, index_start_seq, &condition)?;

        if self.has_index_data_for_prefix(&index_prefix)? {
            let mut indexed = self.query_by_index_prefix(
                &index_prefix,
                &filter_without_limit,
                index_start_seq,
                &condition,
            )?;
            events.append(&mut indexed);
        }

        if let Some(limit) = filter.limit {
            events.truncate(limit);
        }

        Ok(events)
    }

    fn query_by_review(
        &self,
        review_id: &str,
        filter: &EventFilter,
    ) -> Result<Vec<Event>, StoreError> {
        let index_prefix = review_index_prefix(review_id);
        let index_start_seq = self.index_start_seq()?;

        let condition = |event: &Event| match &event.kind {
            brehon_types::EventKind::ReviewRequested { review_id: r, .. } => r == review_id,
            brehon_types::EventKind::ReviewScoreReceived { review_id: r, .. } => r == review_id,
            brehon_types::EventKind::ReviewApproved { review_id: r, .. } => r == review_id,
            brehon_types::EventKind::ReviewRejected { review_id: r, .. } => r == review_id,
            brehon_types::EventKind::ReviewChangesRequested { review_id: r, .. } => r == review_id,
            _ => false,
        };

        let mut filter_without_limit = filter.clone();
        filter_without_limit.limit = None;

        if index_start_seq == 0 {
            return self.scan_all_events_with_condition(&filter_without_limit, &condition);
        }

        let mut events =
            self.scan_all_events_before_seq(&filter_without_limit, index_start_seq, &condition)?;

        if self.has_index_data_for_prefix(&index_prefix)? {
            let mut indexed = self.query_by_index_prefix(
                &index_prefix,
                &filter_without_limit,
                index_start_seq,
                &condition,
            )?;
            events.append(&mut indexed);
        }

        if let Some(limit) = filter.limit {
            events.truncate(limit);
        }

        Ok(events)
    }

    fn query_by_agent(
        &self,
        agent_id: &str,
        filter: &EventFilter,
    ) -> Result<Vec<Event>, StoreError> {
        let index_prefix = agent_index_prefix(agent_id);
        let index_start_seq = self.index_start_seq()?;

        let condition = |event: &Event| match &event.kind {
            brehon_types::EventKind::AgentSpawned { agent_id: a, .. } => a == agent_id,
            brehon_types::EventKind::AgentDied { agent_id: a, .. } => a == agent_id,
            brehon_types::EventKind::TaskAssigned { agent_id: a, .. } => a == agent_id,
            _ => false,
        };

        let mut filter_without_limit = filter.clone();
        filter_without_limit.limit = None;

        if index_start_seq == 0 {
            return self.scan_all_events_with_condition(&filter_without_limit, &condition);
        }

        let mut events =
            self.scan_all_events_before_seq(&filter_without_limit, index_start_seq, &condition)?;

        if self.has_index_data_for_prefix(&index_prefix)? {
            let mut indexed = self.query_by_index_prefix(
                &index_prefix,
                &filter_without_limit,
                index_start_seq,
                &condition,
            )?;
            events.append(&mut indexed);
        }

        if let Some(limit) = filter.limit {
            events.truncate(limit);
        }

        Ok(events)
    }

    fn index_start_seq(&self) -> Result<u64, StoreError> {
        match self.meta.get(KEY_META_INDEX_START_SEQ)? {
            Some(bytes) => {
                let arr: [u8; 8] = bytes
                    .as_ref()
                    .try_into()
                    .map_err(|_| StoreError::Storage("Invalid index watermark format".into()))?;
                Ok(u64::from_be_bytes(arr))
            }
            None => Ok(0),
        }
    }

    fn has_index_data_for_prefix(&self, prefix: &[u8]) -> Result<bool, StoreError> {
        Ok(self
            .events
            .prefix(prefix)
            .next()
            .transpose()
            .map_err(|e| StoreError::Storage(e.to_string()))?
            .is_some())
    }

    fn query_by_index_prefix<F>(
        &self,
        index_prefix: &[u8],
        filter: &EventFilter,
        min_seq: u64,
        condition: &F,
    ) -> Result<Vec<Event>, StoreError>
    where
        F: Fn(&Event) -> bool,
    {
        let mut events = Vec::new();
        let mut count = 0;
        let limit = filter.limit.unwrap_or(usize::MAX);
        let since_ts = filter.since.map(|t| t.timestamp());
        let until_ts = filter.until.map(|t| t.timestamp());

        let iter = self.events.prefix(index_prefix);
        for result in iter {
            if count >= limit {
                break;
            }

            let (index_key, _index_value) =
                result.map_err(|e| StoreError::Storage(e.to_string()))?;
            let Some(seq) = parse_seq_from_index_key(&index_key) else {
                continue;
            };
            if seq < min_seq {
                continue;
            }

            let log_key = crate::keys::log_key(seq);
            let event_value = match self.events.get(&log_key) {
                Ok(Some(value)) => value,
                Ok(None) => continue,
                Err(e) => return Err(StoreError::Storage(e.to_string())),
            };

            let envelope = serde_json::from_slice::<brehon_types::EventEnvelope>(&event_value)
                .map_err(StoreError::from)?;
            let event = envelope.event;

            if let Some(ts) = since_ts {
                if event.timestamp.timestamp() < ts {
                    continue;
                }
            }

            if let Some(ts) = until_ts {
                if event.timestamp.timestamp() > ts {
                    continue;
                }
            }

            if !condition(&event) {
                continue;
            }

            if let Some(kinds) = &filter.kinds {
                let kind_matches = kinds.iter().any(|k| k == &event.kind);
                if !kind_matches {
                    continue;
                }
            }

            events.push(event);
            count += 1;
        }

        Ok(events)
    }

    fn scan_all_events(&self, filter: &EventFilter) -> Result<Vec<Event>, StoreError> {
        let mut events = Vec::new();
        let limit = filter.limit.unwrap_or(usize::MAX);

        let iter = self.events.prefix(b"log:");
        for (count, result) in iter.enumerate() {
            if count >= limit {
                break;
            }

            let (_key, value) = result.map_err(|e| StoreError::Storage(e.to_string()))?;
            let envelope = serde_json::from_slice::<brehon_types::EventEnvelope>(&value)
                .map_err(StoreError::from)?;

            events.push(envelope.event);
        }

        Ok(events)
    }

    fn scan_all_events_with_condition<F>(
        &self,
        filter: &EventFilter,
        condition: &F,
    ) -> Result<Vec<Event>, StoreError>
    where
        F: Fn(&Event) -> bool,
    {
        let mut events = Vec::new();
        let mut count = 0;
        let limit = filter.limit.unwrap_or(usize::MAX);
        let since_ts = filter.since.map(|t| t.timestamp());
        let until_ts = filter.until.map(|t| t.timestamp());

        let iter = self.events.prefix(b"log:");
        for result in iter {
            if count >= limit {
                break;
            }

            let (_key, value) = result.map_err(|e| StoreError::Storage(e.to_string()))?;
            let envelope = serde_json::from_slice::<brehon_types::EventEnvelope>(&value)
                .map_err(StoreError::from)?;

            if let Some(ts) = since_ts {
                if envelope.event.timestamp.timestamp() < ts {
                    continue;
                }
            }

            if let Some(ts) = until_ts {
                if envelope.event.timestamp.timestamp() > ts {
                    continue;
                }
            }

            if !condition(&envelope.event) {
                continue;
            }

            if let Some(kinds) = &filter.kinds {
                let kind_matches = kinds.iter().any(|k| k == &envelope.event.kind);
                if !kind_matches {
                    continue;
                }
            }

            events.push(envelope.event);
            count += 1;
        }

        Ok(events)
    }

    fn scan_all_events_before_seq<F>(
        &self,
        filter: &EventFilter,
        exclusive_max_seq: u64,
        condition: &F,
    ) -> Result<Vec<Event>, StoreError>
    where
        F: Fn(&Event) -> bool,
    {
        let mut events = Vec::new();
        let mut count = 0;
        let limit = filter.limit.unwrap_or(usize::MAX);
        let since_ts = filter.since.map(|t| t.timestamp());
        let until_ts = filter.until.map(|t| t.timestamp());

        let iter = self.events.prefix(b"log:");
        for result in iter {
            if count >= limit {
                break;
            }

            let (key, value) = result.map_err(|e| StoreError::Storage(e.to_string()))?;
            let Some(seq) = parse_seq_from_log_key(&key) else {
                continue;
            };
            if seq >= exclusive_max_seq {
                break;
            }

            let envelope = serde_json::from_slice::<brehon_types::EventEnvelope>(&value)
                .map_err(StoreError::from)?;

            if let Some(ts) = since_ts {
                if envelope.event.timestamp.timestamp() < ts {
                    continue;
                }
            }

            if let Some(ts) = until_ts {
                if envelope.event.timestamp.timestamp() > ts {
                    continue;
                }
            }

            if !condition(&envelope.event) {
                continue;
            }

            if let Some(kinds) = &filter.kinds {
                let kind_matches = kinds.iter().any(|k| k == &envelope.event.kind);
                if !kind_matches {
                    continue;
                }
            }

            events.push(envelope.event);
            count += 1;
        }

        Ok(events)
    }

    fn apply_filters(&self, mut events: Vec<Event>, filter: &EventFilter) -> Vec<Event> {
        if let Some(kinds) = &filter.kinds {
            events.retain(|e| kinds.iter().any(|k| k == &e.kind));
        }

        if let Some(since) = filter.since {
            events.retain(|e| e.timestamp >= since);
        }

        if let Some(until) = filter.until {
            events.retain(|e| e.timestamp <= until);
        }

        events
    }
}

#[cfg(test)]
mod tests {
    use brehon_types::EventFilter;

    #[test]
    fn test_query_construction() {
        let filter = EventFilter::new().aggregate("T001").limit(10);
        assert_eq!(filter.aggregate_id, Some("T001".to_string()));
        assert_eq!(filter.limit, Some(10));
    }
}
