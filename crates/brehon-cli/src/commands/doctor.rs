use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use brehon_types::{agent::AdapterKind, BrehonConfig};

/// How long a single endpoint probe may take before it is reported unreachable.
/// Kept short so `brehon doctor` stays responsive even when a local server is
/// down or wedged.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

pub fn execute(project_path: Option<&Path>, repair: bool, json: bool) -> Result<()> {
    let brehon_root = project_path
        .map(|p| p.join(".brehon"))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default().join(".brehon"));

    if repair {
        let report = brehon_doctor::run_doctor_repair(&brehon_root);
        if json {
            println!("{}", report.to_json()?);
        } else {
            println!("{}", report);
        }
        return Ok(());
    }

    let (report_text, has_errors) = brehon_doctor::run_doctor_cli(&brehon_root);

    println!("{}", report_text);

    // Advisory: probe any configured local OpenAI-compatible endpoints so the
    // operator learns "server down / wrong port / which model" before a run.
    // Never affects the exit status — a local server being offline is an
    // operator condition, not a corrupt-state error.
    if let Some(section) = probe_local_endpoints_off_runtime(project_path) {
        println!("\n{section}");
    }

    if has_errors {
        std::process::exit(1);
    }

    Ok(())
}

/// One endpoint to probe (deduplicated by `base_url`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProbeTarget {
    launcher: String,
    base_url: String,
}

/// Outcome of probing a single endpoint.
#[derive(Debug)]
enum ProbeOutcome {
    Reachable { models: Vec<String> },
    Unreachable { detail: String },
}

/// Run the blocking endpoint probe on a dedicated thread so it never executes
/// `reqwest::blocking` inside the async `#[tokio::main]` runtime, which would
/// panic. The probe is pure I/O with no shared state, so a plain thread is safe.
fn probe_local_endpoints_off_runtime(project_path: Option<&Path>) -> Option<String> {
    let owned = project_path.map(Path::to_path_buf);
    std::thread::spawn(move || local_endpoint_report(owned.as_deref()))
        .join()
        .ok()
        .flatten()
}

/// Build the advisory "Local endpoints" report, or `None` when no launcher
/// declares a local OpenAI-compatible `base_url`.
fn local_endpoint_report(project_path: Option<&Path>) -> Option<String> {
    // Use the diagnostics loader so a fatal config issue does not abort the
    // probe; if the config cannot be loaded at all, simply skip the section.
    let config = brehon_config::load_config_for_diagnostics(project_path)
        .ok()
        .map(|(config, _warnings)| config)?;

    let targets = select_probe_targets(&config);
    if targets.is_empty() {
        return None;
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .ok()?;

    let mut lines = vec!["Local endpoints".to_string()];
    for target in targets {
        let line = match probe_endpoint(&client, &target.base_url) {
            ProbeOutcome::Reachable { models } => {
                let models = if models.is_empty() {
                    "(no models advertised)".to_string()
                } else {
                    models.join(", ")
                };
                format!(
                    "  [ok] {} ({}) - {}",
                    target.launcher, target.base_url, models
                )
            }
            ProbeOutcome::Unreachable { detail } => format!(
                "  [fail] {} ({}) - unreachable: {detail}",
                target.launcher, target.base_url
            ),
        };
        lines.push(line);
    }
    Some(lines.join("\n"))
}

/// Select launchers that point at a local OpenAI-compatible endpoint, one entry
/// per distinct `base_url` (lanes commonly share one endpoint).
fn select_probe_targets(config: &BrehonConfig) -> Vec<ProbeTarget> {
    let mut targets: Vec<ProbeTarget> = config
        .launchers
        .iter()
        .filter(|(_, launcher)| {
            matches!(
                launcher.adapter,
                AdapterKind::OpenAiCompatible | AdapterKind::NativeAgent
            )
        })
        .filter_map(|(name, launcher)| {
            launcher
                .base_url
                .as_ref()
                .filter(|base_url| is_loopback_http_url(base_url))
                .map(|base_url| ProbeTarget {
                    launcher: name.clone(),
                    base_url: base_url.clone(),
                })
        })
        .collect();
    // Deterministic order, then probe each endpoint once even if several lanes
    // share it.
    targets.sort_by(|a, b| {
        a.base_url
            .cmp(&b.base_url)
            .then_with(|| a.launcher.cmp(&b.launcher))
    });
    targets.dedup_by(|a, b| a.base_url == b.base_url);
    targets
}

fn is_loopback_http_url(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url.trim()) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|addr| addr.is_loopback())
}

