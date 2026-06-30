use anyhow::{Context, Result};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
    Router,
};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::{Mutex, RwLock};

use crate::discovery::{self, RepoKind, Workspace};
use crate::{gh, jj};

const REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// Snapshot of the PR cache passed into render functions. Cheap to clone —
/// holds ~hundreds of small `PrInfo` entries.
#[derive(Default, Clone)]
struct CacheSnapshot {
    map: HashMap<(String, String, u64), gh::PrInfo>,
    last_sync: Option<SystemTime>,
}

/// Shared mutable cache populated by the background refresh task and the
/// `/resync` endpoint. Renderers take a cloned `CacheSnapshot` and never touch
/// the lock themselves, so a slow render can't starve a refresh.
struct PrCacheInner {
    map: HashMap<(String, String, u64), gh::PrInfo>,
    last_sync: Option<SystemTime>,
}

impl PrCacheInner {
    fn snapshot(&self) -> CacheSnapshot {
        CacheSnapshot {
            map: self.map.clone(),
            last_sync: self.last_sync,
        }
    }
}

/// Apply cached PR metadata to every commit in `groups`. Pure (no I/O): the
/// cache is populated out-of-band by the background refresh task. Sets
/// `is_draft`, `pr_title`, `pr_state`, and `review_decision`; commits whose PRs
/// haven't been fetched yet render in fallback form (plain "PR" badge).
fn annotate_drafts(
    groups: &mut [&mut [jj::LogLine]],
    map: &HashMap<(String, String, u64), gh::PrInfo>,
) {
    if map.is_empty() {
        return;
    }
    for group in groups.iter_mut() {
        for line in group.iter_mut() {
            if let Some(c) = line.commit.as_mut() {
                if let Some(url) = &c.pr_url {
                    if let Some(key) = gh::parse_pr_url(url) {
                        if let Some(info) = map.get(&key) {
                            c.is_draft = Some(info.is_draft);
                            c.pr_title = Some(info.title.clone());
                            c.pr_state = Some(info.state);
                            c.review_decision = Some(info.review_decision);
                        }
                    }
                }
            }
        }
    }
}

fn has_any_jj(ws: &Workspace) -> bool {
    ws.repos.iter().any(|r| r.kind == RepoKind::Jj)
}

/// Display title for a commit. Prefers the authoritative GitHub PR title
/// (when the PR is open and gh returned it) over the local jj commit
/// description.first_line(), which may be stale relative to renames done
/// via `gh pr edit`.
/// Build the trailing markdown status tag for a PR bullet. Order matters:
/// `closed`/`merged` short-circuit `draft`. The trailing space ensures the
/// `` `draft` ``-tail regex in the show-drafts JS filter still matches.
fn md_status_tag(c: &jj::Commit) -> String {
    match c.pr_state {
        Some(gh::PrState::Closed) => " `closed`".to_string(),
        Some(gh::PrState::Merged) => " `merged`".to_string(),
        _ => {
            let mut s = String::new();
            if c.review_decision == Some(gh::ReviewDecision::Approved) {
                s.push_str(" \u{2705}");
            }
            if c.is_draft == Some(true) {
                s.push_str(" `draft`");
            }
            s
        }
    }
}

fn display_title<'a>(c: &'a jj::Commit) -> &'a str {
    c.pr_title
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if c.title.is_empty() {
                "(no description)"
            } else {
                c.title.as_str()
            }
        })
}

#[derive(Clone)]
struct AppState {
    root: Arc<PathBuf>,
    sources: Arc<PathBuf>,
    cache: Arc<RwLock<PrCacheInner>>,
    /// Held for the duration of a refresh. Ensures /resync calls serialize and
    /// don't pile up parallel `gh pr list` runs against the same repos.
    refresh_lock: Arc<Mutex<()>>,
}

