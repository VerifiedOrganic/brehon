//! Compile a [`SandboxSpec`] into a process-confinement argv prefix (ADR-0010 T12).
//!
//! `SandboxSpec` has been fully serde'd, merged, validated and unit-tested for
//! a long time, and had exactly one non-test consumer: `agent_env.rs` took the
//! profile *name*, exported `BREHON_PERMISSION_PROFILE`, and trusted the vendor
//! CLI to apply the profile to itself. Confinement was an honour system.
//!
//! This is the compiler. It is a pure function — path in, argv out — so the
//! whole policy surface is golden-testable without spawning anything.
//!
//! **Why a prefix rather than `pre_exec`.** `Pty::spawn` goes through
//! `portable_pty::CommandBuilder`, which exposes no `pre_exec` and consumes it
//! internally for setsid/TTY. A prefix is also the shape `sandbox-exec` and
//! `bwrap` already have, and `sandbox-exec` execs in place, so setpgid,
//! `kill_on_drop` and the reaper logic are untouched.
//!
//! **Why no trait.** `SandboxBackend` is already an enum with one
//! implementation per platform. An interface with one implementation per arm
//! is not an abstraction.

use std::path::{Path, PathBuf};

use brehon_types::config::{
    CredentialClass, EnvPolicy, FsRootSpec, NetworkClass, SandboxBackend, SandboxSpec,
};

/// Everything the compiler needs that is not in the spec.
pub struct ConfineCtx<'a> {
    /// The agent's own directory — the one place it may freely write.
    pub work: &'a Path,
    /// Paths that must never fall inside a write grant: canonical's checkout
    /// and the base mirror.
    ///
    /// Explicit rather than derived, because the obvious derivation is wrong.
    /// `<project>/.brehon` IS granted — it is coordination state, the one
    /// deliberate hole ADR-0010 accepts — and it contains `runtime/`, so
    /// "reject any write root containing the runtime directory" would refuse
    /// the policy the ADR prescribes.
    pub protected: &'a [PathBuf],
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfineError {
    /// Landlock has no deny rules, only allow-grants over a default-deny
    /// handled set, so a deny nested inside a grant is *unexpressible* there.
    /// Erroring here beats silently under-enforcing on one platform.
    #[error(
        "denied root '{denied}' is inside writable root '{write}'; Landlock cannot express a nested deny, so this policy would mean different things on macOS and Linux"
    )]
    NestedDeny { denied: String, write: String },

    /// A write grant that contains a read-only root, the runtime directory, or
    /// the project itself is not a sandbox.
    #[error("writable root '{write}' contains '{protected}', which must not be writable")]
    WriteRootTooBroad { write: String, protected: String },

    #[error(
        "sandbox path '{path}' does not exist, so it cannot be canonicalized; a policy naming an unresolvable path fails closed and presents as a broken agent"
    )]
    UnresolvablePath { path: String },

    #[error("confinement is unsupported on this platform; use WSL2 or a container")]
    UnsupportedPlatform,

    #[error("sandbox backend {0} is not available on this host")]
    BackendUnavailable(SandboxBackend),
}

/// What this host can actually enforce, reported *before* a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfineSupport {
    pub backend: Option<SandboxBackend>,
    pub detail: String,
    /// Things the operator must know are NOT enforced here. Surfaced up front
    /// rather than discovered in a postmortem.
    pub unenforceable: Vec<&'static str>,
}

