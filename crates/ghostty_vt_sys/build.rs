//! Build script for ghostty_vt_sys
//!
//! This script:
//! 1. Locates the Zig compiler
//! 2. Builds libghostty-vt using Zig
//! 3. Links the resulting static library

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ZigVersion {
    major: u64,
    minor: u64,
    patch: u64,
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Find workspace root (two levels up from crates/ghostty_vt_sys)
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("Could not find workspace root");

    let ghostty_dir = resolve_ghostty_dir(workspace_root);

    // Find Zig compiler
    let zig = find_zig(workspace_root).expect(
        "Compatible Zig compiler not found.\n\
         ghostty_vt_sys currently requires Zig >= 0.15.2 and < 0.16.0.\n\
         Set ZIG to a compatible binary or place one at .context/zig/zig.",
    );
    let zig_version = zig_version(&zig).unwrap_or_else(|| {
        panic!(
            "Failed to determine Zig version for {}.\n\
             ghostty_vt_sys currently requires Zig >= 0.15.2 and < 0.16.0.",
            zig.display()
        )
    });
    assert_supported_zig_version(&zig, zig_version);

    // Rebuild triggers
    println!("cargo:rerun-if-changed=zig/build.zig");
    println!("cargo:rerun-if-changed=zig/build.zig.zon");
    println!("cargo:rerun-if-changed=zig/lib.zig");
    println!("cargo:rerun-if-changed=include/ghostty_vt.h");
    println!(
        "cargo:rerun-if-changed={}",
        ghostty_dir.join("build.zig.zon").display()
    );

    // Build with Zig
    let zig_out = out_dir.join("zig-out");
    let zig_local_cache = out_dir.join("zig-local-cache");
    let zig_global_cache = out_dir.join("zig-global-cache");
    fs::create_dir_all(&zig_local_cache).expect("Failed to create Zig local cache dir");
    fs::create_dir_all(&zig_global_cache).expect("Failed to create Zig global cache dir");

    let mut cmd = Command::new(&zig);
    cmd.current_dir(manifest_dir.join("zig"))
        .env("ZIG_LOCAL_CACHE_DIR", &zig_local_cache)
        .env("ZIG_GLOBAL_CACHE_DIR", &zig_global_cache)
        .arg("build")
        .arg("-Doptimize=ReleaseFast")
        .arg("--prefix")
        .arg(&zig_out);

    // Pass cross-compilation target to Zig when the Cargo target differs from host
    if let Ok(target) = env::var("TARGET") {
        if let Some(zig_target) = rust_target_to_zig(&target) {
            cmd.arg(format!("-Dtarget={zig_target}"));
        }
    }

    let status = cmd.status().expect("Failed to execute zig build");

    if !status.success() {
        panic!("Zig build failed with status: {status}");
    }

    // Link the static library
    let lib_dir = zig_out.join("lib");
    let archive_path = lib_dir.join("libghostty_vt.a");

    // Zig 0.15.2 produces misaligned mach-o archives on macOS, which ld rejects:
    //   "64-bit mach-o member 'libghostty_vt_zcu.o' not 8-byte aligned"
    // Re-pack with the system ar to restore correct alignment.
    #[cfg(target_os = "macos")]
    fix_archive_alignment(&archive_path);

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=ghostty_vt");

    // Link C standard library
    println!("cargo:rustc-link-lib=c");
}

