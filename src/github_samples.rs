//! Import de packs de sons depuis des dépôts GitHub.
//!
//! Un repo = une racine de samples dédiée sous `<data>/repos/<owner>-<repo>/`
//! (auto-incluse par `sample_roots`). Le tarball codeload est streamé et
//! filtré : seuls les fichiers audio et les manifestes `.json` / `*-alias.json`
//! sont extraits — un `strudel.json` à la racine du repo active le mode
//! manifeste (clés `bank_nom` → `s("nom").bank("bank")`).

use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;

/// Dépôt GitHub cible d'un import.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubRepo {
    pub owner: String,
    pub repo: String,
    /// Branche explicite (`/tree/<branch>` ou `owner/repo/branch`) ;
    /// `None` = branche par défaut (main puis master, puis API).
    pub branch: Option<String>,
}

impl GithubRepo {
    /// Nom de dossier sanitized : `<owner>-<repo>`.
    pub fn dir_name(&self) -> String {
        sanitize_component(&format!("{}-{}", self.owner, self.repo))
    }
}

/// Nom GitHub valide : owner = alnum + `-`, repo/branche = alnum + `-_.`.
fn valid_github_name(s: &str, owner: bool) -> bool {
    !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || if owner { c == '-' } else { matches!(c, '-' | '_' | '.') }
        })
}

/// Acccepte : `https://github.com/o/r`, `github.com/o/r`, `git@github.com:o/r.git`,
/// `github:o/r`, `o/r` et suffixes `/tree/<branch>`.
pub fn parse_github_url(input: &str) -> Option<GithubRepo> {
    // Ignore query/fragment collées par mégarde (?tab=readme, #readme).
    let s = input
        .trim()
        .trim_end_matches('/')
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('/');
    if s.is_empty() {
        return None;
    }
    let mut rest = s;
    let mut recognized = false;
    // Préfixes connus
    for prefix in [
        "https://github.com/",
        "http://github.com/",
        "github.com/",
        "git@github.com:",
        "github:",
        "www.github.com/",
    ] {
        if let Some(r) = rest.strip_prefix(prefix) {
            rest = r;
            recognized = true;
            break;
        }
    }
    // Une URL avec schéma ou un remote git@ non reconnu = autre hôte → refus.
    if !recognized && (s.contains("://") || s.starts_with("git@")) {
        return None;
    }
    rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }
    let owner = parts.remove(0).to_string();
    let repo = parts.remove(0).trim_end_matches(".git").to_string();
    if !valid_github_name(&owner, true) || !valid_github_name(&repo, false) {
        return None;
    }
    // /tree/<branch> ou segment seul = branche
    let branch = match parts.first() {
        Some(&"tree") => parts.get(1).map(|s| s.to_string()),
        Some(&b) if !b.is_empty() => Some(b.to_string()),
        _ => None,
    };
    if let Some(b) = &branch {
        if !valid_github_name(b, false) {
            return None;
        }
    }
    Some(GithubRepo {
        owner,
        repo,
        branch,
    })
}

fn sanitize_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Types de fichiers conservés à l'extraction.
fn keep_file(rel: &Path) -> bool {
    let name = rel
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if name.ends_with(".json") {
        return true; // manifestes strudel + *-alias.json
    }
    crate::sample_loader::is_audio_file(rel)
}

/// État d'avancement rapporté pendant l'import.
#[derive(Clone, Debug)]
pub struct ImportProgress {
    /// true = tarball téléchargé, extraction en cours.
    pub extracting: bool,
    pub files: usize,
    pub bytes: u64,
}

#[derive(Debug)]
pub struct ImportReport {
    pub dir: PathBuf,
    pub files: usize,
    pub bytes: u64,
}

/// Événements envoyés par la thread d'import vers l'UI.
#[derive(Debug)]
pub enum GithubImportEvent {
    Progress { files: usize, bytes: u64 },
    Done(Result<ImportReport, String>),
}

const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024; // 256 Mio / fichier
const MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 Gio / repo