/// Compile `spec` into an argv prefix.
///
/// `Ok(vec![])` **only** when `unsafe_marker` is set. Every other inability to
/// build a boundary is `Err`, and `Err` refuses the spawn.
pub fn confine_argv(spec: &SandboxSpec, ctx: &ConfineCtx) -> Result<Vec<String>, ConfineError> {
    if spec.unsafe_marker {
        return Ok(Vec::new());
    }

    let read = canonicalize_all(&spec.read_roots)?;
    let write = canonicalize_all(&spec.write_roots)?;
    let denied = canonicalize_all(&spec.denied_roots)?;
    let work = canonicalize(ctx.work)?;
    let protected: Vec<PathBuf> = ctx
        .protected
        .iter()
        .map(|path| canonicalize(path).unwrap_or_else(|_| path.clone()))
        .collect();

    check_invariants(&read, &write, &denied, &protected)?;

    match spec.backend {
        // An explicit no-sandbox backend without `unsafe_marker` is a
        // contradiction: the profile asked for confinement and named a backend
        // that provides none.
        SandboxBackend::None => Err(ConfineError::BackendUnavailable(SandboxBackend::None)),
        SandboxBackend::Seatbelt | SandboxBackend::OsDefault if cfg!(target_os = "macos") => {
            Ok(seatbelt_argv(&read, &write, &denied, &work))
        }
        SandboxBackend::Bubblewrap | SandboxBackend::OsDefault if cfg!(target_os = "linux") => {
            Ok(landlock_argv(&read, &write, &work))
        }
        backend => {
            if cfg!(not(any(target_os = "macos", target_os = "linux"))) {
                Err(ConfineError::UnsupportedPlatform)
            } else {
                Err(ConfineError::BackendUnavailable(backend))
            }
        }
    }
}

/// The invariants, enforced rather than discovered.
fn check_invariants(
    _read: &[PathBuf],
    write: &[PathBuf],
    denied: &[PathBuf],
    protected: &[PathBuf],
) -> Result<(), ConfineError> {
    for w in write {
        for d in denied {
            if d.starts_with(w) {
                return Err(ConfineError::NestedDeny {
                    denied: d.display().to_string(),
                    write: w.display().to_string(),
                });
            }
        }
        // A blanket grant swallowing canonical or the base mirror is not a
        // sandbox. A lane destroyed its own test canonical with a blanket
        // `/private/tmp` grant, which is why this is checked rather than
        // trusted.
        //
        // Read roots are deliberately NOT checked: reads are open by design
        // (denying them breaks rustc, cargo, node and every vendor CLI), so a
        // write grant nested under the `/` read root is expected.
        for protected in protected {
            if protected.starts_with(w) {
                return Err(ConfineError::WriteRootTooBroad {
                    write: w.display().to_string(),
                    protected: protected.display().to_string(),
                });
            }
        }
    }
    Ok(())
}

/// `/tmp` vs `/private/tmp` renders a Seatbelt profile inert. It fails closed,
/// so it presents as a broken agent rather than a broken jail — which is why
/// this is an error and not a warning.
fn canonicalize(path: &Path) -> Result<PathBuf, ConfineError> {
    path.canonicalize()
        .map_err(|_| ConfineError::UnresolvablePath {
            path: path.display().to_string(),
        })
}

fn canonicalize_all(roots: &[FsRootSpec]) -> Result<Vec<PathBuf>, ConfineError> {
    roots
        .iter()
        .map(|root| canonicalize(Path::new(&root.path)))
        .collect()
}

/// SBPL is **last-match-wins**, so deny-before-allow is silently defeated.
/// Emission order is read → write → denied, and it is golden-tested.
fn seatbelt_argv(
    read: &[PathBuf],
    write: &[PathBuf],
    denied: &[PathBuf],
    work: &Path,
) -> Vec<String> {
    let mut profile = String::from("(version 1)\n(deny default)\n");
    profile.push_str("(allow process-exec)\n(allow process-fork)\n(allow sysctl-read)\n");
    // A confined agent killed a process outside its sandbox: `pkill -f node`
    // is ordinary dev-server cleanup and it ends a 72-hour run. NOT
    // `(target pgrp)`, which reopens the hole whenever the victim shares a
    // process group.
    profile.push_str("(deny signal)\n(allow signal (target self) (target children))\n");

    for path in read {
        profile.push_str(&format!(
            "(allow file-read* (subpath {}))\n",
            sbpl_str(path)
        ));
    }
    for path in write {
        profile.push_str(&format!(
            "(allow file-read* file-write* (subpath {}))\n",
            sbpl_str(path)
        ));
    }
    for path in denied {
        profile.push_str(&format!(
            "(deny file-read* file-write* (subpath {}))\n",
            sbpl_str(path)
        ));
    }
    // `(allow default)` would leave AF_UNIX connect open, and a tmux server is
    // routinely running: `tmux send-keys` executes in an unsandboxed pane.
    profile.push_str("(deny network-outbound (regex #\"^/private/tmp/tmux-\"))\n");
    profile.push_str("(deny network-outbound (regex #\"^/var/run/docker\\\\.sock\"))\n");

    let _ = work;
    // Ends at `--`; the caller appends the real command.
    vec![
        "sandbox-exec".to_string(),
        "-p".to_string(),
        profile,
        "--".to_string(),
    ]
}