/// Probe `{base_url}/models` and return the advertised model ids.
fn probe_endpoint(client: &reqwest::blocking::Client, base_url: &str) -> ProbeOutcome {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    match client.get(&url).send() {
        Ok(response) if response.status().is_success() => {
            match response.json::<serde_json::Value>() {
                Ok(body) => ProbeOutcome::Reachable {
                    models: parse_model_ids(&body),
                },
                Err(err) => ProbeOutcome::Unreachable {
                    detail: format!("invalid /models response: {err}"),
                },
            }
        }
        Ok(response) => ProbeOutcome::Unreachable {
            detail: format!("HTTP {}", response.status()),
        },
        Err(err) => ProbeOutcome::Unreachable {
            detail: err.to_string(),
        },
    }
}

/// Extract model ids from an OpenAI-style `/models` response body.
fn parse_model_ids(body: &serde_json::Value) -> Vec<String> {
    body.get("data")
        .and_then(serde_json::Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|model| {
                    model
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn parse_model_ids_extracts_data_ids() {
        let body = serde_json::json!({
            "object": "list",
            "data": [{"id": "qwen3-coder-30b"}, {"id": "devstral-24b"}]
        });
        assert_eq!(
            parse_model_ids(&body),
            vec!["qwen3-coder-30b", "devstral-24b"]
        );
    }

    #[test]
    fn parse_model_ids_is_empty_when_data_absent_or_malformed() {
        assert!(parse_model_ids(&serde_json::json!({})).is_empty());
        assert!(parse_model_ids(&serde_json::json!({"data": "nope"})).is_empty());
        assert!(parse_model_ids(&serde_json::json!({"data": [{"name": "x"}]})).is_empty());
    }

    #[test]
    fn loopback_url_detection_accepts_only_local_http_endpoints() {
        assert!(is_loopback_http_url("http://127.0.0.1:8080/v1"));
        assert!(is_loopback_http_url("https://localhost:8080/v1"));
        assert!(is_loopback_http_url("http://[::1]:8080/v1"));
        assert!(!is_loopback_http_url("http://192.168.1.10:8080/v1"));
        assert!(!is_loopback_http_url("https://api.openai.com/v1"));
        assert!(!is_loopback_http_url("file:///tmp/socket"));
        assert!(!is_loopback_http_url("not a url"));
    }

    #[test]
    fn probe_endpoint_reports_models_from_a_live_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let body = r#"{"data":[{"id":"local-model"}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        let client = reqwest::blocking::Client::builder()
            .timeout(PROBE_TIMEOUT)
            .build()
            .unwrap();
        let outcome = probe_endpoint(&client, &format!("http://{addr}/v1"));
        server.join().unwrap();

        match outcome {
            ProbeOutcome::Reachable { models } => assert_eq!(models, vec!["local-model"]),
            ProbeOutcome::Unreachable { detail } => panic!("expected reachable, got {detail}"),
        }
    }

    #[test]
    fn probe_endpoint_reports_unreachable_when_nothing_listens() {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        // Port 1 is reserved and never listening in test environments.
        let outcome = probe_endpoint(&client, "http://127.0.0.1:1/v1");
        assert!(matches!(outcome, ProbeOutcome::Unreachable { .. }));
    }
}
