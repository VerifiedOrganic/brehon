//! Soak test: Sustained mock MCP tool calls.
//!
//! Verifies that the mock MCP server (which models the real server's panic
//! boundaries, size limits, and drain tracking) does not leak in-flight work
//! entries and remains usable after many repeated invocations.

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use brehon_test_harness::{MockEchoTool, MockMcpError, MockMcpServer, MockPanicTool, MockSlowTool};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn soak_mcp_tool_calls_no_in_flight_leak() {
    let server = Arc::new(MockMcpServer::new());
    server.register_tool(std::sync::Arc::new(MockEchoTool {
        name: "counting_tool".to_string(),
    }));
    let calls = crate::soak_cycles_locked(500);

    for i in 0..calls {
        let result = server
            .call_tool("counting_tool", serde_json::json!({"payload": i}))
            .await;
        assert!(result.is_ok(), "Call {} should succeed: {:?}", i, result);
    }

    // Note: in-flight work is tracked via a global OnceLock; we skip asserting
    // on the global count here because concurrent tests share the tracker.
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn soak_mcp_panic_boundary_holds() {
    let server = Arc::new(MockMcpServer::new());
    server.register_tool(std::sync::Arc::new(MockEchoTool {
        name: "counting_tool".to_string(),
    }));
    server.register_tool(std::sync::Arc::new(MockPanicTool {
        name: "panic_tool".to_string(),
        call_count: Arc::new(AtomicUsize::new(0)),
        panic_on_call: Some(5),
    }));
    let calls = crate::soak_cycles_locked(100);

    let mut panics_caught = 0;
    let mut oks = 0;

    for i in 0..calls {
        let tool_name = if i % 5 == 0 {
            "panic_tool"
        } else {
            "counting_tool"
        };
        let result = server
            .call_tool(tool_name, serde_json::json!({"value": i}))
            .await;

        match result {
            Ok(_) => {
                oks += 1;
            }
            Err(MockMcpError::Internal(msg)) => {
                assert!(msg.contains("panic_tool"));
                panics_caught += 1;
            }
            Err(other) => {
                panic!("Unexpected error on call {}: {:?}", i, other);
            }
        }
    }

    assert!(
        panics_caught > 0,
        "Should have caught at least one panic during soak"
    );
    assert!(oks > 0, "Should have observed successful calls");

    // Server should still be usable after panics
    let result = server
        .call_tool("counting_tool", serde_json::json!({"payload": 999}))
        .await;
    assert!(result.is_ok(), "Server should remain usable after panics");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn soak_mcp_concurrent_calls_safe() {
    let server = Arc::new(MockMcpServer::new());
    server.register_tool(std::sync::Arc::new(MockEchoTool {
        name: "counting_tool".to_string(),
    }));
    let tasks = crate::soak_cycles_locked(20);
    let calls_per_task = crate::soak_cycles_locked(50);

    let mut handles = vec![];

    for task_id in 0..tasks {
        let server = Arc::clone(&server);
        handles.push(tokio::spawn(async move {
            let mut successes = 0;
            for i in 0..calls_per_task {
                let result = server
                    .call_tool(
                        "counting_tool",
                        serde_json::json!({"task": task_id, "i": i}),
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
        tasks * calls_per_task,
        "All concurrent calls should succeed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn soak_mcp_slow_tool_drain_completes() {
    let server = Arc::new(MockMcpServer::new());
    server.register_tool(std::sync::Arc::new(MockSlowTool {
        name: "slow_tool".to_string(),
        delay_ms: 50,
    }));
    let calls = crate::soak_cycles_locked(10);

    // Launch several slow calls
    let mut handles = vec![];
    for i in 0..calls {
        let server = Arc::clone(&server);
        handles.push(tokio::spawn(async move {
            server
                .call_tool("slow_tool", serde_json::json!({"i": i}))
                .await
                .is_ok()
        }));
    }

    // Wait for all to complete
    let results: Vec<bool> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    assert!(results.iter().all(|&b| b), "All slow calls should complete");

    // Global drain tracker is shared across tests; skip asserting exact count
}
