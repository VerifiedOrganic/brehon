#![cfg(unix)]

use std::borrow::Cow;
use std::env;
use std::error::Error;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use brehon_mux::{
    AgentAdapter, CustomAgentConfig, HarnessCapabilities, HarnessControlPlane, HarnessTransport,
    Mux, MuxConfig,
};
use portable_pty::{native_pty_system, Child, CommandBuilder, PtySize};

const HELPER_TEST_NAME: &str = "__brehon_tui_fullscreen_helper";
const HELPER_ENV: &str = "BREHON_TUI_FULLSCREEN_HELPER";
const TMP_ENV: &str = "BREHON_TUI_FULLSCREEN_TMP";
const SUPERVISOR_INPUT_LOG: &str = "supervisor-input.bin";
const WORKER_INPUT_LOG: &str = "worker-input.bin";
const SUPERVISOR_ID: &str = "test-supervisor";
const WORKER_ID: &str = "test-worker";
const READY_TEXT: &[u8] = b"SUPERVISOR_CHILD_READY";
const WORKER_READY_TEXT: &[u8] = b"WORKER_CHILD_READY";
const WORKER_INPUT_MARKER: &[u8] = b"worker fullscreen smoke\n";
const CTRL_F: &[u8] = b"\x06";
const CTRL_Q: &[u8] = b"\x11";
const CTRL_R: &[u8] = b"\x12";
const CTRL_S: &[u8] = b"\x13";
const CTRL_W: &[u8] = b"\x17";
const SGR_WHEEL_UP: &[u8] = b"\x1b[<64;10;10M";
const ENTER_ALT_SCREEN: &[u8] = b"\x1b[?1049h";
const LEAVE_ALT_SCREEN: &[u8] = b"\x1b[?1049l";
const LEAVE_ALT_SCREEN_1047: &[u8] = b"\x1b[?1047l";
const LEAVE_ALT_SCREEN_47: &[u8] = b"\x1b[?47l";
const MOUSE_ENABLE_SEQUENCES: &[&[u8]] = &[
    b"\x1b[?1000h",
    b"\x1b[?1002h",
    b"\x1b[?1003h",
    b"\x1b[?1006h",
];

// This integration test owns a real PTY and process-global terminal modes.
static SERIAL_PTY_TEST: Mutex<()> = Mutex::new(());