/// Re-pack a static archive with the system `ar` to ensure 8-byte alignment
/// of mach-o objects. This works around a Zig 0.15.2 bug on macOS.
#[cfg(target_os = "macos")]
fn fix_archive_alignment(archive_path: &Path) {
    if !archive_path.exists() {
        panic!("Archive not found at {}", archive_path.display());
    }

    let ar = find_ar();

    // Create a unique temp directory under /tmp using a timestamp suffix.
    // We loop without a pre-check to avoid a TOCTOU race: if `create_dir`
    // returns AlreadyExists we generate a new name and retry.
    let temp_dir = {
        let base = std::env::temp_dir();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut dir = base.join(format!(
            "ghostty_vt_sys_archive_fix_{}_{}",
            std::process::id(),
            now
        ));
        let mut collision = 0u32;
        loop {
            match fs::create_dir(&dir) {
                Ok(()) => break dir,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    collision += 1;
                    dir = base.join(format!(
                        "ghostty_vt_sys_archive_fix_{}_{}_{}",
                        std::process::id(),
                        now,
                        collision
                    ));
                }
                Err(e) => panic!("Failed to create temp dir for archive fix: {e}"),
            }
        }
    };

    // Extract existing objects
    let extract_output = Command::new(&ar)
        .current_dir(&temp_dir)
        .arg("-x")
        .arg(archive_path)
        .output()
        .expect("Failed to run `ar -x`");
    if !extract_output.status.success() {
        let stderr = String::from_utf8_lossy(&extract_output.stderr);
        panic!("`ar -x` failed for {}: {}", archive_path.display(), stderr);
    }

    // Collect object files (skip non-.o files like __.SYMDEF)
    let mut objects: Vec<PathBuf> = fs::read_dir(&temp_dir)
        .expect("Failed to read temp dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "o"))
        .collect();
    objects.sort();

    // Zig-produced archives store objects with restrictive permissions;
    // ensure ar can read them for re-packing.
    for obj in &objects {
        let mut perms = fs::metadata(obj)
            .expect("Failed to read object metadata")
            .permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(perms.mode() | 0o644);
        }
        #[cfg(not(unix))]
        perms.set_readonly(false);
        fs::set_permissions(obj, perms).expect("Failed to chmod object file");
    }

    if objects.is_empty() {
        panic!("No .o files found in archive {}", archive_path.display());
    }

    // Re-pack to a temporary archive in the same directory as the original,
    // then atomically replace the original. Keeping the temp archive on the
    // same filesystem guarantees that fs::rename is atomic and cannot fail
    // with EXDEV.
    let temp_archive = archive_path.with_extension("a.tmp");
    let repack_output = Command::new(&ar)
        .current_dir(&temp_dir)
        .arg("-rcs")
        .arg(&temp_archive)
        .args(&objects)
        .output()
        .expect("Failed to run `ar -rcs`");
    if !repack_output.status.success() {
        let stderr = String::from_utf8_lossy(&repack_output.stderr);
        let _ = fs::remove_file(&temp_archive);
        panic!(
            "`ar -rcs` failed for {}: {}",
            archive_path.display(),
            stderr
        );
    }

    fs::rename(&temp_archive, archive_path).unwrap_or_else(|e| {
        let _ = fs::remove_file(&temp_archive);
        panic!(
            "Failed to replace {} with repacked archive: {}",
            archive_path.display(),
            e
        )
    });

    // Best-effort cleanup of temp directory
    let _ = fs::remove_dir_all(&temp_dir);
}

/// Locate the system `ar` binary on macOS.
///
/// 1. `/usr/bin/ar` — the Apple-provided tool on macOS.
/// 2. `xcrun --find ar` — resolves via the Xcode toolchain.
/// 3. `"ar"` — bare command name, resolved via PATH as a last resort.
#[cfg(target_os = "macos")]
fn find_ar() -> PathBuf {
    let system_ar = PathBuf::from("/usr/bin/ar");
    if system_ar.exists() {
        return system_ar;
    }

    if let Ok(output) = Command::new("xcrun").args(["--find", "ar"]).output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return PathBuf::from(path);
            }
        }
    }

    PathBuf::from("ar")
}

fn resolve_ghostty_dir(workspace_root: &Path) -> PathBuf {
    let primary = workspace_root.join("vendor/ghostty");
    if ghostty_is_initialized(&primary) {
        return primary;
    }

    if let Some(shared_root) = find_shared_repo_root(workspace_root) {
        let fallback = shared_root.join("vendor/ghostty");
        if ghostty_is_initialized(&fallback) {
            println!(
                "cargo:warning=Using shared vendored Ghostty from {} because {} is not initialized",
                fallback.display(),
                primary.display()
            );
            return fallback;
        }
    }

    if !primary.exists() {
        panic!(
            "\n\nGhostty submodule not found at {}\n\n\
             This is required to build ghostty_vt_sys.\n\n\
             To fix, run:\n\
             \n\
             git submodule update --init --recursive\n\n",
            primary.display()
        );
    }

    panic!(
        "\n\nGhostty submodule exists but is not initialized at {}\n\n\
         The vendor/ghostty directory exists but appears to be empty.\n\
         This commonly happens in git worktrees where submodules\n\
         were not properly initialized.\n\n\
         To fix, run:\n\
         \n\
         git submodule update --init --recursive\n\n",
        primary.display()
    );
}

fn ghostty_is_initialized(path: &Path) -> bool {
    path.join("build.zig.zon").exists()
}

fn find_shared_repo_root(workspace_root: &Path) -> Option<PathBuf> {
    let git_path = workspace_root.join(".git");
    if git_path.is_dir() {
        return Some(workspace_root.to_path_buf());
    }

    let gitdir_line = std::fs::read_to_string(&git_path).ok()?;
    let gitdir = gitdir_line
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)?;
    let gitdir_path = absolutize_path(workspace_root, Path::new(gitdir));

    let commondir_path = gitdir_path.join("commondir");
    let commondir = std::fs::read_to_string(commondir_path).ok()?;
    let common_git_dir = absolutize_path(&gitdir_path, Path::new(commondir.trim()));

    common_git_dir.parent().map(Path::to_path_buf)
}

