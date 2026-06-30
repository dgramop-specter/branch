use std::collections::{HashMap, HashSet};
use std::process::Command;

/// Parse a GitHub PR URL like `https://github.com/<owner>/<repo>/pull/<n>`
/// into its components. Returns `None` for any other URL shape.
pub fn parse_pr_url(url: &str) -> Option<(String, String, u64)> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let mut parts = rest.trim_end_matches('/').split('/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    let pull = parts.next()?;
    if pull != "pull" && pull != "pulls" {
        return None;
    }
    let n: u64 = parts.next()?.parse().ok()?;
    Some((owner, repo, n))
}

#[derive(Debug, Clone)]
pub struct PrInfo {
    pub is_draft: bool,
    pub title: String,
}

/// For each (owner, repo) in `repos`, run `gh pr list --state open --json number,isDraft,title`
/// once and collect per-PR metadata. Closed/merged PRs aren't returned by
/// `gh pr list`, so they're absent from the map — callers fall back to the
/// local jj commit title for those.
pub fn fetch_pr_map(
    repos: &HashSet<(String, String)>,
) -> HashMap<(String, String, u64), PrInfo> {
    let mut out = HashMap::new();
    for (owner, repo) in repos {
        let slug = format!("{owner}/{repo}");
        let output = match Command::new("gh")
            .args([
                "pr",
                "list",
                "--repo",
                &slug,
                "--state",
                "open",
                "--json",
                "number,isDraft,title",
                "--limit",
                "500",
            ])
            .output()
        {
            Ok(o) if o.status.success() => o,
            Ok(o) => {
                eprintln!(
                    "gh pr list {}: {}",
                    slug,
                    String::from_utf8_lossy(&o.stderr).trim()
                );
                continue;
            }
            Err(e) => {
                eprintln!("spawn gh: {e}");
                continue;
            }
        };
        #[derive(serde::Deserialize)]
        struct Row {
            number: u64,
            #[serde(rename = "isDraft")]
            is_draft: bool,
            title: String,
        }
        let rows: Vec<Row> = match serde_json::from_slice(&output.stdout) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("parse gh json for {slug}: {e}");
                continue;
            }
        };
        for r in rows {
            out.insert(
                (owner.clone(), repo.clone(), r.number),
                PrInfo {
                    is_draft: r.is_draft,
                    title: r.title,
                },
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pr_url() {
        let u = "https://github.com/Specter-Co/qrt/pull/1762";
        assert_eq!(
            parse_pr_url(u),
            Some(("Specter-Co".into(), "qrt".into(), 1762))
        );
    }

    #[test]
    fn rejects_non_pr_url() {
        assert!(parse_pr_url("https://github.com/Specter-Co/qrt/issues/1").is_none());
        assert!(parse_pr_url("https://example.com/x/y/pull/1").is_none());
    }
}