#[test]
fn panesmith_fullscreen_attach_detach_repeats_and_ctrl_q_exits() -> Result<(), Box<dyn Error>> {
    let _serial = SERIAL_PTY_TEST
        .lock()
        .expect("serial PTY test lock poisoned");
    let temp = tempfile::tempdir()?;
    let supervisor_input_log = temp.path().join(SUPERVISOR_INPUT_LOG);
    let worker_input_log = temp.path().join(WORKER_INPUT_LOG);
    let output = Arc::new(Mutex::new(Vec::new()));

    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 40,
        cols: 140,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut command = CommandBuilder::new(env::current_exe()?);
    command.args(["--exact", HELPER_TEST_NAME, "--nocapture"]);
    command.env(HELPER_ENV, "1");
    command.env(TMP_ENV, temp.path().as_os_str());
    command.env("TERM", "xterm-256color");
    command.env("NO_COLOR", "1");
    command.cwd(env!("CARGO_MANIFEST_DIR"));

    let mut child = pair.slave.spawn_command(command)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let reader_output = Arc::clone(&output);
    let _reader_thread = thread::spawn(move || {
        let mut buf = [0_u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => reader_output.lock().unwrap().extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
    });

    wait_for_contains(&output, READY_TEXT, Duration::from_secs(10))?;
    wait_for_contains(&output, SUPERVISOR_ID.as_bytes(), Duration::from_secs(10))?;

    write_input(&mut writer, CTRL_W)?;
    wait_for_contains(&output, WORKER_READY_TEXT, Duration::from_secs(10))?;
    wait_for_contains(&output, WORKER_ID.as_bytes(), Duration::from_secs(10))?;

    let worker_enter_offset = output_len(&output);
    write_input(&mut writer, CTRL_F)?;
    wait_for_any_after(
        &output,
        worker_enter_offset,
        MOUSE_ENABLE_SEQUENCES,
        Duration::from_secs(5),
        "mouse capture enable for worker attach",
    )?;
    write_input(&mut writer, WORKER_INPUT_MARKER)?;
    write_input(&mut writer, SGR_WHEEL_UP)?;
    thread::sleep(Duration::from_millis(150));

    let worker_detach_offset = output_len(&output);
    write_input(&mut writer, CTRL_F)?;
    wait_for_contains_after(
        &output,
        worker_detach_offset,
        ENTER_ALT_SCREEN,
        Duration::from_secs(5),
        "dashboard restore after worker detach",
    )?;

    let worker_reset_offset = output_len(&output);
    write_input(&mut writer, CTRL_R)?;
    wait_for_contains_after(
        &output,
        worker_reset_offset,
        WORKER_READY_TEXT,
        Duration::from_secs(5),
        "worker Panesmith reset respawn",
    )?;

    write_input(&mut writer, CTRL_S)?;
    wait_for_contains(&output, SUPERVISOR_ID.as_bytes(), Duration::from_secs(10))?;

    for cycle in 1..=2 {
        let enter_offset = output_len(&output);
        write_input(&mut writer, CTRL_F)?;

        wait_for_any_after(
            &output,
            enter_offset,
            MOUSE_ENABLE_SEQUENCES,
            Duration::from_secs(5),
            &format!("mouse capture enable for attach cycle {cycle}"),
        )?;

        let before_detach_offset = output_len(&output);
        let fullscreen_output = output_slice(&output, enter_offset, before_detach_offset);
        assert!(
            !contains_any(
                &fullscreen_output,
                &[LEAVE_ALT_SCREEN, LEAVE_ALT_SCREEN_1047, LEAVE_ALT_SCREEN_47]
            ),
            "fullscreen attach cycle {cycle} left the alternate screen before detach; \
             this would expose parent terminal scrollback\n{}",
            tail_string(&fullscreen_output)
        );

        write_input(&mut writer, SGR_WHEEL_UP)?;
        thread::sleep(Duration::from_millis(150));

        let detach_offset = output_len(&output);
        write_input(&mut writer, CTRL_F)?;
        wait_for_contains_after(
            &output,
            detach_offset,
            ENTER_ALT_SCREEN,
            Duration::from_secs(5),
            &format!("dashboard restore after detach cycle {cycle}"),
        )?;
    }

    write_input(&mut writer, CTRL_Q)?;
    let status = wait_for_child_exit(&mut *child, Duration::from_secs(8), &output)?;
    assert!(
        status.success(),
        "helper test process exited unsuccessfully: {:?}\n{}",
        status,
        output_tail(&output)
    );

    let child_input = std::fs::read(&supervisor_input_log).unwrap_or_default();
    assert!(
        !child_input
            .windows(CTRL_F.len())
            .any(|window| window == CTRL_F),
        "Ctrl-f detach chord was forwarded into the attached child PTY: {:?}",
        child_input
    );
    assert!(
        !child_input
            .windows(SGR_WHEEL_UP.len())
            .any(|window| window == SGR_WHEEL_UP),
        "fullscreen mouse wheel event was forwarded into the attached child PTY: {:?}",
        child_input
    );
    assert!(
        !child_input
            .windows(CTRL_Q.len())
            .any(|window| window == CTRL_Q),
        "Ctrl-q was forwarded into the child PTY after fullscreen detach: {:?}",
        child_input
    );

    let worker_input = std::fs::read(&worker_input_log).unwrap_or_default();
    assert!(
        worker_input
            .windows(WORKER_INPUT_MARKER.len())
            .any(|window| window == WORKER_INPUT_MARKER),
        "fullscreen input did not reach the worker PTY session: {:?}",
        worker_input
    );
    assert!(
        !worker_input
            .windows(CTRL_F.len())
            .any(|window| window == CTRL_F),
        "Ctrl-f detach chord was forwarded into the worker PTY: {:?}",
        worker_input
    );
    assert!(
        !worker_input
            .windows(SGR_WHEEL_UP.len())
            .any(|window| window == SGR_WHEEL_UP),
        "fullscreen mouse wheel event was forwarded into the worker PTY: {:?}",
        worker_input
    );
    assert!(
        !worker_input
            .windows(CTRL_Q.len())
            .any(|window| window == CTRL_Q),
        "Ctrl-q was forwarded into the worker PTY after fullscreen detach: {:?}",
        worker_input
    );

    Ok(())
}

#[test]
fn __brehon_tui_fullscreen_helper() -> Result<(), Box<dyn Error>> {
    if env::var_os(HELPER_ENV).is_none() {
        return Ok(());
    }

    println!("PARENT_SCROLLBACK_SENTINEL_BEFORE_TUI");

    let tmp = env::var_os(TMP_ENV)
        .map(PathBuf::from)
        .expect("helper temp directory env must be set");
    let supervisor_input_log = tmp.join(SUPERVISOR_INPUT_LOG);
    let worker_input_log = tmp.join(WORKER_INPUT_LOG);
    let supervisor_script = format!(
        "stty raw -echo || true; printf 'SUPERVISOR_CHILD_READY\\r\\n'; exec cat > {}",
        shell_single_quote(&supervisor_input_log.to_string_lossy())
    );
    let worker_script = format!(
        "stty raw -echo || true; printf 'WORKER_CHILD_READY\\r\\n'; exec cat >> {}",
        shell_single_quote(&worker_input_log.to_string_lossy())
    );
    let mut mux = Mux::factory(MuxConfig {
        cwd: tmp.clone(),
        session_name: Some("fullscreen-pty-regression".to_string()),
        workers: 1,
        worker_names: vec![WORKER_ID.to_string()],
        supervisor_name: SUPERVISOR_ID.to_string(),
        supervisor_cli: custom_interactive_agent(
            "fullscreen-test-agent",
            "sh",
            &["-c", supervisor_script.as_str()],
        ),
        worker_cli: custom_interactive_agent(
            "fullscreen-worker-test-agent",
            "sh",
            &["-c", worker_script.as_str()],
        ),
        include_director: false,
        rows: 32,
        cols: 120,
        ..Default::default()
    })?;

    assert!(
        mux.is_panesmith_managed(SUPERVISOR_ID),
        "helper mux supervisor must be Panesmith-managed"
    );
    assert!(
        mux.is_panesmith_managed(WORKER_ID),
        "helper mux worker must be Panesmith-managed"
    );
    assert!(mux.focus(SUPERVISOR_ID));

    let runtime = tokio::runtime::Runtime::new()?;
    let shutdown = Arc::new(AtomicBool::new(false));
    brehon_tui::run_tui(shutdown, mux, runtime.handle().clone())?;
    Ok(())
}

fn custom_interactive_agent(name: &str, command: &str, args: &[&str]) -> AgentAdapter {
    AgentAdapter::Custom(CustomAgentConfig {
        name: name.to_string(),
        command: Some(command.to_string()),
        args: args.iter().map(|arg| arg.to_string()).collect(),
        base_url: None,
        api_key_env: None,
        headers: Vec::new(),
        capabilities: HarnessCapabilities {
            supports_hooks: false,
            supports_subagents: false,
            supports_textbox_submit: true,
            supports_teams: false,
            one_shot: false,
            uses_ink_prompt: false,
            prompt_injection_strategy: brehon_mux::PromptInjectionStrategy::ImmediateSubmit,
            tool_prefix: Cow::Borrowed("mcp_brehon_"),
            transport: HarnessTransport::InteractivePty,
            preferred_control_plane: HarnessControlPlane::PtyInjection,
        },
    })
}

fn write_input(writer: &mut Box<dyn Write + Send>, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    writer.write_all(bytes)?;
    writer.flush()?;
    thread::sleep(Duration::from_millis(75));
    Ok(())
}

fn wait_for_contains(
    output: &Arc<Mutex<Vec<u8>>>,
    needle: &[u8],
    timeout: Duration,
) -> Result<(), Box<dyn Error>> {
    wait_for_contains_after(output, 0, needle, timeout, &format!("{needle:?}"))
}

fn wait_for_contains_after(
    output: &Arc<Mutex<Vec<u8>>>,
    offset: usize,
    needle: &[u8],
    timeout: Duration,
    description: &str,
) -> Result<(), Box<dyn Error>> {
    wait_for_condition(timeout, || output_contains_after(output, offset, needle))
        .then_some(())
        .ok_or_else(|| {
            format!(
                "timed out waiting for {description}; output tail:\n{}",
                output_tail(output)
            )
            .into()
        })
}

fn wait_for_any_after(
    output: &Arc<Mutex<Vec<u8>>>,
    offset: usize,
    needles: &[&[u8]],
    timeout: Duration,
    description: &str,
) -> Result<(), Box<dyn Error>> {
    wait_for_condition(timeout, || {
        needles
            .iter()
            .any(|needle| output_contains_after(output, offset, needle))
    })
    .then_some(())
    .ok_or_else(|| {
        format!(
            "timed out waiting for {description}; output tail:\n{}",
            output_tail(output)
        )
        .into()
    })
}

fn wait_for_child_exit(
    child: &mut dyn Child,
    timeout: Duration,
    output: &Arc<Mutex<Vec<u8>>>,
) -> Result<portable_pty::ExitStatus, Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "timed out waiting for helper process to exit after Ctrl-q\n{}",
                output_tail(output)
            )
            .into());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_condition(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    predicate()
}

fn output_contains_after(output: &Arc<Mutex<Vec<u8>>>, offset: usize, needle: &[u8]) -> bool {
    let bytes = output.lock().unwrap();
    bytes
        .get(offset.min(bytes.len())..)
        .is_some_and(|haystack| {
            haystack
                .windows(needle.len())
                .any(|window| window == needle)
        })
}

fn output_len(output: &Arc<Mutex<Vec<u8>>>) -> usize {
    output.lock().unwrap().len()
}

fn output_slice(output: &Arc<Mutex<Vec<u8>>>, start: usize, end: usize) -> Vec<u8> {
    let bytes = output.lock().unwrap();
    let start = start.min(bytes.len());
    let end = end.min(bytes.len());
    bytes[start..end].to_vec()
}

fn contains_any(haystack: &[u8], needles: &[&[u8]]) -> bool {
    needles.iter().any(|needle| {
        haystack
            .windows(needle.len())
            .any(|window| window == *needle)
    })
}

fn output_tail(output: &Arc<Mutex<Vec<u8>>>) -> String {
    let bytes = output.lock().unwrap();
    tail_string(&bytes)
}

fn tail_string(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(5000);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
