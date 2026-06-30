use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct Commit {
    pub change_id: String,
    pub commit_id: String,
    pub empty: bool,
    pub title: String,
    /// Short commit IDs of this commit's parents. Length >= 2 means this is
    /// a merge. Used by the sources view to decide whether to keep a commit
    /// for its topological role even when it has no PR.
    pub parents: Vec<String>,
    pub pr_url: Option<String>,
    pub pr_number: Option<u64>,
    /// `Some(true)` = draft, `Some(false)` = ready for review, `None` = no PR
    /// or status couldn't be fetched (treated as not-draft for filtering).
    pub is_draft: Option<bool>,
    /// Authoritative PR title as last set on GitHub. `None` when there's no
    /// PR or the gh lookup didn't return this PR. Callers should prefer this
    /// over `title` whenever it's populated.
    pub pr_title: Option<String>,
    /// PR lifecycle state from GitHub. `None` when no PR is referenced or the
    /// gh lookup didn't return a row for it (e.g. older than the gh window).
    pub pr_state: Option<crate::gh::PrState>,
    /// PR review decision from GitHub. `None` for the same reasons as
    /// `pr_state`; otherwise `Approved`/`ChangesRequested`/`ReviewRequired`/`None`.
    pub review_decision: Option<crate::gh::ReviewDecision>,
}

/// One line of jj log output. `graph` is the raw box-drawing prefix
/// (preserved verbatim so the tree renders correctly). `commit` is
/// `Some` when the line carries commit metadata, `None` for connector
/// lines (e.g. `│`, `├─╮`).
#[derive(Debug, Clone)]
pub struct LogLine {
    pub graph: String,
    pub commit: Option<Commit>,
}

const UNIT: char = '\x1f';
const CHANGE_ID_SHORT_LEN: usize = 12;
const COMMIT_ID_SHORT_LEN: usize = 12;

/// Single-line template per commit. `description.first_line()` keeps
/// each commit on one line so we can pair it with the graph chars on
/// the same row, while we make a separate non-graph call to pull the
/// full description for the "Pull Request:" trailer.
const TEMPLATE: &str = concat!(
    "change_id.short() ++ \"\\x1f\"",
    "++ commit_id.short() ++ \"\\x1f\"",
    "++ if(empty, \"1\", \"0\") ++ \"\\x1f\"",
    "++ parents.map(|p| p.commit_id().short()).join(\",\") ++ \"\\x1f\"",
    "++ description.first_line() ++ \"\\n\"",
);

const TRAILER_TEMPLATE: &str = concat!(
    "change_id.short() ++ \"\\x1f\"",
    "++ description ++ \"\\x1e\"",
);

/// Return jj log lines between trunk() and @, with the graph prefix preserved.
pub fn stack(repo: &Path) -> Result<Vec<LogLine>> {
    let revset = pick_revset(repo)?;
    let pr_map = run_trailer_log(repo, Some(&revset)).unwrap_or_default();
    let mut lines = run_graph_log(repo, Some(&revset))?;
    for line in &mut lines {
        if let Some(c) = line.commit.as_mut() {
            if let Some(url) = pr_map.get(&c.change_id) {
                c.pr_url = Some(url.clone());
                c.pr_number = parse_pr_number(url);
            }
        }
    }
    Ok(lines)
}

/// Return jj log lines using jj's default revset (the same view as
/// running `jj log` with no `-r`).
pub fn default_log(repo: &Path) -> Result<Vec<LogLine>> {
    let pr_map = run_trailer_log(repo, None).unwrap_or_default();
    let mut lines = run_graph_log(repo, None)?;
    for line in &mut lines {
        if let Some(c) = line.commit.as_mut() {
            if let Some(url) = pr_map.get(&c.change_id) {
                c.pr_url = Some(url.clone());
                c.pr_number = parse_pr_number(url);
            }
        }
    }
    Ok(lines)
}

/// Run `jj log` restricted to the given commit IDs (use commit IDs rather
/// than change IDs because change IDs can be divergent). jj draws the
/// topology among just those commits, inserting `~ (elided revisions)`
/// connectors where parent chains are skipped.
pub fn log_revs(repo: &Path, commit_ids: &[String]) -> Result<Vec<LogLine>> {
    if commit_ids.is_empty() {
        return Ok(Vec::new());
    }
    let revset = commit_ids.join("|");
    run_graph_log(repo, Some(&revset))
}

