use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct BranchConfig {
    branch: String,
}

#[derive(Debug)]
pub struct Workspace {
    pub path: PathBuf,
    pub branch: String,
    pub repos: Vec<RepoDir>,
}

#[derive(Debug)]
pub struct RepoDir {
    pub name: String,
    pub path: PathBuf,
    pub kind: RepoKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoKind {
    Jj,
    GitOnly,
}

#[derive(Debug)]
pub struct SourceRepo {
    pub namespace: String,
    pub name: String,
    pub path: PathBuf,
}

const MAX_DEPTH: usize = 6;

/// Enumerate `~/sources/<namespace>/<repo>` directories that hold a jj repo
/// (colocated or pure). Non-jj repos are dropped.
pub fn scan_sources(root: &Path) -> Result<Vec<SourceRepo>> {
    let mut out = Vec::new();
    let ns_iter = fs::read_dir(root)
        .with_context(|| format!("read_dir {}", root.display()))?;
    for ns_entry in ns_iter.flatten() {
        let ns_path = ns_entry.path();
        if !ns_path.is_dir() {
            continue;
        }
        let ns_name = ns_entry.file_name().to_string_lossy().to_string();
        if ns_name.starts_with('.') {
            continue;
        }
        let repo_iter = match fs::read_dir(&ns_path) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for repo_entry in repo_iter.flatten() {
            let path = repo_entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = repo_entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if !path.join(".jj").exists() {
                continue;
            }
            out.push(SourceRepo {
                namespace: ns_name.clone(),
                name,
                path,
            });
        }
    }
    out.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Ok(out)
}

pub fn scan(root: &Path) -> Result<Vec<Workspace>> {
    let mut out = Vec::new();
    walk(root, 0, &mut out)?;
    out.sort_by(|a, b| a.branch.cmp(&b.branch));
    Ok(out)
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<Workspace>) -> Result<()> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let toml_path = dir.join("branch.toml");
    if toml_path.exists() {
        match load_workspace(dir, &toml_path) {
            Ok(ws) => out.push(ws),
            Err(e) => eprintln!("workspace at {}: {e:#}", dir.display()),
        }
        return Ok(());
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("read_dir {}: {e}", dir.display());
            return Ok(());
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if s.starts_with('.') {
            continue;
        }
        walk(&path, depth + 1, out)?;
    }
    Ok(())
}

fn load_workspace(dir: &Path, toml_path: &Path) -> Result<Workspace> {
    let raw = fs::read_to_string(toml_path)
        .with_context(|| format!("read {}", toml_path.display()))?;
    let cfg: BranchConfig =
        toml::from_str(&raw).with_context(|| format!("parse {}", toml_path.display()))?;
    let mut repos = Vec::new();
    for entry in fs::read_dir(dir)
        .with_context(|| format!("read_dir {}", dir.display()))?
        .flatten()
    {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let kind = if path.join(".jj").exists() {
            RepoKind::Jj
        } else if path.join(".git").exists() {
            RepoKind::GitOnly
        } else {
            continue;
        };
        repos.push(RepoDir {
            name,
            path,
            kind,
        });
    }
    repos.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Workspace {
        path: dir.to_path_buf(),
        branch: cfg.branch,
        repos,
    })
}