/// Re-enter our own binary, which applies a Landlock ruleset and execs.
///
/// Leaf-only cache grants are *mandatory* here: "grant `~/.cargo`, deny
/// `~/.cargo/bin`" is exactly the nested deny Landlock cannot represent, so
/// without it macOS and Linux silently disagree on the rule that matters most.
fn landlock_argv(read: &[PathBuf], write: &[PathBuf], work: &Path) -> Vec<String> {
    let mut argv = vec![
        std::env::current_exe()
            .map(|exe| exe.display().to_string())
            .unwrap_or_else(|_| "brehon".to_string()),
        "confine".to_string(),
    ];
    for path in read {
        argv.push("--read".to_string());
        argv.push(path.display().to_string());
    }
    for path in write {
        argv.push("--write".to_string());
        argv.push(path.display().to_string());
    }
    let _ = work;
    argv.push("--".to_string());
    argv
}

/// SBPL string literal. Injection-safe: a policy-closing payload in a path
/// cannot terminate the literal.
fn sbpl_str(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
}

/// The roots an agent actually needs, resolved for this host.
///
/// `profiles.specs` is config-shaped but empty in practice — it is validated
/// and never populated — and it could not carry these anyway: every path here
/// is resolved at spawn from `$HOME` and the agent's own directory.
///
/// **Reads are open.** Denying them breaks rustc, cargo, node and every vendor
/// CLI. ADR-0010 accepts this: the agent can always *learn* canonical's path,
/// and per-agent clones plus these write grants make the knowledge inert.
///
/// **Writes are leaves, never parents.** Granting `~/.cargo` is a full escape
/// — `~/.cargo/bin` is on the operator's PATH and was verified receiving an
/// executable the operator's own shell then resolved, and
/// `~/.rustup/toolchains/*/bin` holds `cargo` and `clippy-driver`. Only the
/// cache directories are granted.
///
/// `<project>/.brehon` is the one deliberate hole: `BREHON_ROOT` points there
/// and the MCP server is a child of the agent CLI, so it inherits this
/// profile. Coordination state, not source or history.
pub fn default_spec(work: &Path, brehon_root: &Path, unsafe_profile: bool) -> SandboxSpec {
    if unsafe_profile {
        return SandboxSpec {
            backend: SandboxBackend::None,
            read_roots: Vec::new(),
            write_roots: Vec::new(),
            denied_roots: Vec::new(),
            network_class: NetworkClass::Unrestricted,
            credential_class: CredentialClass::Unrestricted,
            env_policy: EnvPolicy::Inherit,
            unsafe_marker: true,
        };
    }

    let mut write_roots = vec![fs_root(work), fs_root(brehon_root)];
    if let Some(home) = dirs::home_dir() {
        for leaf in [
            ".cargo/registry",
            ".cargo/git",
            ".rustup/downloads",
            ".rustup/tmp",
            ".npm/_cacache",
        ] {
            let path = home.join(leaf);
            if path.exists() {
                write_roots.push(fs_root(&path));
            }
        }
    }

    SandboxSpec {
        backend: SandboxBackend::OsDefault,
        read_roots: vec![FsRootSpec {
            path: "/".to_string(),
            recursive: true,
        }],
        write_roots,
        denied_roots: Vec::new(),
        network_class: NetworkClass::Allowlisted,
        credential_class: CredentialClass::EnvAllowlist,
        env_policy: EnvPolicy::Minimal,
        unsafe_marker: false,
    }
}