fn absolutize_path(base: &Path, candidate: &Path) -> PathBuf {
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base.join(candidate)
    }
}

/// Map Rust target triple to Zig target triple for cross-compilation.
/// Returns None for native builds (no -Dtarget needed).
fn rust_target_to_zig(rust_target: &str) -> Option<String> {
    // Only pass -Dtarget when cross-compiling (target != host)
    let host = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        return None;
    };

    let host_os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        return None;
    };

    // Parse the Rust target triple: arch-vendor-os[-env]
    let parts: Vec<&str> = rust_target.split('-').collect();
    if parts.len() < 3 {
        return None;
    }

    let target_arch = parts[0];
    let target_os_part = if parts.len() >= 4 { parts[2] } else { parts[1] };

    // Check if this is a native build
    let is_native = target_arch == host
        && ((host_os == "darwin" && target_os_part == "apple")
            || (host_os == "linux" && target_os_part == "linux"));

    if is_native {
        return None;
    }

    // Map to Zig target
    match rust_target {
        "x86_64-unknown-linux-gnu" => Some("x86_64-linux-gnu".to_string()),
        "aarch64-unknown-linux-gnu" => Some("aarch64-linux-gnu".to_string()),
        "x86_64-unknown-linux-musl" => Some("x86_64-linux-musl".to_string()),
        "aarch64-unknown-linux-musl" => Some("aarch64-linux-musl".to_string()),
        "x86_64-apple-darwin" => Some("x86_64-macos".to_string()),
        "aarch64-apple-darwin" => Some("aarch64-macos".to_string()),
        _ => {
            eprintln!(
                "cargo:warning=Unknown target triple for Zig mapping: {rust_target}, \
                 building for host"
            );
            None
        }
    }
}

/// Find the Zig compiler
fn find_zig(workspace_root: &Path) -> Option<PathBuf> {
    // 1. Check ZIG environment variable
    if let Ok(zig) = env::var("ZIG") {
        let path = PathBuf::from(zig);
        if path.exists() {
            return Some(path);
        }
    }

    let mut fallback = None;
    for candidate in zig_candidates(workspace_root) {
        if !candidate.exists() {
            continue;
        }

        if fallback.is_none() {
            fallback = Some(candidate.clone());
        }

        if zig_version(&candidate).is_some_and(is_supported_zig_version) {
            return Some(candidate);
        }
    }

    fallback
}

fn zig_candidates(workspace_root: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![workspace_root.join(".context/zig/zig")];

    // Check system PATH (for mise, homebrew, etc.)
    if let Ok(output) = Command::new("which").arg("zig").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                candidates.push(PathBuf::from(path));
            }
        }
    }

    candidates.extend(homebrew_zig_candidates());
    candidates
}

fn homebrew_zig_candidates() -> Vec<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/opt/homebrew/opt/zig@0.15/bin/zig"),
        PathBuf::from("/usr/local/opt/zig@0.15/bin/zig"),
    ];

    for cellar in [
        "/opt/homebrew/Cellar/zig@0.15",
        "/usr/local/Cellar/zig@0.15",
    ] {
        let Ok(entries) = fs::read_dir(cellar) else {
            continue;
        };

        for entry in entries.flatten() {
            candidates.push(entry.path().join("bin/zig"));
        }
    }

    candidates
}

fn zig_version(zig: &Path) -> Option<ZigVersion> {
    let output = Command::new(zig).arg("version").output().ok()?;
    if !output.status.success() {
        return None;
    }

    parse_zig_version(String::from_utf8_lossy(&output.stdout).trim())
}

fn parse_zig_version(version: &str) -> Option<ZigVersion> {
    let version = version.split(['-', '+']).next().unwrap_or(version);
    let mut parts = version.split('.');

    Some(ZigVersion {
        major: parts.next()?.parse().ok()?,
        minor: parts.next()?.parse().ok()?,
        patch: parts.next()?.parse().ok()?,
    })
}

fn is_supported_zig_version(version: ZigVersion) -> bool {
    let min = ZigVersion {
        major: 0,
        minor: 15,
        patch: 2,
    };
    let max = ZigVersion {
        major: 0,
        minor: 16,
        patch: 0,
    };

    version >= min && version < max
}

fn assert_supported_zig_version(zig: &Path, version: ZigVersion) {
    if is_supported_zig_version(version) {
        return;
    }

    panic!(
        "Unsupported Zig version {}.{}.{} at {}.\n\
         ghostty_vt_sys currently requires Zig >= 0.15.2 and < 0.16.0.\n\
         Set ZIG to a compatible binary or install one at .context/zig/zig.",
        version.major,
        version.minor,
        version.patch,
        zig.display()
    );
}
