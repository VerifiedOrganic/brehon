//! Chaos test: Mock MCP server under random failures.
//!
//! Tests panic isolation, request-size enforcement, and concurrent safety
//! when delays, drops, and oversized payloads are injected.

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;

use brehon_test_harness::{
    ChaosConfig, ChaosInjector, MockEchoTool, MockMcpError, MockMcpServer, MockPanicTool,
};

fn chaos_server() -> MockMcpServer {
    let server = MockMcpServer::new();
    server.register_tool(std::sync::Arc::new(MockEchoTool {
        name: "delayed_echo".to_string(),
    }));
    server.register_tool(std::sync::Arc::new(MockPanicTool {
        name: "chaos_panic".to_string(),
        call_count: Arc::new(AtomicUsize::new(0)),
        panic_on_call: Some(2),
    }));
    server
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn chaos_mcp_concurrent_calls_with_delays() {
    let server = Arc::new(chaos_server());
    let config = ChaosConfig::with_delays(Duration::from_millis(0), Duration::from_millis(10));
    let task_count = 20;
    let calls_per_task = 25;

    let mut handles = vec![];

    for task_id in 0..task_count {
        let server = Arc::clone(&server);
        let mut injector = ChaosInjector::new(config.clone());

        handles.push(tokio::spawn(async move {
            let mut successes = 0;
            for i in 0..calls_per_task {
                injector.delay().await;

                let result = server
                    .call_tool(
                        "delayed_echo",
                        serde_json::json!({"payload": format!("task-{}-call-{}", task_id, i)}),
                    )
                    .await;

                if result.is_ok() {
                    successes += 1;
                }
            }
            successes
        }));
    }

    let results: Vec<_> = futures::future::join_all(handles).await;
    let total_successes: usize = results.into_iter().map(|r| r.unwrap()).sum();

    assert_eq!(
        total_successes,
        task_count * calls_per_task,
        "All delayed_echo calls should succeed despite chaos delays"
    );

    // Note: global drain tracker is shared across concurrent tests
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn chaos_mcp_panic_under_load() {
    let server = Arc::new(chaos_server());
    let task_count = 10;
    let calls_per_task = 20;
    let mut panics = 0;
    let mut oks = 0;

    let mut handles = vec![];

    for task_id in 0..task_count {
        let server = Arc::clone(&server);
        handles.push(tokio::spawn(async move {
            let mut task_panics = 0;
            let mut task_oks = 0;
            for i in 0..calls_per_task {
                let result = server
                    .call_tool("chaos_panic", serde_json::json!({"task": task_id, "i": i}))
                    .await;
                match result {
                    Ok(_) => task_oks += 1,
                    Err(MockMcpError::Internal(_)) => task_panics += 1,
                    Err(other) => panic!("Unexpected error: {:?}", other),
                }
            }
            (task_panics, task_oks)
        }));
    }

    let results: Vec<_> = futures::future::join_all(handles).await;
    for (p, o) in results.into_iter().map(|r| r.unwrap()) {
        panics += p;
        oks += o;
    }

    assert!(panics > 0, "Should have observed panics");
    assert!(oks > 0, "Should have observed successful calls");

    // Server remains operational
    let result = server
        .call_tool("delayed_echo", serde_json::json!({"payload": "final"}))
        .await;
    assert!(result.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chaos_mcp_oversized_payload_rejected_under_load() {
    let server = Arc::new(MockMcpServer::new().with_max_payload_bytes(64));
    server.register_tool(std::sync::Arc::new(MockEchoTool {
        name: "delayed_echo".to_string(),
    }));

    let task_count = 10;
    let calls_per_task = 20;
    let mut rejections = 0;
    let mut accepts = 0;

    let mut handles = vec![];

    for _task_id in 0..task_count {
        let server = Arc::clone(&server);
        handles.push(tokio::spawn(async move {
            let mut task_rejections = 0;
            let mut task_accepts = 0;
            for i in 0..calls_per_task {
                let payload = if i % 3 == 0 {
                    "x".repeat(256) // oversized
                } else {
                    "ok".to_string() // within limit
                };

                let result = server
                    .call_tool("delayed_echo", serde_json::json!({"payload": payload}))
                    .await;

                match result {
                    Ok(_) => task_accepts += 1,
                    Err(MockMcpError::OversizedPayload { .. }) => task_rejections += 1,
                    Err(other) => panic!("Unexpected error: {:?}", other),
                }
            }
            (task_rejections, task_accepts)
        }));
    }

    let results: Vec<_> = futures::future::join_all(handles).await;
    for (r, a) in results.into_iter().map(|r| r.unwrap()) {
        rejections += r;
        accepts += a;
    }

    assert!(rejections > 0, "Should have rejected oversized payloads");
    assert!(accepts > 0, "Should have accepted small payloads");
}
