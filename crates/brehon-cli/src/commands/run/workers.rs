use std::sync::Arc;

use brehon_types::{BrehonConfig, WorkerAssignmentMode};
use anyhow::Result;

pub(crate) fn resolve_worker_pool_counts(
    config: &BrehonConfig,
    workers_override: Option<&str>,
) -> Result<Vec<u32>> {
    if config.roles.workers.is_empty() {
        return Ok(Vec::new());
    }

    let requested_count = match workers_override {
        Some(raw) => Some(raw.parse::<u32>().map_err(|_| {
            anyhow::anyhow!("Invalid --workers value '{raw}'. Expected a positive integer.")
        })?),
        None => config.orchestration.spawn_workers,
    };

    if let Some(worker_count) = requested_count {
        if worker_count == 0 {
            return Err(anyhow::anyhow!(
                "Invalid worker count 0. Use a positive value or omit the override."
            ));
        }

        let generic_capacity: u32 = config
            .roles
            .workers
            .iter()
            .filter(|pool| pool.assignment_mode != WorkerAssignmentMode::Reserved)
            .map(|pool| pool.max)
            .sum();
        if worker_count > generic_capacity {
            return Err(anyhow::anyhow!(
                "Requested worker count {} exceeds generic worker pool capacity {}.",
                worker_count,
                generic_capacity
            ));
        }

        let mut counts: Vec<u32> = config
            .roles
            .workers
            .iter()
            .map(|pool| {
                if pool.assignment_mode == WorkerAssignmentMode::Reserved {
                    pool.min
                } else {
                    0
                }
            })
            .collect();
        let mut remaining = worker_count;
        while remaining > 0 {
            let mut assigned_this_pass = false;
            for (idx, pool) in config.roles.workers.iter().enumerate() {
                if remaining == 0 {
                    break;
                }
                if pool.assignment_mode == WorkerAssignmentMode::Reserved {
                    continue;
                }
                if counts[idx] < pool.max {
                    counts[idx] += 1;
                    remaining -= 1;
                    assigned_this_pass = true;
                }
            }

            if !assigned_this_pass {
                return Err(anyhow::anyhow!(
                    "Requested worker count {} could not be allocated across worker pools.",
                    worker_count
                ));
            }
        }

        Ok(counts)
    } else {
        Ok(config.roles.workers.iter().map(|pool| pool.min).collect())
    }
}

pub(crate) fn push_runtime_dashboard_event(
    dashboard_data: &Arc<std::sync::Mutex<brehon_tui::DashboardData>>,
    description: impl Into<String>,
) {
    let mut dashboard = dashboard_data.lock().unwrap();
    dashboard.events.push(brehon_tui::EventInfo {
        timestamp: chrono::Local::now().format("%H:%M").to_string(),
        description: description.into(),
    });
    const MAX_EVENTS: usize = 50;
    if dashboard.events.len() > MAX_EVENTS {
        let drop_count = dashboard.events.len() - MAX_EVENTS;
        dashboard.events.drain(0..drop_count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_worker_pool_counts_defaults_to_pool_minimums() {
        let config = brehon_config::parse_defaults().unwrap();
        assert_eq!(
            resolve_worker_pool_counts(&config, None).unwrap(),
            vec![1, 1]
        );
    }

    #[test]
    fn test_resolve_worker_pool_counts_uses_override() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.orchestration.spawn_workers = Some(4);
        assert_eq!(
            resolve_worker_pool_counts(&config, None).unwrap(),
            vec![4, 1]
        );
    }

    #[test]
    fn test_resolve_worker_pool_counts_rejects_zero() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.orchestration.spawn_workers = Some(0);
        let err = resolve_worker_pool_counts(&config, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("worker count 0"));
    }

    #[test]
    fn test_resolve_worker_pool_counts_rejects_count_above_capacity() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.orchestration.spawn_workers = Some(6);
        let err = resolve_worker_pool_counts(&config, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("exceeds generic worker pool capacity"));
    }

    #[test]
    fn test_resolve_worker_pool_counts_cli_override_takes_precedence() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.orchestration.spawn_workers = Some(2);
        assert_eq!(
            resolve_worker_pool_counts(&config, Some("4")).unwrap(),
            vec![4, 1]
        );
    }

    #[test]
    fn test_resolve_worker_pool_counts_rejects_invalid_cli_override() {
        let config = brehon_config::parse_defaults().unwrap();
        let err = resolve_worker_pool_counts(&config, Some("abc"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Invalid --workers value"));
    }
}