fn pick_revset(repo: &Path) -> Result<String> {
    for revset in ["trunk()..@", "main..@", "master..@"] {
        let status = Command::new("jj")
            .args(["--ignore-working-copy", "log", "--no-graph", "-r", revset, "-T", "\"\""])
            .current_dir(repo)
            .output();
        if let Ok(o) = status {
            if o.status.success() {
                return Ok(revset.to_string());
            }
        }
    }
    anyhow::bail!("no usable revset (trunk/main/master) in {}", repo.display())
}

fn run_graph_log(repo: &Path, revset: Option<&str>) -> Result<Vec<LogLine>> {
    let mut args: Vec<&str> = vec!["--ignore-working-copy", "log", "--color=never"];
    if let Some(r) = revset {
        args.push("-r");
        args.push(r);
    }
    args.push("-T");
    args.push(TEMPLATE);
    let output = Command::new("jj")
        .args(&args)
        .current_dir(repo)
        .output()
        .with_context(|| format!("spawn jj in {}", repo.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "jj log failed in {}: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut out = Vec::new();
    for raw in stdout.lines() {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        match line.split_once(UNIT) {
            None => {
                // Pure connector line (no commit data).
                out.push(LogLine {
                    graph: line.to_string(),
                    commit: None,
                });
            }
            Some((before, after)) => {
                // `before` ends with the change_id; everything before that is the graph prefix.
                let (graph, change_id) = split_graph_and_id(before);
                let rest: Vec<&str> = after.splitn(4, UNIT).collect();
                if rest.len() != 4 || rest[0].len() < COMMIT_ID_SHORT_LEN {
                    out.push(LogLine {
                        graph: line.to_string(),
                        commit: None,
                    });
                    continue;
                }
                let commit_id = rest[0].to_string();
                let empty = rest[1] == "1";
                let parents: Vec<String> = if rest[2].is_empty() {
                    Vec::new()
                } else {
                    rest[2].split(',').map(|s| s.to_string()).collect()
                };
                let title = rest[3].trim().to_string();
                out.push(LogLine {
                    graph: graph.to_string(),
                    commit: Some(Commit {
                        change_id: change_id.to_string(),
                        commit_id,
                        empty,
                        title,
                        parents,
                        pr_url: None,
                        pr_number: None,
                        is_draft: None,
                        pr_title: None,
                        pr_state: None,
                        review_decision: None,
                    }),
                });
            }
        }
    }
    Ok(out)
}

fn split_graph_and_id(before_unit: &str) -> (&str, &str) {
    // `before_unit` looks like `"@  abcdef012345"` or `"│ ○  abcdef012345"`.
    // The change_id is the trailing run of alphanumeric characters (length 12).
    if before_unit.len() >= CHANGE_ID_SHORT_LEN {
        let (graph, id) = before_unit.split_at(before_unit.len() - CHANGE_ID_SHORT_LEN);
        if id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return (graph, id);
        }
    }
    (before_unit, "")
}

fn run_trailer_log(
    repo: &Path,
    revset: Option<&str>,
) -> Result<std::collections::HashMap<String, String>> {
    let mut args: Vec<&str> = vec!["--ignore-working-copy", "log", "--no-graph"];
    if let Some(r) = revset {
        args.push("-r");
        args.push(r);
    }
    args.push("-T");
    args.push(TRAILER_TEMPLATE);
    let output = Command::new("jj")
        .args(&args)
        .current_dir(repo)
        .output()
        .with_context(|| format!("spawn jj (trailer) in {}", repo.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "jj log (trailer) failed in {}: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut map = std::collections::HashMap::new();
    for rec in stdout.split('\x1e') {
        if rec.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = rec.splitn(2, UNIT).collect();
        if parts.len() != 2 {
            continue;
        }
        let change_id = parts[0].trim_start_matches('\n').to_string();
        if let Some(url) = extract_pr_url(parts[1]) {
            map.insert(change_id, url);
        }
    }
    Ok(map)
}

fn extract_pr_url(desc: &str) -> Option<String> {
    for line in desc.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("Pull Request:") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn parse_pr_number(url: &str) -> Option<u64> {
    let last = url.trim_end_matches('/').rsplit('/').next()?;
    last.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pr_url() {
        let d = "feat: thing\n\nbody body\n\nPull Request: https://github.com/Specter-Co/qrt/pull/1762\n";
        assert_eq!(
            extract_pr_url(d).as_deref(),
            Some("https://github.com/Specter-Co/qrt/pull/1762")
        );
        assert_eq!(parse_pr_number("https://github.com/Specter-Co/qrt/pull/1762"), Some(1762));
    }
}