fn fs_root(path: &Path) -> FsRootSpec {
    FsRootSpec {
        path: path.display().to_string(),
        recursive: true,
    }
}

/// Environment the confined process needs alongside the argv prefix.
///
/// A per-agent TMPDIR under the work root, never the shared `/tmp`: a
/// writable shared temp directory is a cross-agent channel, and `git
/// init/add/commit` needs no tmp grant at all. Note macOS `mktemp -t` reads
/// `_CS_DARWIN_USER_TEMP_DIR` and ignores this, so nothing Brehon owns may
/// live in the shared directory regardless.
///
/// Separate from [`confine_argv`] because it is env, not argv — folding it
/// into the prefix would put a directory path where the command belongs.
pub fn confine_env(ctx: &ConfineCtx) -> Vec<(String, String)> {
    let tmp = ctx.work.join(".brehon-tmp");
    let tmp = tmp.display().to_string();
    ["TMPDIR", "TEMP", "TMP"]
        .into_iter()
        .map(|key| (key.to_string(), tmp.clone()))
        .collect()
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
            backend: Some(SandboxBackend::Bubblewrap),
            detail: "Landlock".to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn root(path: &Path) -> FsRootSpec {
        FsRootSpec {
            path: path.display().to_string(),
            recursive: true,
        }
    }

    fn spec(backend: SandboxBackend) -> SandboxSpec {
        SandboxSpec {
            backend,
            read_roots: Vec::new(),
            write_roots: Vec::new(),
            denied_roots: Vec::new(),
            network_class: NetworkClass::Denied,
            credential_class: CredentialClass::None,
            env_policy: EnvPolicy::Minimal,
            unsafe_marker: false,
        }
    }

    /// `work` plus a canonical checkout standing in for what must stay
    /// unwritable.
    fn dirs() -> (tempfile::TempDir, PathBuf, Vec<PathBuf>) {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        let canonical = temp.path().join("canonical");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&canonical).unwrap();
        (temp, work, vec![canonical])
    }

    /// The fail-open case, and the only one.
    #[test]
    fn unsafe_marker_is_the_only_empty_prefix() {
        let (_t, work, protected) = dirs();
        let mut s = spec(SandboxBackend::None);
        s.unsafe_marker = true;
        let argv = confine_argv(
            &s,
            &ConfineCtx {
                work: &work,
                protected: &protected,
            },
        )
        .unwrap();
        assert!(argv.is_empty(), "unsafe_marker means no prefix");
    }

    /// A backend that provides no confinement, without the explicit marker, is
    /// a contradiction — and must fail closed rather than silently spawn bare.
    #[test]
    fn backend_none_without_the_marker_refuses() {
        let (_t, work, protected) = dirs();
        let err = confine_argv(
            &spec(SandboxBackend::None),
            &ConfineCtx {
                work: &work,
                protected: &protected,
            },
        )
        .expect_err("no backend and no marker must refuse the spawn");
        assert_eq!(err, ConfineError::BackendUnavailable(SandboxBackend::None));
    }

    #[test]
    fn a_deny_nested_in_a_write_grant_is_refused() {
        let (_t, work, protected) = dirs();
        let nested = work.join("secrets");
        std::fs::create_dir_all(&nested).unwrap();
        let mut s = spec(SandboxBackend::OsDefault);
        s.write_roots = vec![root(&work)];
        s.denied_roots = vec![root(&nested)];

        let err = confine_argv(
            &s,
            &ConfineCtx {
                work: &work,
                protected: &protected,
            },
        )
        .expect_err("Landlock cannot express a nested deny");
        assert!(matches!(err, ConfineError::NestedDeny { .. }));
    }

    #[test]
    fn a_write_root_swallowing_canonical_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("everything");
        let canonical = work.join("canonical");
        std::fs::create_dir_all(&canonical).unwrap();
        let protected = vec![canonical];
        let mut s = spec(SandboxBackend::OsDefault);
        s.write_roots = vec![root(&work)];

        let err = confine_argv(
            &s,
            &ConfineCtx {
                work: &work,
                protected: &protected,
            },
        )
        .expect_err("a write grant must not swallow canonical");
        assert!(matches!(err, ConfineError::WriteRootTooBroad { .. }));
    }

    #[test]
    fn an_unresolvable_path_refuses_rather_than_rendering_the_policy_inert() {
        let (_t, work, protected) = dirs();
        let mut s = spec(SandboxBackend::OsDefault);
        s.read_roots = vec![FsRootSpec {
            path: "/definitely/not/here".to_string(),
            recursive: true,
        }];

        let err = confine_argv(
            &s,
            &ConfineCtx {
                work: &work,
                protected: &protected,
            },
        )
        .expect_err("an unresolvable path must not be emitted");
        assert!(matches!(err, ConfineError::UnresolvablePath { .. }));
    }

    /// SBPL is last-match-wins, so this ordering is load-bearing, not cosmetic.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_emits_read_then_write_then_denied() {
        let (_t, work, protected) = dirs();
        let readable = work.join("ro");
        let writable = work.join("rw");
        let secret = work.join("secret");
        for dir in [&readable, &writable, &secret] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let mut s = spec(SandboxBackend::Seatbelt);
        s.read_roots = vec![root(&readable)];
        s.write_roots = vec![root(&writable)];
        s.denied_roots = vec![root(&secret)];

        let argv = confine_argv(
            &s,
            &ConfineCtx {
                work: &work,
                protected: &protected,
            },
        )
        .unwrap();

        assert_eq!(argv[0], "sandbox-exec");
        assert_eq!(
            argv.last().map(String::as_str),
            Some("--"),
            "the prefix must end at `--` so the caller appends the command"
        );
        let profile = &argv[2];
        let read_at = profile.find("file-read* (subpath").unwrap();
        let write_at = profile.find("file-read* file-write*").unwrap();
        let deny_at = profile.find("(deny file-read* file-write*").unwrap();
        assert!(
            read_at < write_at && write_at < deny_at,
            "SBPL is last-match-wins; deny must be emitted last:\n{profile}"
        );
        assert!(profile.contains("(deny signal)"));
        assert!(
            profile.contains("(allow signal (target self) (target children))"),
            "a confined agent must not be able to signal outside its sandbox"
        );
        assert!(
            !profile.contains("(target pgrp)"),
            "target pgrp reopens the hole whenever the victim shares a process group"
        );
        assert!(
            profile.contains("tmux-"),
            "AF_UNIX connect to tmux stays denied"
        );
    }

    /// Canonicalization is what stops /tmp vs /private/tmp rendering the
    /// profile inert.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_emits_canonical_paths() {
        let (_t, work, protected) = dirs();
        let mut s = spec(SandboxBackend::Seatbelt);
        s.write_roots = vec![root(&work)];

        let argv = confine_argv(
            &s,
            &ConfineCtx {
                work: &work,
                protected: &protected,
            },
        )
        .unwrap();

        let canonical = work.canonicalize().unwrap();
        assert!(
            argv[2].contains(canonical.display().to_string().as_str()),
            "the emitted profile must name the resolved path"
        );
    }

    #[test]
    fn probe_names_what_it_cannot_enforce() {
        let support = probe();
        assert!(
            !support.unenforceable.is_empty(),
            "the operator must see the gaps before a run, not after an incident"
        );
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            assert!(support.backend.is_some());
        } else {
            assert!(support.backend.is_none());
        }
    }

    /// The one test that proves the compiled policy is a boundary rather than
    /// a string. Everything else here is golden-testing text.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_confined_process_cannot_write_outside_its_work_root() {
        let (_t, work, protected) = dirs();
        let outside = _t.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();

        let mut s = spec(SandboxBackend::Seatbelt);
        s.read_roots = vec![FsRootSpec {
            path: "/".to_string(),
            recursive: true,
        }];
        s.write_roots = vec![root(&work)];

        let argv = confine_argv(
            &s,
            &ConfineCtx {
                work: &work,
                protected: &protected,
            },
        )
        .unwrap();

        let run = |target: &Path| {
            std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .arg("/usr/bin/touch")
                .arg(target)
                .status()
                .expect("sandbox-exec")
                .success()
        };

        let inside = work.canonicalize().unwrap().join("inside.txt");
        assert!(run(&inside), "a write inside the work root must succeed");
        assert!(inside.exists());

        let escape = outside.canonicalize().unwrap().join("escape.txt");
        assert!(!run(&escape), "a write outside the work root must fail");
        assert!(!escape.exists(), "confinement is not advisory");
    }

    #[test]
    fn tmpdir_is_per_agent_and_never_the_shared_one() {
        let (_t, work, protected) = dirs();
        let env = confine_env(&ConfineCtx {
            work: &work,
            protected: &protected,
        });
        assert_eq!(env.len(), 3);
        for (key, value) in env {
            assert!(["TMPDIR", "TEMP", "TMP"].contains(&key.as_str()));
            assert!(
                value.starts_with(work.display().to_string().as_str()),
                "a shared temp dir is a cross-agent channel: {value}"
            );
        }
    }

    /// Each of these is a reproduced escape, not a preference.
    #[test]
    fn the_default_spec_grants_cache_leaves_never_parents() {
        let (_t, work, _protected) = dirs();
        let brehon_root = work.join(".brehon");
        std::fs::create_dir_all(&brehon_root).unwrap();

        let spec = default_spec(&work, &brehon_root, false);

        assert!(!spec.unsafe_marker);
        let writes: Vec<&str> = spec.write_roots.iter().map(|r| r.path.as_str()).collect();
        for granted in &writes {
            assert!(
                !granted.ends_with("/.cargo") && !granted.ends_with("/.rustup"),
                "granting a cache parent is a full escape: ~/.cargo/bin is on the \
                 operator's PATH — got {granted}"
            );
        }
        assert!(
            writes.iter().any(|w| w.ends_with(".brehon")),
            "the .brehon grant is deliberate: the MCP server inherits this profile"
        );
        assert!(
            writes.iter().any(|w| *w == work.display().to_string()),
            "the agent must be able to write its own directory"
        );
        assert_eq!(
            spec.read_roots.len(),
            1,
            "reads are open: denying them breaks rustc, cargo, node and every vendor CLI"
        );
    }

    /// The unsafe profile is the only path to no confinement, and it says so.
    #[test]
    fn the_unsafe_default_spec_is_explicitly_marked() {
        let (_t, work, _protected) = dirs();
        let spec = default_spec(&work, &work.join(".brehon"), true);
        assert!(spec.unsafe_marker);
        assert_eq!(spec.backend, SandboxBackend::None);
    }

    /// The default spec must survive its own invariants — a default that
    /// cannot compile would fail closed on every spawn.
    #[test]
    fn the_default_spec_compiles() {
        let (_t, work, protected) = dirs();
        let brehon_root = work.join(".brehon");
        std::fs::create_dir_all(&brehon_root).unwrap();

        let spec = default_spec(&work, &brehon_root, false);
        let argv = confine_argv(
            &spec,
            &ConfineCtx {
                work: &work,
                protected: &protected,
            },
        )
        .expect("the shipped default must compile on a supported host");
        assert!(!argv.is_empty());
    }
}
