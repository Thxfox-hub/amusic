//! Resolve and bootstrap the Strudel JS runtime shipped with struIDEl.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Packaged Strudel tree installed by the distro package.
pub const SYSTEM_STRUDEL_ROOT: &str = "/usr/share/struidel/strudel";
/// Source archive used to (re)install the runtime next to user data.
pub const SYSTEM_RUNTIME_ARCHIVE: &str = "/usr/share/struidel/struidel-runtime.tar.gz";

fn runtime_ok(root: &Path) -> bool {
    root.join("scripts").join("strudel-runner.mjs").is_file()
        && root.join("packages").join("core").is_dir()
        && root.join("doc.json").is_file()
}

/// XDG data dir for a user-local runtime install.
pub fn user_strudel_root() -> PathBuf {
    if let Some(dirs) = directories::ProjectDirs::from("cc", "strudel", "struidel") {
        return dirs.data_local_dir().join("strudel");
    }
    PathBuf::from("struidel-runtime")
}

fn candidate_roots() -> Vec<PathBuf> {
    let mut out = Vec::new();

    if let Ok(p) = std::env::var("STRUDEL_HOME") {
        out.push(PathBuf::from(p));
    }

    let settings = crate::settings::load_settings();
    if let Some(ref p) = settings.strudel_local_path {
        out.push(PathBuf::from(p));
    }

    out.push(PathBuf::from(SYSTEM_STRUDEL_ROOT));
    out.push(user_strudel_root());

    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            out.push(parent.join("../share/struidel/strudel"));
            out.push(parent.join("strudel"));
            out.push(parent.join("../strudel"));
        }
    }

    // Development monorepo: ide-native/../strudel
    out.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("strudel"),
    );

    out
}

fn find_existing_runtime() -> Option<PathBuf> {
    for cand in candidate_roots() {
        if let Ok(c) = cand.canonicalize() {
            if runtime_ok(&c) {
                return Some(c);
            }
        } else if runtime_ok(&cand) {
            return Some(cand);
        }
    }
    None
}

fn find_bootstrap_archive() -> Option<PathBuf> {
    let mut cands = vec![PathBuf::from(SYSTEM_RUNTIME_ARCHIVE)];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            cands.push(parent.join("../share/struidel/struidel-runtime.tar.gz"));
            cands.push(parent.join("struidel-runtime.tar.gz"));
        }
    }
    // Next to packaged sources during development
    cands.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("aur")
            .join("struidel")
            .join("struidel-runtime-0.1.0.tar.gz"),
    );
    cands.into_iter().find(|p| p.is_file())
}

fn link_strudel_packages(root: &Path) -> Result<(), String> {
    let nm = root.join("node_modules").join("@strudel");
    fs::create_dir_all(&nm).map_err(|e| format!("mkdir @strudel: {e}"))?;
    for pkg in ["core", "mini", "tonal", "transpiler", "superdough"] {
        let link = nm.join(pkg);
        let target = PathBuf::from("../../packages").join(pkg);
        let _ = fs::remove_file(&link);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &link)
                .map_err(|e| format!("symlink @strudel/{pkg}: {e}"))?;
        }
        #[cfg(not(unix))]
        {
            let _ = target;
            let _ = link;
        }
    }
    Ok(())
}

fn npm_install_runtime(root: &Path) -> Result<(), String> {
    let status = Command::new("npm")
        .args(["install", "--omit=dev", "--no-fund", "--no-audit"])
        .current_dir(root)
        .status()
        .map_err(|e| {
            format!("failed to run npm (install the 'npm' package): {e}")
        })?;
    if !status.success() {
        return Err(format!("npm install failed in {}", root.display()));
    }
    link_strudel_packages(root)
}

fn bootstrap_from_archive(archive: &Path, dest: &Path) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    if dest.exists() {
        fs::remove_dir_all(dest).map_err(|e| format!("remove {}: {e}", dest.display()))?;
    }

    let staging_parent = dest
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let staging = staging_parent.join(format!(
        ".struidel-runtime-extract-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| format!("mkdir staging: {e}"))?;

    let status = Command::new("tar")
        .args([
            "-xzf",
            archive.to_str().ok_or("archive path is not UTF-8")?,
            "-C",
            staging.to_str().ok_or("staging path is not UTF-8")?,
        ])
        .status()
        .map_err(|e| format!("failed to run tar: {e}"))?;
    if !status.success() {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!("tar extract failed for {}", archive.display()));
    }

    let extracted = staging.join("struidel-runtime");
    if !extracted.is_dir() {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!(
            "archive {} does not contain struidel-runtime/",
            archive.display()
        ));
    }

    fs::rename(&extracted, dest).map_err(|e| {
        let _ = fs::remove_dir_all(&staging);
        format!("move runtime to {}: {e}", dest.display())
    })?;
    let _ = fs::remove_dir_all(&staging);

    npm_install_runtime(dest)?;
    if !runtime_ok(dest) {
        return Err(format!(
            "runtime install incomplete at {}",
            dest.display()
        ));
    }
    Ok(())
}

/// Ensure a usable Strudel runtime exists; install into the user data dir if missing.
pub fn ensure_strudel_runtime() -> Result<PathBuf, String> {
    if let Some(existing) = find_existing_runtime() {
        return Ok(existing);
    }

    let dest = user_strudel_root();
    let archive = find_bootstrap_archive().ok_or_else(|| {
        format!(
            "Strudel runtime not found and no install archive available \
             (expected {SYSTEM_RUNTIME_ARCHIVE}). Reinstall the struidel package."
        )
    })?;

    eprintln!(
        "[struidel] Strudel runtime missing — installing from {} → {}",
        archive.display(),
        dest.display()
    );
    bootstrap_from_archive(&archive, &dest)?;

    let mut settings = crate::settings::load_settings();
    settings.strudel_local_path = Some(dest.to_string_lossy().into_owned());
    crate::settings::save_settings(&settings);

    eprintln!("[struidel] Strudel runtime ready at {}", dest.display());
    Ok(dest)
}

/// Root of the Strudel JS runtime (`scripts/`, `packages/`, `doc.json`).
/// Prefers an already-valid install; does not bootstrap (call [`ensure_strudel_runtime`] first).
pub fn strudel_root() -> PathBuf {
    find_existing_runtime().unwrap_or_else(|| {
        let user = user_strudel_root();
        if runtime_ok(&user) {
            user
        } else {
            PathBuf::from(SYSTEM_STRUDEL_ROOT)
        }
    })
}

pub fn doc_json_path() -> Option<PathBuf> {
    let path = strudel_root().join("doc.json");
    path.is_file().then_some(path)
}

pub fn runner_mjs_path() -> PathBuf {
    strudel_root().join("scripts").join("strudel-runner.mjs")
}

pub fn package_dir(package: &str) -> PathBuf {
    strudel_root().join("packages").join(package)
}

/// Default samples / cache directory (XDG data).
pub fn default_sample_folder() -> PathBuf {
    if let Some(dirs) = directories::ProjectDirs::from("cc", "strudel", "struidel") {
        return dirs.data_local_dir().join("samples");
    }
    PathBuf::from("struidel-samples")
}

/// Cache file for the language index (XDG cache).
pub fn language_index_cache_path() -> PathBuf {
    if let Some(dirs) = directories::ProjectDirs::from("cc", "strudel", "struidel") {
        return dirs.cache_dir().join("strudel_language_index.json");
    }
    std::env::temp_dir().join("struidel_language_index.json")
}
