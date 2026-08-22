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

pub use brehon_types::confine::{ConfineSupport, probe};

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
            // Seatbelt expresses a nested deny (last-match-wins); Landlock
            // cannot. Refusing here on a Landlock host is the difference
            // between the two platforms meaning the same thing and only
            // appearing to.
            if cfg!(not(target_os = "macos")) && d.starts_with(w) {
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
    // `(allow default)`, NOT `(deny default)`.
    //
    // This is the base docs/research/containment-2026-08.md specifies, and the
    // reason is not stylistic. Under `(deny default)` every unlisted operation
    // class is denied — including `network-outbound` and `mach-lookup` — so a
    // confined agent cannot reach its model API, resolve DNS, or open a PTY
    // ioctl. Verified: `curl https://1.1.1.1/` fails and node's `fetch` gives
    // ENOTFOUND. The boundary this file exists to build is a *filesystem write*
    // boundary; denying everything else breaks the agent without adding it.
    //
    // Reads stay open by design: denying them breaks rustc, cargo, node and
    // every vendor CLI, and ADR-0010 accepts that explicitly.
    let mut profile = String::from("(version 1)\n(allow default)\n");

    // Deny all writes, then allow back. SBPL is last-match-wins, so this
    // ordering is the policy: a blanket deny first, the grants after it.
    profile.push_str("(deny file-write*)\n");

    // Devices are not the boundary. `/dev/null`, the tty and the dtrace helper
    // are needed by essentially every program — `git --version` alone fails
    // with "could not open '/dev/null'" without this — and granting them
    // gives away nothing a filesystem write boundary is protecting.
    profile.push_str("(allow file-write* (subpath \"/dev\"))\n");

    // A confined agent killed a process outside its sandbox: `pkill -f node`
    // is ordinary dev-server cleanup and it ends a 72-hour run. NOT
    // `(target pgrp)`, which reopens the hole whenever the victim shares a
    // process group.
    profile.push_str("(deny signal)\n(allow signal (target self) (target children))\n");

    for path in write {
        profile.push_str(&format!(
            "(allow file-write* (subpath {}))\n",
            sbpl_str(path)
        ));
    }
    // Denies last so they win over any grant above them.
    for path in denied {
        profile.push_str(&format!(
            "(deny file-read* file-write* (subpath {}))\n",
            sbpl_str(path)
        ));
    }
    // `(allow default)` leaves AF_UNIX connect open, and a tmux server is
    // routinely running: `tmux send-keys` executes in an unsandboxed pane.
    // These lines are only meaningful on this base.
    profile.push_str("(deny network-outbound (regex #\"^/private/tmp/tmux-\"))\n");
    profile.push_str("(deny network-outbound (regex #\"^/var/run/docker\\\\.sock\"))\n");

    let _ = read;
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
pub fn default_spec(
    work: &Path,
    brehon_root: &Path,
    worktrees_root: Option<&Path>,
    unsafe_profile: bool,
) -> SandboxSpec {
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

    // Only roots that exist. `canonicalize` refuses an unresolvable path — it
    // has to, or `/tmp` vs `/private/tmp` renders the whole profile inert —
    // and granting a directory that is not there grants nothing anyway. This
    // is the same rule already applied to the cache leaves below.
    let mut write_roots = vec![fs_root(work)];
    if brehon_root.exists() {
        write_roots.push(fs_root(brehon_root));
    }
    // Brehon's own MCP server runs as a child of the agent CLI and inherits
    // this profile, so anything Brehon writes on the agent's behalf has to be
    // grantable. ADR-0010 already accepts this for `<project>/.brehon`; the
    // worktree root is the same category — epic and initiative integration
    // worktrees and review preflight worktrees all live under it.
    //
    // `base.git` is carved back out below: it is the one thing under this root
    // that must stay agent-read-only, because that ownership split is what
    // makes refreshing it safe.
    let mut denied_roots = Vec::new();
    if let Some(worktrees_root) = worktrees_root.filter(|root| root.exists()) {
        {
            write_roots.push(fs_root(worktrees_root));
            let base = worktrees_root.join("base.git");
            if base.exists() {
                denied_roots.push(fs_root(&base));
            }
        }
    }
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
        denied_roots,
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
    let tmp = ctx.work.join(".brehon-tmp").display().to_string();
    let mut env: Vec<(String, String)> = ["TMPDIR", "TEMP", "TMP"]
        .into_iter()
        .map(|key| (key.to_string(), tmp.clone()))
        .collect();

    // Redirect the vendor CLIs' own config directories per-agent. This removes
    // a class rather than defending against it: `~/.claude/settings.json`
    // carries a live `hooks` key that Claude Code executes, so a shared config
    // directory is arbitrary code execution in the operator's own session the
    // moment one agent writes to it. One env var each.
    env.push((
        "CLAUDE_CONFIG_DIR".to_string(),
        ctx.work
            .join(".brehon-agent-home/claude")
            .display()
            .to_string(),
    ));
    env.push((
        "CODEX_HOME".to_string(),
        ctx.work
            .join(".brehon-agent-home/codex")
            .display()
            .to_string(),
    ));
    env
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

    /// Seatbelt is last-match-wins so a nested deny is expressible; Landlock
    /// has no deny rules at all. The platforms must not silently disagree.
    #[test]
    fn a_deny_nested_in_a_write_grant() {
        let (_t, work, protected) = dirs();
        let nested = work.join("secrets");
        std::fs::create_dir_all(&nested).unwrap();
        let mut s = spec(SandboxBackend::OsDefault);
        s.write_roots = vec![root(&work)];
        s.denied_roots = vec![root(&nested)];

        let result = confine_argv(
            &s,
            &ConfineCtx {
                work: &work,
                protected: &protected,
            },
        );

        if cfg!(target_os = "macos") {
            let argv = result.expect("Seatbelt expresses a nested deny");
            let profile = &argv[2];
            let grant = profile.find("(allow file-write* (subpath").unwrap();
            let deny = profile
                .find("(deny file-read* file-write* (subpath")
                .unwrap();
            assert!(grant < deny, "last-match-wins: the deny must come after");
        } else {
            assert!(
                matches!(result, Err(ConfineError::NestedDeny { .. })),
                "Landlock cannot express this, so it must refuse rather than under-enforce"
            );
        }
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
    /// The base is `(allow default)` + `(deny file-write*)` + grants, and the
    /// ordering carries the policy because SBPL is last-match-wins.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_denies_writes_then_grants_them_back() {
        let (_t, work, protected) = dirs();
        let writable = work.join("rw");
        let secret = work.join("secret");
        for dir in [&writable, &secret] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let mut s = spec(SandboxBackend::Seatbelt);
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
        let profile = &argv[2];

        assert!(
            profile.starts_with("(version 1)\n(allow default)\n"),
            "a `(deny default)` base denies network and mach-lookup, which \
             leaves the agent unable to reach its model API:\n{profile}"
        );
        let blanket = profile.find("(deny file-write*)").unwrap();
        let grant = profile.find("(allow file-write* (subpath").unwrap();
        let deny = profile
            .find("(deny file-read* file-write* (subpath")
            .unwrap();
        assert!(
            blanket < grant && grant < deny,
            "deny-all-writes, grant back, then explicit denies:\n{profile}"
        );
        assert!(profile.contains("(deny signal)"));
        assert!(!profile.contains("(target pgrp)"));
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
        for (key, value) in &env {
            assert!(
                value.starts_with(work.display().to_string().as_str()),
                "{key} must live under the agent's own directory, not a shared one: {value}"
            );
        }
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        for required in ["TMPDIR", "TEMP", "TMP"] {
            assert!(keys.contains(&required), "missing {required}");
        }
    }

    /// `~/.claude/settings.json` has a live `hooks` key that Claude Code
    /// executes, so a shared vendor config directory is arbitrary code
    /// execution in the operator's own session. Redirecting removes the class.
    #[test]
    fn vendor_config_dirs_are_redirected_per_agent() {
        let (_t, work, protected) = dirs();
        let env = confine_env(&ConfineCtx {
            work: &work,
            protected: &protected,
        });

        for key in ["CLAUDE_CONFIG_DIR", "CODEX_HOME"] {
            let value = env
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
                .unwrap_or_else(|| panic!("{key} must be redirected"));
            assert!(
                value.starts_with(work.display().to_string().as_str()),
                "{key} must not point at the operator's home: {value}"
            );
        }
    }

    /// Each of these is a reproduced escape, not a preference.
    #[test]
    fn the_default_spec_grants_cache_leaves_never_parents() {
        let (_t, work, _protected) = dirs();
        let brehon_root = work.join(".brehon");
        std::fs::create_dir_all(&brehon_root).unwrap();

        let spec = default_spec(&work, &brehon_root, None, false);

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
        let spec = default_spec(&work, &work.join(".brehon"), None, true);
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

        let spec = default_spec(&work, &brehon_root, None, false);
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

    /// A grant for a directory that does not exist grants nothing, and
    /// `canonicalize` would refuse it — which would fail every spawn closed
    /// before `.brehon` had been created.
    #[test]
    fn the_default_spec_skips_roots_that_do_not_exist() {
        let (_t, work, protected) = dirs();
        let absent = work.join("not-created-yet");

        let spec = default_spec(&work, &absent, None, false);

        assert!(
            !spec
                .write_roots
                .iter()
                .any(|r| r.path == absent.display().to_string()),
            "an absent root must not be granted"
        );
        confine_argv(
            &spec,
            &ConfineCtx {
                work: &work,
                protected: &protected,
            },
        )
        .expect("the spec must still compile");
    }

    /// The write boundary must hold while the agent can still do its job.
    /// The previous version of this file only ran `/usr/bin/touch`, so it
    /// passed over a profile that denied every network syscall — a confined
    /// agent could not have reached its model API at all.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_confined_process_keeps_network_and_exec() {
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

        // A dynamically-linked binary that needs dyld, mach-lookup and a
        // working TMPDIR — the shape of every vendor CLI.
        let version = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .args(["/usr/bin/git", "--version"])
            .output()
            .expect("sandbox-exec");
        assert!(
            version.status.success(),
            "a confined agent must still be able to exec: {}",
            String::from_utf8_lossy(&version.stderr)
        );

        // Outbound TCP, without DNS so the test does not depend on a resolver.
        let net = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .args(["/usr/bin/nc", "-z", "-G", "2", "1.1.1.1", "443"])
            .output()
            .expect("sandbox-exec");
        assert!(
            net.status.success(),
            "a confined agent must still reach its model API: {}",
            String::from_utf8_lossy(&net.stderr)
        );
    }
}
