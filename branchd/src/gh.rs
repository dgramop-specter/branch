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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
    None,
}

#[derive(Debug, Clone)]
pub struct PrInfo {
    pub is_draft: bool,
    pub title: String,
    pub state: PrState,
    pub review_decision: ReviewDecision,
}

/// For each (owner, repo) in `repos`, run `gh pr list --state all` once and
/// collect per-PR metadata. Includes closed and merged PRs so the UI can show
/// red/purple badges for them. PRs outside the 500-most-recent window are
/// absent from the map — callers fall back to the local jj commit title.
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
                "all",
                "--json",
                "number,isDraft,title,state,reviewDecision",
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
            #[serde(default)]
            state: String,
            #[serde(rename = "reviewDecision", default)]
            review_decision: String,
        }
        let rows: Vec<Row> = match serde_json::from_slice(&output.stdout) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("parse gh json for {slug}: {e}");
                continue;
            }
        };
        for r in rows {
            let state = match r.state.as_str() {
                "OPEN" => PrState::Open,
                "MERGED" => PrState::Merged,
                "CLOSED" => PrState::Closed,
                _ => PrState::Open,
            };
            let review_decision = match r.review_decision.as_str() {
                "APPROVED" => ReviewDecision::Approved,
                "CHANGES_REQUESTED" => ReviewDecision::ChangesRequested,
                "REVIEW_REQUIRED" => ReviewDecision::ReviewRequired,
                _ => ReviewDecision::None,
            };
            out.insert(
                (owner.clone(), repo.clone(), r.number),
                PrInfo {
                    is_draft: r.is_draft,
                    title: r.title,
                    state,
                    review_decision,
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
