//! What a host can actually enforce (ADR-0010 T16).
//!
//! Lives here rather than in brehon-pty because `brehon doctor` reports it
//! and may only depend on brehon-types and brehon-config — and a duplicated
//! copy would let the report drift from the thing that enforces.

use crate::config::SandboxBackend;

/// What this host can actually enforce, reported *before* a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfineSupport {
    pub backend: Option<SandboxBackend>,
    pub detail: String,
    /// Things the operator must know are NOT enforced here. Surfaced up front
    /// rather than discovered in a postmortem.
    pub unenforceable: Vec<&'static str>,
}

/// What this host can enforce, for `brehon doctor` and pre-run reporting.
pub fn probe() -> ConfineSupport {
    if cfg!(target_os = "macos") {
        ConfineSupport {
            backend: Some(SandboxBackend::Seatbelt),
            detail: "sandbox-exec (Seatbelt)".to_string(),
            unenforceable: vec![
                "setuid binaries cannot exec under any profile: ps, top, sudo, su and traceroute fail; pgrep, pkill, lsof and netstat work",
                "filter.<name>.* and merge.<name>.driver are unkillable by flags",
                "docker run -v writes as the Docker VM, so confinement is decorative for a lane that needs the socket",
                "<project>/.brehon is granted: coordination state shared across agents",
            ],
        }
    } else if cfg!(target_os = "linux") {
        ConfineSupport {
            // Deliberately None: T14 was never built, and reporting a backend
            // that does not exist reads as coverage while providing none.
            backend: None,
            detail: "Landlock (NOT IMPLEMENTED — T14 is unbuilt, so spawning fails closed)"
                .to_string(),
            unenforceable: vec![
                "Landlock never restricts chmod, chown, utime or setxattr",
                "signal scoping needs ABI v6 (kernel 6.12); below that a confined agent can signal outside its sandbox",
                "unix-socket connect needs ABI v9 (Linux 7.1); no shipping LTS has it",
                "filter.<name>.* and merge.<name>.driver are unkillable by flags",
                "<project>/.brehon is granted: coordination state shared across agents",
            ],
        }
    } else {
        ConfineSupport {
            backend: None,
            detail: "no supported confinement backend; use WSL2 or a container".to_string(),
            unenforceable: vec!["any path confinement"],
        }
    }
}