/// Télécharge et extrait le repo dans `<repos_root>/<owner>-<repo>/`.
/// `on_progress` est appelé à chaque fichier extrait (et une fois au début).
/// Bloquant — à appeler depuis un thread.
pub fn import_repo(
    repo: &GithubRepo,
    dest_root: &Path,
    on_progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportReport, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())?;

    let branch = match &repo.branch {
        Some(b) => b.clone(),
        None => resolve_default_branch(&client, repo)?,
    };
    let url = format!(
        "https://codeload.github.com/{}/{}/tar.gz/refs/heads/{}",
        repo.owner, repo.repo, branch
    );
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| format!("téléchargement impossible : {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "repo introuvable ou branche « {branch} » absente (HTTP {})",
            resp.status()
        ));
    }

    let dest = dest_root.join(repo.dir_name());
    if dest.exists() {
        // Réimport = ré-extraction propre dans un dossier temporaire puis swap.
        let tmp = dest_root.join(format!(".{}.tmp", repo.dir_name()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
        let report = extract_tarball(resp, &tmp, on_progress)?;
        let _ = fs::remove_dir_all(&dest);
        fs::rename(&tmp, &dest).map_err(|e| format!("installation : {e}"))?;
        return Ok(ImportReport {
            dir: dest,
            files: report.files,
            bytes: report.bytes,
        });
    }
    fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
    match extract_tarball(resp, &dest, on_progress) {
        Ok(report) => Ok(report),
        Err(e) => {
            let _ = fs::remove_dir_all(&dest);
            Err(e)
        }
    }
}

fn resolve_default_branch(
    client: &reqwest::blocking::Client,
    repo: &GithubRepo,
) -> Result<String, String> {
    // Heuristique rapide : main puis master via codeload (évite l'API rate-limit).
    for cand in ["main", "master"] {
        let url = format!(
            "https://codeload.github.com/{}/{}/tar.gz/refs/heads/{}",
            repo.owner, repo.repo, cand
        );
        if let Ok(resp) = client.get(&url).send() {
            if resp.status().is_success() {
                return Ok(cand.to_string());
            }
        }
    }
    // Secours : API GitHub pour la vraie default_branch.
    let api = format!(
        "https://api.github.com/repos/{}/{}",
        repo.owner, repo.repo
    );
    let resp = client
        .get(&api)
        .header("User-Agent", "struidel")
        .send()
        .map_err(|e| format!("dépôt introuvable : {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("dépôt « {}/{} » introuvable (HTTP {})", repo.owner, repo.repo, resp.status()));
    }
    let v: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    v.get("default_branch")
        .and_then(|b| b.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "default_branch illisible".to_string())
}

/// Stream le tarball gzip et n'écrit que les fichiers utiles.
fn extract_tarball(
    reader: impl Read,
    dest: &Path,
    on_progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportReport, String> {
    let decoder = GzDecoder::new(reader);
    let mut archive = tar::Archive::new(decoder);
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut entries = archive
        .entries()
        .map_err(|e| format!("archive illisible : {e}"))?;

    on_progress(ImportProgress {
        extracting: false,
        files: 0,
        bytes: 0,
    });

    while let Some(entry) = entries.next() {
        let mut entry = entry.map_err(|e| format!("entrée tar : {e}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let size = entry.header().size().unwrap_or(0);
        if size > MAX_FILE_BYTES {
            continue;
        }
        if bytes + size > MAX_TOTAL_BYTES {
            return Err("dépôt trop volumineux (> 2 Gio de sons)".to_string());
        }
        let raw_path = entry.path().map_err(|e| e.to_string())?.into_owned();
        // Le tarball codeload préfixe tout par `<repo>-<branch>/` : on retire
        // le premier composant et on rejette les chemins dangereux.
        let rel = match strip_tar_root(&raw_path) {
            Some(r) => r,
            None => continue,
        };
        if rel.as_os_str().is_empty() || !keep_file(&rel) {
            continue;
        }
        let mut out_path = dest.to_path_buf();
        for comp in rel.components() {
            match comp {
                Component::Normal(c) => out_path.push(c),
                _ => {
                    out_path = PathBuf::new(); // chemin non sûr → skip
                    break;
                }
            }
        }
        if out_path.as_os_str().is_empty() {
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = fs::File::create(&out_path).map_err(|e| format!("{} : {e}", out_path.display()))?;
        // Copie bornée à `size` octets.
        let mut buf = vec![0u8; 1 << 20];
        let mut remaining = size;
        let mut written = 0u64;
        while remaining > 0 {
            let want = remaining.min(buf.len() as u64) as usize;
            let n = entry.read(&mut buf[..want]).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            written += n as u64;
            remaining -= n as u64;
        }
        drop(out);
        bytes += written;
        files += 1;
        on_progress(ImportProgress {
            extracting: true,
            files,
            bytes,
        });
    }

    if files == 0 {
        return Err("aucun fichier audio trouvé dans le dépôt".to_string());
    }
    Ok(ImportReport {
        dir: dest.to_path_buf(),
        files,
        bytes,
    })
}

/// Retire le composant racine `<repo>-<branch>/` du chemin tar.
/// Rejette les chemins absolus, `..`, `.git*` et tout ce qui n'est pas
/// un dossier normal en tête.
fn strip_tar_root(path: &Path) -> Option<PathBuf> {
    let mut comps = path.components();
    // Le 1er composant DOIT être un dossier normal (la racine du tarball) :
    // RootDir/ParentDir/CurDir en tête = chemin hostile → refus.
    match comps.next() {
        Some(Component::Normal(_)) => {}
        _ => return None,
    }
    let mut out = PathBuf::new();
    for comp in comps {
        match comp {
            Component::Normal(c) => {
                let s = c.to_string_lossy();
                if s.starts_with(".git") {
                    return None;
                }
                out.push(c);
            }
            _ => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_urls() {
        let cases = [
            ("https://github.com/tidalcycles/Dirt-Samples", "tidalcycles", "Dirt-Samples", None),
            ("https://github.com/user/repo/", "user", "repo", None),
            ("github.com/a/b", "a", "b", None),
            ("git@github.com:me/pads.git", "me", "pads", None),
            ("github:owner/samples", "owner", "samples", None),
            ("https://github.com/o/r/tree/dev", "o", "r", Some("dev")),
            ("o/r", "o", "r", None),
        ];
        for (input, owner, repo, branch) in cases {
            let r = parse_github_url(input).unwrap_or_else(|| panic!("{input}"));
            assert_eq!(r.owner, owner, "{input}");
            assert_eq!(r.repo, repo, "{input}");
            assert_eq!(r.branch.as_deref(), branch, "{input}");
        }
        assert!(parse_github_url("https://github.com/onlyowner").is_none());
        assert!(parse_github_url("").is_none());
        // Autres hôtes et noms invalides → refus.
        assert!(parse_github_url("https://gitlab.com/a/b").is_none());
        assert!(parse_github_url("git@gitlab.com:a/b.git").is_none());
        assert!(parse_github_url("https://github.com/a b/c").is_none());
        // Query/fragment tolérées.
        assert_eq!(
            parse_github_url("https://github.com/a/b?tab=readme").unwrap().repo,
            "b"
        );
    }

    #[test]
    fn dir_name_sanitized() {
        let r = GithubRepo {
            owner: "me".into(),
            repo: "my pack!".into(),
            branch: None,
        };
        assert_eq!(r.dir_name(), "me-my-pack");
    }

    #[test]
    fn strip_root_and_reject_bad_paths() {
        assert_eq!(
            strip_tar_root(Path::new("repo-main/kick/001.wav")),
            Some(PathBuf::from("kick/001.wav"))
        );
        assert_eq!(strip_tar_root(Path::new("repo-main/")), None);
        assert_eq!(strip_tar_root(Path::new("repo-main/../evil.wav")), None);
        assert_eq!(strip_tar_root(Path::new("/abs/x.wav")), None);
        assert_eq!(strip_tar_root(Path::new("repo-main/.git/config")), None);
        assert_eq!(strip_tar_root(Path::new("repo-main/.github/workflows/x.yml")), None);
    }

    #[test]
    fn keep_file_filters() {
        assert!(keep_file(Path::new("kick/001.wav")));
        assert!(keep_file(Path::new("strudel.json")));
        assert!(keep_file(Path::new("crate-alias.json")));
        assert!(!keep_file(Path::new("README.md")));
        assert!(!keep_file(Path::new("docs/pic.png")));
        assert!(!keep_file(Path::new("kit/archive.zip")));
    }

    /// Extraction de bout en bout sur un tarball synthétique local —
    /// passe par le vrai `extract_tarball`.
    #[test]
    fn extract_tarball_end_to_end() {
        // Construit un .tar.gz en mémoire : racine `repo-main/` + fichiers.
        let mut tar_buf = Vec::new();
        {
            let enc = flate2::write::GzEncoder::new(&mut tar_buf, flate2::Compression::fast());
            let mut builder = tar::Builder::new(enc);
            let mut add = |path: &str, data: &[u8]| {
                let mut h = tar::Header::new_gnu();
                h.set_size(data.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                builder
                    .append_data(&mut h, path, data)
                    .expect("append");
            };
            add("repo-main/kick/001.wav", &[1u8, 2, 3]);
            add("repo-main/hat/002.WAV", &[4u8]);
            add("repo-main/strudel.json", b"{\"_base\":\"x\"}");
            add("repo-main/README.md", b"nope");
            add("repo-main/.git/config", b"x");
            add("repo-main/pads/lead.mp3", &[7u8, 8]);
            builder.into_inner().unwrap().finish().unwrap();
        }

        let dest = std::env::temp_dir().join(format!("gh-import-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dest);
        fs::create_dir_all(&dest).unwrap();

        let report = extract_tarball(&tar_buf[..], &dest, &mut |_| {}).unwrap();
        assert_eq!(report.files, 4);
        assert!(dest.join("kick/001.wav").exists());
        assert!(dest.join("strudel.json").exists());
        assert!(!dest.join("README.md").exists());
        assert!(!dest.join(".git/config").exists());
        assert_eq!(fs::read(dest.join("pads/lead.mp3")).unwrap(), vec![7u8, 8]);
        let _ = fs::remove_dir_all(&dest);
    }
}