pub async fn serve(addrs: Vec<SocketAddr>, root: PathBuf, sources: PathBuf) -> Result<()> {
    let state = AppState {
        root: Arc::new(root),
        sources: Arc::new(sources),
        cache: Arc::new(RwLock::new(PrCacheInner {
            map: HashMap::new(),
            last_sync: None,
        })),
        refresh_lock: Arc::new(Mutex::new(())),
    };

    // Initial fetch in the background so the daemon starts serving immediately
    // (first request may show "syncing…" until this finishes).
    tokio::spawn({
        let state = state.clone();
        async move {
            refresh_cache(&state, "initial").await;
        }
    });
    // Periodic refresh every 5 minutes.
    tokio::spawn({
        let state = state.clone();
        async move {
            let mut interval = tokio::time::interval(REFRESH_INTERVAL);
            // Skip the immediate first tick — the spawn above handles initial fetch.
            interval.tick().await;
            loop {
                interval.tick().await;
                refresh_cache(&state, "scheduled").await;
            }
        }
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/w/*branch", get(workspace_view))
        .route("/sources", get(sources_index))
        .route("/s/*nsrepo", get(source_view))
        .route("/resync", post(resync))
        .route("/healthz", get(healthz))
        .with_state(state);

    // One TCP listener per bind address, all sharing the same Router. Each
    // gets its own tokio task; if any errors the daemon exits.
    let mut joinset = tokio::task::JoinSet::new();
    for addr in addrs {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind {addr}"))?;
        let app = app.clone();
        joinset.spawn(async move {
            axum::serve(listener, app)
                .await
                .with_context(|| format!("serve {addr}"))
        });
    }
    while let Some(res) = joinset.join_next().await {
        res.context("axum task join")??;
    }
    Ok(())
}

/// Walk every workspace and source repo, collect the (owner, repo) pairs
/// referenced by PR trailers, run `gh pr list` once per repo, then store the
/// resulting map. Serialized via `refresh_lock` so only one runs at a time.
/// `source` is a short tag included in log lines ("initial", "scheduled",
/// "user").
async fn refresh_cache(state: &AppState, source: &str) {
    let waiting_started = std::time::Instant::now();
    let _guard = state.refresh_lock.lock().await;
    let waited = waiting_started.elapsed();
    if waited.as_millis() > 50 {
        eprintln!(
            "refresh[{source}]: queued behind another refresh for {:.1}s",
            waited.as_secs_f64()
        );
    }
    eprintln!("refresh[{source}]: starting");
    let started = std::time::Instant::now();
    let root = state.root.clone();
    let sources = state.sources.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut repos: HashSet<(String, String)> = HashSet::new();
        if let Ok(workspaces) = discovery::scan(&root) {
            for ws in workspaces {
                for r in &ws.repos {
                    if r.kind != RepoKind::Jj {
                        continue;
                    }
                    if let Ok(lines) = jj::stack(&r.path) {
                        for line in lines {
                            if let Some(c) = line.commit {
                                if let Some(url) = &c.pr_url {
                                    if let Some((o, n, _)) = gh::parse_pr_url(url) {
                                        repos.insert((o, n));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Ok(srcs) = discovery::scan_sources(&sources) {
            for s in srcs {
                if let Ok(lines) = jj::default_log(&s.path) {
                    for line in lines {
                        if let Some(c) = line.commit {
                            if let Some(url) = &c.pr_url {
                                if let Some((o, n, _)) = gh::parse_pr_url(url) {
                                    repos.insert((o, n));
                                }
                            }
                        }
                    }
                }
            }
        }
        let repo_count = repos.len();
        let map = gh::fetch_pr_map(&repos);
        (repo_count, map)
    })
    .await;
    let (repo_count, map) = match result {
        Ok(v) => v,
        Err(e) => {
            eprintln!("refresh[{source}]: join error: {e}");
            return;
        }
    };
    let pr_count = map.len();
    let mut w = state.cache.write().await;
    w.map = map;
    w.last_sync = Some(SystemTime::now());
    eprintln!(
        "refresh[{source}]: done in {:.1}s ({repo_count} repos, {pr_count} PRs)",
        started.elapsed().as_secs_f64()
    );
}

async fn cache_snapshot(state: &AppState) -> CacheSnapshot {
    state.cache.read().await.snapshot()
}

async fn resync(State(state): State<AppState>) -> impl IntoResponse {
    refresh_cache(&state, "user").await;
    StatusCode::OK
}

async fn healthz() -> &'static str {
    "ok\n"
}

async fn index(State(state): State<AppState>) -> impl IntoResponse {
    let root = state.root.clone();
    let snap = cache_snapshot(&state).await;
    let body = tokio::task::spawn_blocking(move || render_index(&root, &snap))
        .await
        .unwrap_or_else(|e| format!("<pre>join error: {e}</pre>"));
    Html(body)
}

async fn workspace_view(
    State(state): State<AppState>,
    Path(branch): Path<String>,
) -> impl IntoResponse {
    let root = state.root.clone();
    let snap = cache_snapshot(&state).await;
    let result = tokio::task::spawn_blocking(move || render_one(&root, &branch, &snap))
        .await
        .unwrap_or_else(|e| Err(format!("join error: {e}")));
    match result {
        Ok(body) => (StatusCode::OK, Html(body)),
        Err(msg) => (StatusCode::NOT_FOUND, Html(error_page(&msg))),
    }
}

async fn sources_index(State(state): State<AppState>) -> impl IntoResponse {
    let sources = state.sources.clone();
    let snap = cache_snapshot(&state).await;
    let body = tokio::task::spawn_blocking(move || render_sources_index(&sources, &snap))
        .await
        .unwrap_or_else(|e| format!("<pre>join error: {e}</pre>"));
    Html(body)
}

async fn source_view(
    State(state): State<AppState>,
    Path(nsrepo): Path<String>,
) -> impl IntoResponse {
    let sources = state.sources.clone();
    let root = state.root.clone();
    let snap = cache_snapshot(&state).await;
    let result =
        tokio::task::spawn_blocking(move || render_source(&sources, &root, &nsrepo, &snap))
            .await
            .unwrap_or_else(|e| Err(format!("join error: {e}")));
    match result {
        Ok(body) => (StatusCode::OK, Html(body)),
        Err(msg) => (StatusCode::NOT_FOUND, Html(error_page(&msg))),
    }
}

/// (workspace_branch, repo_name) tags per git commit_id, gathered across all
/// workspaces. Used to annotate source-repo commits with the lines of work
/// that include them.
type CommitTags = HashMap<String, Vec<(String, String)>>;

fn build_commit_tags(root: &PathBuf) -> CommitTags {
    let mut map: CommitTags = HashMap::new();
    let workspaces = match discovery::scan(root) {
        Ok(v) => v,
        Err(_) => return map,
    };
    for ws in workspaces {
        for repo in &ws.repos {
            if repo.kind != RepoKind::Jj {
                continue;
            }
            let Ok(lines) = jj::stack(&repo.path) else {
                continue;
            };
            for line in lines {
                if let Some(c) = line.commit {
                    map.entry(c.commit_id)
                        .or_default()
                        .push((ws.branch.clone(), repo.name.clone()));
                }
            }
        }
    }
    map
}

/// Fetch jj::stack for each jj repo in `ws`, annotate from the cache snapshot,
/// return pairs of (repo_name, lines). Trims the leading empty/no-description
/// commit (jj's working copy `@`) off each stack.
fn fetch_ws_lines(ws: &Workspace, snap: &CacheSnapshot) -> Vec<(String, Vec<jj::LogLine>)> {
    let mut pairs: Vec<(String, Vec<jj::LogLine>)> = Vec::new();
    for repo in &ws.repos {
        if repo.kind != RepoKind::Jj {
            continue;
        }
        let lines = trim_top_empty(jj::stack(&repo.path).unwrap_or_default());
        pairs.push((repo.name.clone(), lines));
    }
    let mut slices: Vec<&mut [jj::LogLine]> = pairs.iter_mut().map(|(_, v)| v.as_mut_slice()).collect();
    annotate_drafts(&mut slices, &snap.map);
    pairs
}

/// Drop leading rows while each commit encountered is empty AND has no
/// description (jj's working-copy `@` at the top of a stack). Any connector
/// rows preceding the dropped commit are also discarded.
fn trim_top_empty(lines: Vec<jj::LogLine>) -> Vec<jj::LogLine> {
    let mut out = Vec::new();
    let mut still_trimming = true;
    let mut pending: Vec<jj::LogLine> = Vec::new();
    for line in lines {
        if !still_trimming {
            out.push(line);
            continue;
        }
        match &line.commit {
            None => pending.push(line),
            Some(c) if c.empty && c.title.is_empty() => {
                // Drop the buffered connectors with this commit.
                pending.clear();
            }
            Some(_) => {
                out.append(&mut pending);
                out.push(line);
                still_trimming = false;
            }
        }
    }
    out
}

fn render_index(root: &PathBuf, snap: &CacheSnapshot) -> String {
    let workspaces: Vec<Workspace> = match discovery::scan(root) {
        Ok(v) => v.into_iter().filter(has_any_jj).collect(),
        Err(e) => return error_page(&format!("scan failed: {e:#}")),
    };
    let mut body = String::new();
    body.push_str(PAGE_HEAD);
    render_nav(&mut body, "workspaces", snap);
    render_controls(&mut body);
    body.push_str(&format!(
        "<p class=\"sub\">root: <code>{}</code> &middot; {} workspace{}</p>\n",
        html_escape(&root.display().to_string()),
        workspaces.len(),
        if workspaces.len() == 1 { "" } else { "s" }
    ));
    if workspaces.is_empty() {
        body.push_str("<p>No jj workspaces found.</p>\n");
    }
    let no_tags: CommitTags = HashMap::new();
    for ws in &workspaces {
        let repo_lines = fetch_ws_lines(ws, snap);
        render_workspace(&mut body, ws, &repo_lines, &no_tags, Some(&ws.branch));
    }
    body.push_str(PAGE_TAIL);
    body
}

fn render_one(root: &PathBuf, branch: &str, snap: &CacheSnapshot) -> Result<String, String> {
    let workspaces = discovery::scan(root).map_err(|e| format!("scan failed: {e:#}"))?;
    let ws = workspaces
        .into_iter()
        .find(|w| w.branch == branch && has_any_jj(w))
        .ok_or_else(|| format!("no jj workspace with branch {branch:?}"))?;
    let mut body = String::new();
    body.push_str(PAGE_HEAD);
    render_nav(&mut body, "workspaces", snap);
    render_controls(&mut body);
    body.push_str("<p class=\"sub\"><a href=\"/\">&larr; all workspaces</a></p>\n");
    let no_tags: CommitTags = HashMap::new();
    let repo_lines = fetch_ws_lines(&ws, snap);
    render_workspace(&mut body, &ws, &repo_lines, &no_tags, Some(&ws.branch));
    body.push_str(PAGE_TAIL);
    Ok(body)
}

fn render_sources_index(sources_root: &PathBuf, snap: &CacheSnapshot) -> String {
    let repos = match discovery::scan_sources(sources_root) {
        Ok(v) => v,
        Err(e) => return error_page(&format!("scan_sources failed: {e:#}")),
    };
    let mut body = String::new();
    body.push_str(PAGE_HEAD);
    render_nav(&mut body, "sources", snap);
    body.push_str(&format!(
        "<p class=\"sub\">root: <code>{}</code> &middot; {} source{}</p>\n",
        html_escape(&sources_root.display().to_string()),
        repos.len(),
        if repos.len() == 1 { "" } else { "s" }
    ));
    if repos.is_empty() {
        body.push_str("<p>No jj source repos found.</p>\n");
    }
    let mut current_ns: Option<&str> = None;
    for repo in &repos {
        if current_ns != Some(repo.namespace.as_str()) {
            if current_ns.is_some() {
                body.push_str("</ul>\n");
            }
            body.push_str(&format!(
                "<h2>{ns}</h2>\n<ul class=\"source-list\">\n",
                ns = html_escape(&repo.namespace)
            ));
            current_ns = Some(repo.namespace.as_str());
        }
        let href = format!(
            "/s/{}/{}",
            url_path_escape(&repo.namespace),
            url_path_escape(&repo.name)
        );
        body.push_str(&format!(
            "<li><a href=\"{href}\">{name}</a></li>\n",
            href = href,
            name = html_escape(&repo.name)
        ));
    }
    if current_ns.is_some() {
        body.push_str("</ul>\n");
    }
    body.push_str(PAGE_TAIL);
    body
}

fn render_source(
    sources_root: &PathBuf,
    workspace_root: &PathBuf,
    nsrepo: &str,
    snap: &CacheSnapshot,
) -> Result<String, String> {
    let (ns, name) = nsrepo
        .split_once('/')
        .ok_or_else(|| format!("bad path {nsrepo:?}; expected <namespace>/<repo>"))?;
    let repo_path = sources_root.join(ns).join(name);
    if !repo_path.join(".jj").exists() {
        return Err(format!(
            "no jj source repo at {}",
            repo_path.display()
        ));
    }
    let tags = build_commit_tags(workspace_root);
    let mut lines = jj::default_log(&repo_path)
        .map_err(|e| format!("jj log failed: {e:#}"))?;
    {
        let mut slices = [lines.as_mut_slice()];
        annotate_drafts(&mut slices[..], &snap.map);
    }
    let focused = focus_on_open(lines);

    // Re-render the focused subset by asking jj to draw the graph for *just*
    // those commits. jj's own algorithm gives accurate merges/splits between
    // surviving commits and emits `~ (elided revisions)` between distant ones.
    let focus_commit_ids: Vec<String> = focused
        .iter()
        .filter_map(|l| l.commit.as_ref().map(|c| c.commit_id.clone()))
        .collect();
    let mut enriched: std::collections::HashMap<String, jj::Commit> =
        std::collections::HashMap::new();
    for line in &focused {
        if let Some(c) = &line.commit {
            enriched.insert(c.commit_id.clone(), c.clone());
        }
    }
    let topo_lines = jj::log_revs(&repo_path, &focus_commit_ids)
        .unwrap_or_else(|e| {
            eprintln!("log_revs: {e:#}");
            Vec::new()
        });
    // Graft the pre-enriched commit data (PR/draft/etc.) onto the topo-aware
    // graph. The graph chars come from jj; the commit metadata is what we
    // already fetched.
    let grafted: Vec<jj::LogLine> = topo_lines
        .into_iter()
        .map(|mut l| {
            if let Some(c) = &l.commit {
                if let Some(rich) = enriched.get(&c.commit_id) {
                    l.commit = Some(rich.clone());
                }
            }
            l
        })
        .collect();

    let mut body = String::new();
    body.push_str(PAGE_HEAD);
    render_nav(&mut body, "sources", snap);
    render_controls(&mut body);
    body.push_str(&format!(
        "<p class=\"sub\"><a href=\"/sources\">&larr; all sources</a> &middot; <code>{}</code></p>\n",
        html_escape(&repo_path.display().to_string())
    ));
    body.push_str(&format!(
        "<h2>{}/{}</h2>\n",
        html_escape(ns),
        html_escape(name)
    ));
    if grafted.is_empty() {
        body.push_str("<p class=\"empty\">no commits with open PRs</p>\n");
    } else {
        let md = build_source_md(ns, name, &grafted, &tags);
        render_copy_button(
            &mut body,
            &format!("md-src-{}-{}", id_slug(ns), id_slug(name)),
            &md,
        );
        body.push_str("<div class=\"stack\">\n");
        render_log_lines(&mut body, &grafted, &tags, None);
        body.push_str("</div>\n");
    }
    body.push_str(PAGE_TAIL);
    Ok(body)
}

/// Return the commits to surface on the sources page. Keeps any commit with
/// an open PR (draft or ready), plus topological pivots between them — merges
/// (>=2 parents) and splits (>=2 visible children in this log). Trims away
/// commits outside the [first PR, last PR] window. The graph drawing happens
/// downstream: caller hands the resulting commit IDs back to `jj log -r ...`
/// so jj draws the topology with real branching characters.
fn focus_on_open(lines: Vec<jj::LogLine>) -> Vec<jj::LogLine> {
    let commits: Vec<jj::LogLine> = lines
        .into_iter()
        .filter(|l| l.commit.is_some())
        .collect();

    // Build child-count map: how many commits in the log list each commit as a
    // parent. >=2 means this commit is a split point in the visible topology.
    let mut child_count: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for line in &commits {
        for parent in &line.commit.as_ref().unwrap().parents {
            *child_count.entry(parent.clone()).or_insert(0) += 1;
        }
    }

    let is_structural = |c: &jj::Commit| -> bool {
        c.parents.len() >= 2 || child_count.get(&c.commit_id).copied().unwrap_or(0) >= 2
    };
    let has_open_pr = |c: &jj::Commit| -> bool {
        c.pr_url.is_some() && c.pr_state == Some(gh::PrState::Open)
    };

    // Anchor the window between the first and last commit *with an open PR* —
    // structural commits outside that span aren't between two lines of effort,
    // so they don't get preserved.
    let pr_positions: Vec<usize> = commits
        .iter()
        .enumerate()
        .filter_map(|(i, l)| {
            if has_open_pr(l.commit.as_ref().unwrap()) {
                Some(i)
            } else {
                None
            }
        })
        .collect();
    let (Some(&first), Some(&last)) = (pr_positions.first(), pr_positions.last()) else {
        return Vec::new();
    };

    // Mark each commit "kept" if it has an open PR or is a structural pivot.
    // We additionally keep the *boundary* commits of any run of non-kept
    // commits — the one adjacent to the preceding kept commit and the one
    // adjacent to the following kept commit. Without these, jj would draw a
    // direct edge between distant kept commits and the fork point would
    // disappear into an elision marker. With them, jj sees the branching
    // structure (e.g. two sibling commits sharing a parent) and renders it.
    let kept: Vec<bool> = commits
        .iter()
        .map(|l| {
            let c = l.commit.as_ref().unwrap();
            has_open_pr(c) || is_structural(c)
        })
        .collect();
    let mut include: Vec<bool> = kept.clone();
    let mut i = first;
    while i <= last {
        if kept[i] {
            i += 1;
            continue;
        }
        let mut j = i;
        while j <= last && !kept[j] {
            j += 1;
        }
        // [i, j) is a run of non-kept commits between two kept ones.
        // Include the run's first and last (they're the topological neighbours
        // of the surrounding kept commits, so jj can draw the fork/merge).
        include[i] = true;
        if j > i + 1 {
            include[j - 1] = true;
        }
        i = j;
    }

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (i, line) in commits.iter().enumerate() {
        if i < first || i > last || !include[i] {
            continue;
        }
        let c = line.commit.as_ref().unwrap();
        // The same commit can appear multiple times in jj's default log when it
        // sits on multiple visible paths. Dedupe by commit_id so jj's restricted
        // log doesn't get a redundant revset.
        if !seen.insert(c.commit_id.clone()) {
            continue;
        }
        out.push(line.clone());
    }
    out
}

fn render_controls(body: &mut String) {
    body.push_str(
        "<div class=\"controls\">\
         <label><input type=\"checkbox\" id=\"show-drafts\"> show drafts</label>\
         <label><input type=\"checkbox\" id=\"show-empty\"> show empty</label>\
         </div>\n",
    );
}

/// Build a Slack-pasteable markdown bullet list for one (repo, lines) pair.
/// Each PR-carrying commit becomes a markdown-linked bullet; non-PR commits
/// are skipped. Drafts are tagged so they're visible at a glance.
fn build_repo_md(name: &str, lines: &[jj::LogLine], indent: bool) -> String {
    let pr_commits: Vec<&jj::Commit> = lines
        .iter()
        .filter_map(|l| l.commit.as_ref())
        .filter(|c| c.pr_url.is_some())
        .collect();
    if pr_commits.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    let header = if indent { format!("*{}*", name) } else { format!("**{}**", name) };
    out.push_str(&header);
    out.push('\n');
    for c in pr_commits {
        let url = c.pr_url.as_ref().unwrap();
        let label = c
            .pr_number
            .map(|n| format!("#{n}"))
            .unwrap_or_else(|| "PR".to_string());
        let status_tag = md_status_tag(c);
        let title = display_title(c);
        out.push_str(&format!("- [{label}]({url}) {title}{status_tag}\n"));
    }
    out.push('\n');
    out
}

fn build_workspace_md(ws: &Workspace, repo_lines: &[(String, Vec<jj::LogLine>)]) -> String {
    let mut out = format!("**{}**\n\n", ws.branch);
    for (name, lines) in repo_lines {
        let block = build_repo_md(name, lines, true);
        if !block.is_empty() {
            out.push_str(&block);
        }
    }
    out
}

fn build_source_md(
    ns: &str,
    name: &str,
    lines: &[jj::LogLine],
    tags: &CommitTags,
) -> String {
    let mut out = format!("**{ns}/{name}**\n\n");
    for line in lines {
        let Some(c) = &line.commit else { continue };
        let Some(url) = &c.pr_url else { continue };
        let label = c
            .pr_number
            .map(|n| format!("#{n}"))
            .unwrap_or_else(|| "PR".to_string());
        let status_tag = md_status_tag(c);
        let title = display_title(c);
        let ws_suffix = tags
            .get(&c.commit_id)
            .map(|v| {
                let names: std::collections::BTreeSet<&String> = v.iter().map(|(b, _)| b).collect();
                if names.is_empty() {
                    String::new()
                } else {
                    let joined = names
                        .into_iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(" — _{}_", joined)
                }
            })
            .unwrap_or_default();
        out.push_str(&format!(
            "- [{label}]({url}) {title}{status_tag}{ws_suffix}\n"
        ));
    }
    out
}

/// Render the copy button + a hidden `<pre>` containing the markdown. `id`
/// must be unique on the page — multiple workspaces share the index page,
/// each gets its own button.
fn render_copy_button(body: &mut String, id: &str, markdown: &str) {
    if markdown.trim().is_empty() {
        return;
    }
    body.push_str(&format!(
        "<div class=\"copy-wrap\">\
         <button class=\"copy-md\" type=\"button\" data-target=\"{id}\">Copy as Markdown</button>\
         <pre id=\"{id}\" hidden>{md}</pre>\
         </div>\n",
        id = id,
        md = html_escape(markdown),
    ));
}

fn id_slug(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

fn render_nav(body: &mut String, active: &str, snap: &CacheSnapshot) {
    let workspaces_class = if active == "workspaces" { " active" } else { "" };
    let sources_class = if active == "sources" { " active" } else { "" };
    let sync_status = render_sync_status(snap);
    body.push_str(&format!(
        "<nav class=\"top\"><a class=\"brand\" href=\"/\">branchd</a>\
         <a class=\"navlink{wc}\" href=\"/\">workspaces</a>\
         <a class=\"navlink{sc}\" href=\"/sources\">sources</a>\
         {sync}</nav>\n",
        wc = workspaces_class,
        sc = sources_class,
        sync = sync_status,
    ));
}

/// Inline status block for the nav bar: "Last sync: 3m ago" plus a Resync
/// button. The button is wired up in the page-tail JS to POST `/resync` and
/// reload on completion.
fn render_sync_status(snap: &CacheSnapshot) -> String {
    let label = match snap.last_sync {
        None => "never synced".to_string(),
        Some(t) => match SystemTime::now().duration_since(t) {
            Ok(d) => format!("synced {}", humanize_age(d)),
            // Clock jumped backwards (e.g. NTP step) — fall back to absolute.
            Err(_) => "synced just now".to_string(),
        },
    };
    format!(
        "<span class=\"sync\"><span class=\"sync-time\" title=\"refreshes every 5 min\">{label}</span>\
         <button class=\"resync\" type=\"button\">Resync</button></span>",
        label = html_escape(&label),
    )
}

fn humanize_age(d: Duration) -> String {
    let s = d.as_secs();
    if s < 5 {
        "just now".to_string()
    } else if s < 60 {
        format!("{s}s ago")
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86400)
    }
}

fn render_workspace(
    body: &mut String,
    ws: &Workspace,
    repo_lines: &[(String, Vec<jj::LogLine>)],
    tags: &CommitTags,
    hide_branch: Option<&str>,
) {
    body.push_str("<section class=\"ws\">\n");
    body.push_str(&format!(
        "<h2><a href=\"/w/{href}\">{branch}</a></h2>\n<p class=\"sub\"><code>{path}</code></p>\n",
        href = url_path_escape(&ws.branch),
        branch = html_escape(&ws.branch),
        path = html_escape(&ws.path.display().to_string()),
    ));
    let md = build_workspace_md(ws, repo_lines);
    render_copy_button(body, &format!("md-ws-{}", id_slug(&ws.branch)), &md);
    if repo_lines.is_empty() {
        body.push_str("<p class=\"empty\">no jj repos</p>\n");
        body.push_str("</section>\n");
        return;
    }
    for (name, lines) in repo_lines {
        body.push_str("<div class=\"repo\">\n");
        body.push_str(&format!("<h3>{name}</h3>\n", name = html_escape(name)));
        if lines.is_empty() {
            body.push_str("<p class=\"empty\">no commits ahead of trunk</p>\n");
        } else {
            body.push_str("<div class=\"stack\">\n");
            render_log_lines(body, lines, tags, hide_branch);
            body.push_str("</div>\n");
        }
        body.push_str("</div>\n");
    }
    body.push_str("</section>\n");
}

/// Render a parsed jj log (with graph) as a sequence of `<div>`/`<a>` rows.
/// `tags` annotates each commit with the lines of work that include it;
/// commits matching `hide_branch` are skipped from the chip list (so the
/// page you're viewing doesn't tag itself).
fn render_log_lines(
    body: &mut String,
    lines: &[jj::LogLine],
    tags: &CommitTags,
    hide_branch: Option<&str>,
) {
    for line in lines {
        render_log_line(body, line, tags, hide_branch);
    }
}

fn render_log_line(
    body: &mut String,
    line: &jj::LogLine,
    tags: &CommitTags,
    hide_branch: Option<&str>,
) {
    let graph = html_escape(&line.graph);
    match &line.commit {
        None => {
            body.push_str(&format!(
                "<div class=\"row connector\"><span class=\"graph\">{graph}</span></div>\n",
            ));
        }
        Some(c) => {
            let title = display_title(c);
            let state_class = match c.pr_state {
                Some(gh::PrState::Merged) => " merged",
                Some(gh::PrState::Closed) => " closed",
                _ if c.is_draft == Some(true) => " draft",
                _ => "",
            };
            let approved_mark = if c.review_decision == Some(gh::ReviewDecision::Approved) {
                " <span class=\"check\" title=\"approved\">&#10003;</span>"
            } else {
                ""
            };
            let pr_chip = match (&c.pr_url, c.pr_number) {
                (Some(_), Some(n)) => format!(
                    " <span class=\"pr{cls}\">#{n}{mark}</span>",
                    cls = state_class,
                    n = n,
                    mark = approved_mark,
                ),
                (Some(_), None) => format!(
                    " <span class=\"pr{cls}\">PR{mark}</span>",
                    cls = state_class,
                    mark = approved_mark,
                ),
                _ => String::new(),
            };
            let empty_html = if c.empty {
                " <span class=\"empty-tag\">empty</span>"
            } else {
                ""
            };
            let ws_chips = render_ws_chips(tags, &c.commit_id, hide_branch);
            let main_inner = format!(
                "<span class=\"graph\">{graph}</span><code class=\"id\">{cid}</code> <span class=\"title\">{title}</span>{pr}{empty}",
                graph = graph,
                cid = html_escape(&c.change_id),
                title = html_escape(title),
                pr = pr_chip,
                empty = empty_html,
            );
            let main = match &c.pr_url {
                Some(url) => format!(
                    "<a class=\"linked\" href=\"{url}\" target=\"_blank\">{main_inner}</a>",
                    url = html_escape(url),
                    main_inner = main_inner,
                ),
                None => main_inner,
            };
            let draft_attr = match c.is_draft {
                Some(true) => "true",
                _ => "false",
            };
            let empty_attr = if c.empty { "true" } else { "false" };
            body.push_str(&format!(
                "<div class=\"row commit\" data-empty=\"{e}\" data-draft=\"{d}\">{main}{ws}</div>\n",
                e = empty_attr,
                d = draft_attr,
                main = main,
                ws = ws_chips,
            ));
        }
    }
}

fn render_ws_chips(tags: &CommitTags, commit_id: &str, hide: Option<&str>) -> String {
    let Some(entries) = tags.get(commit_id) else {
        return String::new();
    };
    let mut out = String::new();
    // Dedupe by branch — a commit can show up under multiple repos in the same
    // workspace (shared library, etc.); only show the workspace once.
    let mut seen = std::collections::BTreeSet::new();
    for (branch, _repo) in entries {
        if Some(branch.as_str()) == hide {
            continue;
        }
        if !seen.insert(branch.clone()) {
            continue;
        }
        out.push_str(&format!(
            " <a class=\"wschip\" href=\"/w/{href}\">{branch}</a>",
            href = url_path_escape(branch),
            branch = html_escape(branch),
        ));
    }
    out
}

fn error_page(msg: &str) -> String {
    format!(
        "{head}<h1>branchd</h1><pre class=\"err\">{msg}</pre>{tail}",
        head = PAGE_HEAD,
        msg = html_escape(msg),
        tail = PAGE_TAIL,
    )
}

fn url_path_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

const PAGE_HEAD: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>branchd</title>
<style>
  :root { color-scheme: light dark; }
  body { font: 14px/1.4 -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif;
         max-width: 960px; margin: 2rem auto; padding: 0 1rem; }
  h1 { margin: 0 0 .25rem 0; }
  h2 a { color: inherit; text-decoration: none; }
  h2 a:hover { text-decoration: underline; }
  nav.top { display: flex; gap: 1rem; align-items: baseline; margin-bottom: .75rem;
            padding-bottom: .5rem; border-bottom: 1px solid #ddd3; }
  nav.top .brand { font-weight: 700; font-size: 1.1rem; color: inherit; text-decoration: none; }
  nav.top .navlink { color: #888; text-decoration: none; }
  nav.top .navlink.active { color: inherit; font-weight: 600; }
  nav.top .navlink:hover { color: inherit; }
  nav.top .sync { margin-left: auto; display: inline-flex; align-items: center; gap: .5rem;
                  font-size: .85em; color: #888; }
  nav.top .sync-time { font-variant-numeric: tabular-nums; }
  button.resync { font: inherit; font-size: .85em; padding: .15rem .5rem; cursor: pointer;
                  border: 1px solid #8884; background: #8881; color: inherit;
                  border-radius: 4px; }
  button.resync:hover:not(:disabled) { background: #8882; }
  button.resync:disabled { cursor: progress; opacity: .7; }
  ul.source-list { list-style: none; padding-left: 1rem; margin: .25rem 0 .75rem; }
  ul.source-list li { padding: .1rem 0; }
  .wschip { display: inline-block; padding: 0 .35rem; margin-left: .25rem;
            background: #4a90e222; color: #4a90e2; border-radius: 4px; font-size: .8em;
            text-decoration: none; font-weight: 600; }
  .wschip:hover { background: #4a90e244; }
  h2 { margin: 1.5rem 0 .25rem 0; font-size: 1.15rem; }
  h3 { margin: .75rem 0 .25rem 0; font-size: 1rem; font-weight: 600; }
  .sub { color: #777; margin: 0 0 .5rem 0; }
  code { font-family: ui-monospace, "SF Mono", Menlo, monospace; font-size: .9em; }
  .ws { border-top: 1px solid #ddd3; padding-top: .5rem; margin-top: 1rem; }
  .repo { margin-left: .5rem; padding: .25rem .5rem .5rem; border-left: 2px solid #ccc4; }
  .stack { margin: .25rem 0; }
  .row { display: block; font: 13px/1.2 ui-monospace, "SF Mono", Menlo, monospace;
         padding: 1px 4px; border-radius: 3px; color: inherit; text-decoration: none;
         white-space: pre; }
  .row.commit { padding: 2px 4px; }
  .row .linked { color: inherit; text-decoration: none; display: inline; }
  .row .linked:hover { background: #2da44e18; border-radius: 3px; }
  .row.connector { color: #888; }
  .graph { color: #888; white-space: pre; }
  .id { color: #888; margin-right: .35rem; }
  .title { white-space: pre-wrap; }
  .pr { padding: 0 .35rem; margin-left: .35rem; border-radius: 4px;
        background: #2da44e22; color: #2da44e; font-weight: 600; font-size: .9em; }
  .pr.draft { background: #8884; color: #777; }
  .pr.merged { background: #8250df22; color: #8250df; }
  .pr.closed { background: #cf222e22; color: #cf222e; }
  .pr .check { margin-left: .25rem; font-weight: 700; }
  .controls { display: flex; gap: 1rem; align-items: center; margin: 0 0 .75rem;
              font-size: .9em; color: #888; }
  .controls label { cursor: pointer; user-select: none; }
  .copy-wrap { margin: 0 0 .75rem; }
  button.copy-md { font: inherit; font-size: .85em; padding: .25rem .6rem; cursor: pointer;
                   border: 1px solid #8884; background: #8881; color: inherit;
                   border-radius: 4px; }
  button.copy-md:hover { background: #8882; }
  button.copy-md.copied { background: #2da44e22; border-color: #2da44e88; color: #2da44e; }
  body.hide-drafts .row.commit[data-draft="true"] { display: none; }
  body.hide-empty .row.commit[data-empty="true"] { display: none; }
  .nopr { color: #aaa; font-style: italic; }
  .badge { background: #8884; color: #444; padding: 0 .35rem; border-radius: 4px;
           font-size: .75em; vertical-align: middle; margin-left: .25rem; }
  .empty { color: #999; font-style: italic; }
  .empty-tag { color: #aa6; font-size: .75em; margin-left: .25rem; }
  .err { color: #c33; }
  @media (prefers-color-scheme: dark) {
    body { background: #111; color: #ddd; }
    .sub, .empty { color: #999; }
    .id { color: #888; }
    .badge { color: #ccc; }
  }
</style>
</head>
<body>
"#;

const PAGE_TAIL: &str = r#"<script>
(function() {
  // Default both filters ON (drafts + empties hidden). User preference
  // persists in localStorage across requests/page loads.
  function load(key, def) {
    var v = localStorage.getItem(key);
    return v === null ? def : v === "1";
  }
  function save(key, val) { localStorage.setItem(key, val ? "1" : "0"); }
  function apply(showDrafts, showEmpty) {
    document.body.classList.toggle("hide-drafts", !showDrafts);
    document.body.classList.toggle("hide-empty", !showEmpty);
  }
  var sd = load("branchd.showDrafts", false);
  var se = load("branchd.showEmpty", false);
  var cbD = document.getElementById("show-drafts");
  var cbE = document.getElementById("show-empty");
  if (cbD) { cbD.checked = sd; cbD.addEventListener("change", function() { save("branchd.showDrafts", cbD.checked); apply(cbD.checked, cbE && cbE.checked); }); }
  if (cbE) { cbE.checked = se; cbE.addEventListener("change", function() { save("branchd.showEmpty", cbE.checked); apply(cbD && cbD.checked, cbE.checked); }); }
  apply(sd, se);

  // Copy-as-markdown button: reads textContent from the hidden <pre> it
  // points at, optionally strips draft lines (matching the show-drafts
  // checkbox), and writes to the clipboard. Falls back to a textarea +
  // execCommand when the page isn't served over HTTPS (clipboard API
  // requires a secure context — branchd is HTTP-on-tailscale).
  function filterMarkdown(text, hideDrafts) {
    if (!hideDrafts) return text;
    // Split into blank-line-separated blocks; each block has an optional
    // header line followed by `- ` bullets. Drop draft bullets (lines ending
    // in `` `draft` ``); drop a block entirely if no bullets remain.
    var blocks = text.split(/\n[ \t]*\n/);
    var kept = [];
    for (var i = 0; i < blocks.length; i++) {
      var lines = blocks[i].split("\n").filter(function(l) { return l.length > 0; });
      var headers = [], bullets = [];
      for (var j = 0; j < lines.length; j++) {
        (lines[j].indexOf("- ") === 0 ? bullets : headers).push(lines[j]);
      }
      var keptBullets = bullets.filter(function(l) {
        return !/`draft`\s*$/.test(l);
      });
      if (keptBullets.length === 0 && bullets.length > 0) continue;
      kept.push(headers.concat(keptBullets).join("\n"));
    }
    return kept.join("\n\n") + (text.endsWith("\n") ? "\n" : "");
  }

  // Resync button: POST /resync, then reload so the new badge data appears.
  // Disables the button + swaps label while the request is in flight; multiple
  // /resync calls serialize server-side via the refresh lock.
  document.querySelectorAll("button.resync").forEach(function(btn) {
    btn.addEventListener("click", function() {
      var orig = btn.textContent;
      btn.disabled = true;
      btn.textContent = "Syncing…";
      fetch("/resync", { method: "POST" })
        .then(function(r) {
          if (!r.ok) throw new Error("resync " + r.status);
          location.reload();
        })
        .catch(function(e) {
          btn.disabled = false;
          btn.textContent = orig;
          console.error(e);
        });
    });
  });

  document.querySelectorAll("button.copy-md").forEach(function(btn) {
    btn.addEventListener("click", function() {
      var target = document.getElementById(btn.dataset.target);
      if (!target) return;
      var hideDrafts = !!cbD && !cbD.checked;
      var text = filterMarkdown(target.textContent, hideDrafts);
      var done = function() {
        var orig = btn.textContent;
        btn.textContent = "Copied!";
        btn.classList.add("copied");
        setTimeout(function() { btn.textContent = orig; btn.classList.remove("copied"); }, 1200);
      };
      var fallback = function() {
        var ta = document.createElement("textarea");
        ta.value = text;
        ta.style.position = "fixed"; ta.style.opacity = "0";
        document.body.appendChild(ta);
        ta.select();
        try { document.execCommand("copy"); done(); } catch (e) {}
        document.body.removeChild(ta);
      };
      if (navigator.clipboard && window.isSecureContext) {
        navigator.clipboard.writeText(text).then(done, fallback);
      } else {
        fallback();
      }
    });
  });
})();
</script>
</body></html>
"#;
