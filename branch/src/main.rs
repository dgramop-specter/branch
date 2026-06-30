use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Serialize, Deserialize)]
struct BranchConfig {
    branch: String,
}

#[derive(Parser)]
#[command(name = "branch")]
#[command(about = "A utility for managing jj workspaces across multiple repos")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new branch directory with branch.toml
    New {
        /// Branch name (slashes become subdirectories)
        branch_name: String,
    },
    /// Clone a repo as a jj workspace (or full clone with --deep)
    Clone {
        /// GitHub namespace (e.g., "anthropics")
        namespace: String,
        /// GitHub repo name (e.g., "claude-code")
        repo: String,
        /// Do a full clone instead of a workspace
        #[arg(long)]
        deep: bool,
    },
    /// Migrate an existing trees directory structure to use branch.toml
    Migrate {
        /// Path to the trees directory to migrate
        trees_path: PathBuf,
        /// Show what would be done without making changes
        #[arg(long)]
        dry_run: bool,
    },
    /// Print the directory containing branch.toml
    Root,
    /// Generate Nix flake URLs for a sibling repo
    Nix {
        #[command(subcommand)]
        nix_command: NixCommands,
    },
}

#[derive(Subcommand)]
enum NixCommands {
    /// Generate a remote git+ssh Nix URL for the repo
    Remote {
        /// Name of the sibling repo
        repo: String,
    },
    /// Generate a local git+file Nix URL for the repo
    Local {
        /// Name of the sibling repo
        repo: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::New { branch_name }) => cmd_new(&branch_name),
        Some(Commands::Clone {
            namespace,
            repo,
            deep,
        }) => cmd_clone(&namespace, &repo, deep),
        Some(Commands::Migrate {
            trees_path,
            dry_run,
        }) => cmd_migrate(&trees_path, dry_run),
        Some(Commands::Root) => cmd_root(),
        Some(Commands::Nix { nix_command }) => match nix_command {
            NixCommands::Remote { repo } => cmd_nix_remote(&repo),
            NixCommands::Local { repo } => cmd_nix_local(&repo),
        },
        None => cmd_show_branch(),
    }
}

/// Find branch.toml by traversing up from the current directory
fn find_branch_toml() -> Option<PathBuf> {
    let mut current = env::current_dir().ok()?;
    loop {
        let candidate = current.join("branch.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Read the branch config from branch.toml
fn read_branch_config(path: &Path) -> Result<BranchConfig> {
    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let config: BranchConfig =
        toml::from_str(&content).with_context(|| "Failed to parse branch.toml")?;
    Ok(config)
}

/// Create a new branch directory with branch.toml
fn cmd_new(branch_name: &str) -> Result<()> {
    let cwd = env::current_dir().context("Failed to get current directory")?;
    let branch_dir = cwd.join(branch_name);

    fs::create_dir_all(&branch_dir)
        .with_context(|| format!("Failed to create directory {}", branch_dir.display()))?;

    let config = BranchConfig {
        branch: branch_name.to_string(),
    };
    let toml_content = toml::to_string_pretty(&config).context("Failed to serialize config")?;
    let toml_path = branch_dir.join("branch.toml");

    fs::write(&toml_path, toml_content)
        .with_context(|| format!("Failed to write {}", toml_path.display()))?;

    println!("Created branch directory: {}", branch_dir.display());
    println!("Branch: {}", branch_name);

    Ok(())
}

/// Clone a repo as a workspace or full clone
fn cmd_clone(namespace: &str, repo: &str, deep: bool) -> Result<()> {
    // Find branch.toml
    let toml_path = find_branch_toml().context(
        "No branch.toml found in current or parent directories. Run 'branch new <name>' first.",
    )?;
    let config = read_branch_config(&toml_path)?;
    let branch_name = &config.branch;

    // Get the branch directory (where branch.toml lives)
    let branch_dir = toml_path.parent().unwrap();

    // Source repo path: ~/sources/<namespace>/<repo>
    let home = env::var("HOME").context("HOME environment variable not set")?;
    let sources_dir = PathBuf::from(&home).join("sources").join(namespace);
    let source_repo_path = sources_dir.join(repo);

    // Target directory: <branch_dir>/<repo>
    let target_dir = branch_dir.join(repo);

    if target_dir.exists() {
        bail!("Target directory already exists: {}", target_dir.display());
    }

    // Ensure the source repo exists
    ensure_source_repo(&source_repo_path, namespace, repo)?;

    if deep {
        // Full clone
        do_deep_clone(&source_repo_path, &target_dir, branch_name)?;
    } else {
        // jj workspace
        do_workspace(&source_repo_path, &target_dir, branch_name)?;
    }

    println!("Cloned {} to {}", repo, target_dir.display());
    println!("Branch: {}", branch_name);

    Ok(())
}

/// Get the origin remote URL from a jj repo
fn get_remote_url(repo_path: &Path) -> Result<String> {
    let output = Command::new("jj")
        .args(["git", "remote", "list"])
        .current_dir(repo_path)
        .output()
        .context("Failed to run jj git remote list")?;

    if !output.status.success() {
        bail!("jj git remote list failed");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(2, char::is_whitespace).collect();
        if parts.len() == 2 && parts[0].trim() == "origin" {
            return Ok(parts[1].trim().to_string());
        }
    }

    bail!("No 'origin' remote found");
}

/// Parse namespace and repo from a GitHub remote URL
fn parse_github_url(url: &str) -> Result<(String, String)> {
    // Handle git@github.com:owner/repo.git
    if let Some(rest) = url.strip_prefix("git@github.com:") {
        let path = rest.strip_suffix(".git").unwrap_or(rest);
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        if parts.len() == 2 {
            return Ok((parts[0].to_string(), parts[1].to_string()));
        }
    }
    // Handle https://github.com/owner/repo.git
    if let Some(rest) = url.strip_prefix("https://github.com/") {
        let path = rest.strip_suffix(".git").unwrap_or(rest);
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        if parts.len() == 2 {
            return Ok((parts[0].to_string(), parts[1].to_string()));
        }
    }
    // Handle ssh://git@github.com/owner/repo.git
    if let Some(rest) = url.strip_prefix("ssh://git@github.com/") {
        let path = rest.strip_suffix(".git").unwrap_or(rest);
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        if parts.len() == 2 {
            return Ok((parts[0].to_string(), parts[1].to_string()));
        }
    }
    bail!("Cannot parse GitHub URL: {}", url);
}

/// Check if a jj bookmark exists locally
fn bookmark_exists_local(repo_path: &Path, name: &str) -> bool {
    Command::new("jj")
        .args(["bookmark", "list", name])
        .current_dir(repo_path)
        .output()
        .map(|o| {
            o.status.success() && {
                let stdout = String::from_utf8_lossy(&o.stdout);
                stdout.lines().any(|l| l.starts_with(name))
            }
        })
        .unwrap_or(false)
}

/// Check if a jj bookmark exists on origin
fn bookmark_exists_remote(repo_path: &Path, name: &str) -> bool {
    Command::new("jj")
        .args(["bookmark", "list", "--all-remotes", name])
        .current_dir(repo_path)
        .output()
        .map(|o| {
            o.status.success() && {
                let stdout = String::from_utf8_lossy(&o.stdout);
                let remote_marker = format!("{}@origin", name);
                stdout.lines().any(|l| l.contains(&remote_marker))
            }
        })
        .unwrap_or(false)
}

/// Sanitize a branch name for use as a jj workspace name
fn sanitize_workspace_name(branch_name: &str) -> String {
    branch_name.replace('/', "_")
}

/// Ensure the source repo exists at ~/sources/<namespace>/<repo>
fn ensure_source_repo(source_repo_path: &Path, namespace: &str, repo: &str) -> Result<()> {
    if source_repo_path.exists() {
        // If source repo exists but doesn't have .jj, colocate it
        if !source_repo_path.join(".jj").exists() {
            println!("Initializing jj in existing git repo...");
            let status = Command::new("jj")
                .args(["git", "init", "--colocate"])
                .current_dir(source_repo_path)
                .status()
                .context("Failed to run jj git init --colocate")?;
            if !status.success() {
                bail!("jj git init --colocate failed");
            }
        }

        println!("Source repo exists, fetching latest...");
        let status = Command::new("jj")
            .args(["git", "fetch", "--all-remotes"])
            .current_dir(source_repo_path)
            .status()
            .context("Failed to run jj git fetch")?;
        if !status.success() {
            bail!("jj git fetch failed");
        }
        return Ok(());
    }

    // Create parent directories
    let parent = source_repo_path.parent().unwrap();
    fs::create_dir_all(parent).with_context(|| format!("Failed to create {}", parent.display()))?;

    // Clone the repo using jj with git colocation
    let ssh_url = format!("git@github.com:{}/{}.git", namespace, repo);
    println!("Cloning {} to {}...", ssh_url, source_repo_path.display());

    let status = Command::new("jj")
        .args([
            "git",
            "clone",
            "--colocate",
            &ssh_url,
            source_repo_path.to_str().unwrap(),
        ])
        .status()
        .context("Failed to run jj git clone")?;

    if !status.success() {
        bail!("jj git clone failed");
    }

    Ok(())
}

/// Create a jj workspace at target_dir for the given branch
fn do_workspace(source_repo_path: &Path, target_dir: &Path, branch_name: &str) -> Result<()> {
    let ws_name = sanitize_workspace_name(branch_name);

    // Create the workspace
    let status = Command::new("jj")
        .args([
            "workspace",
            "add",
            "--name",
            &ws_name,
            target_dir.to_str().unwrap(),
        ])
        .current_dir(source_repo_path)
        .status()
        .context("Failed to run jj workspace add")?;

    if !status.success() {
        bail!("jj workspace add failed");
    }

    // Navigate the workspace to the right bookmark
    let local_exists = bookmark_exists_local(source_repo_path, branch_name);
    let remote_exists = bookmark_exists_remote(source_repo_path, branch_name);

    if local_exists {
        let status = Command::new("jj")
            .args(["new", branch_name])
            .current_dir(target_dir)
            .status()
            .context("Failed to run jj new")?;

        if !status.success() {
            bail!("jj new {} failed", branch_name);
        }
    } else if remote_exists {
        // Track the remote bookmark first
        let status = Command::new("jj")
            .args(["bookmark", "track", &format!("{}@origin", branch_name)])
            .current_dir(target_dir)
            .status()
            .context("Failed to track remote bookmark")?;

        if !status.success() {
            bail!("jj bookmark track failed");
        }

        let status = Command::new("jj")
            .args(["new", branch_name])
            .current_dir(target_dir)
            .status()
            .context("Failed to run jj new")?;

        if !status.success() {
            bail!("jj new {} failed", branch_name);
        }
    } else {
        // Bookmark doesn't exist anywhere — create on top of trunk
        let status = Command::new("jj")
            .args(["new", "trunk()"])
            .current_dir(target_dir)
            .status()
            .context("Failed to run jj new trunk()")?;

        if !status.success() {
            bail!("jj new trunk() failed");
        }

        let status = Command::new("jj")
            .args(["bookmark", "create", branch_name])
            .current_dir(target_dir)
            .status()
            .context("Failed to create bookmark")?;

        if !status.success() {
            bail!("jj bookmark create {} failed", branch_name);
        }
    }

    Ok(())
}

/// Do a full (deep) clone at target_dir
fn do_deep_clone(source_repo_path: &Path, target_dir: &Path, branch_name: &str) -> Result<()> {
    let remote_url = get_remote_url(source_repo_path)?;

    println!("Deep cloning {} to {}...", remote_url, target_dir.display());
    let status = Command::new("jj")
        .args([
            "git",
            "clone",
            "--colocate",
            &remote_url,
            target_dir.to_str().unwrap(),
        ])
        .status()
        .context("Failed to run jj git clone")?;

    if !status.success() {
        bail!("jj git clone failed");
    }

    // Navigate to the bookmark
    let local_exists = bookmark_exists_local(target_dir, branch_name);
    let remote_exists = bookmark_exists_remote(target_dir, branch_name);

    if local_exists || remote_exists {
        if remote_exists && !local_exists {
            Command::new("jj")
                .args(["bookmark", "track", &format!("{}@origin", branch_name)])
                .current_dir(target_dir)
                .status()
                .context("Failed to track remote bookmark")?;
        }

        let status = Command::new("jj")
            .args(["new", branch_name])
            .current_dir(target_dir)
            .status()
            .context("Failed to run jj new")?;

        if !status.success() {
            bail!("jj new {} failed", branch_name);
        }
    } else {
        println!(
            "Bookmark '{}' doesn't exist, creating new bookmark...",
            branch_name
        );
        let status = Command::new("jj")
            .args(["new", "trunk()"])
            .current_dir(target_dir)
            .status()
            .context("Failed to run jj new trunk()")?;

        if !status.success() {
            bail!("jj new trunk() failed");
        }

        let status = Command::new("jj")
            .args(["bookmark", "create", branch_name])
            .current_dir(target_dir)
            .status()
            .context("Failed to create bookmark")?;

        if !status.success() {
            bail!("jj bookmark create {} failed", branch_name);
        }
    }

    // Initialize submodules if present (still uses git — jj doesn't handle submodules)
    let gitmodules = target_dir.join(".gitmodules");
    if gitmodules.exists() {
        println!("Initializing submodules...");
        let status = Command::new("git")
            .args(["submodule", "update", "--init", "--recursive"])
            .current_dir(target_dir)
            .status()
            .context("Failed to init submodules")?;

        if !status.success() {
            eprintln!("Warning: submodule initialization failed");
        }
    }

    Ok(())
}

/// Migrate an existing trees directory structure to use branch.toml
fn cmd_migrate(trees_path: &Path, dry_run: bool) -> Result<()> {
    let trees_path = trees_path
        .canonicalize()
        .with_context(|| format!("Failed to resolve path: {}", trees_path.display()))?;

    if !trees_path.is_dir() {
        bail!("Not a directory: {}", trees_path.display());
    }

    if dry_run {
        println!("Dry run - no changes will be made\n");
    }

    let mut found_branches = Vec::new();
    find_branch_dirs(&trees_path, &trees_path, &mut found_branches)?;

    if found_branches.is_empty() {
        println!("No git repositories found under {}", trees_path.display());
        return Ok(());
    }

    for (branch_dir, branch_name) in &found_branches {
        let toml_path = branch_dir.join("branch.toml");

        if toml_path.exists() {
            println!("[skip] {} (branch.toml already exists)", branch_name);
            continue;
        }

        if dry_run {
            println!("[would create] {} -> {}", branch_name, toml_path.display());
        } else {
            let config = BranchConfig {
                branch: branch_name.clone(),
            };
            let toml_content =
                toml::to_string_pretty(&config).context("Failed to serialize config")?;
            fs::write(&toml_path, toml_content)
                .with_context(|| format!("Failed to write {}", toml_path.display()))?;
            println!("[created] {} -> {}", branch_name, toml_path.display());
        }
    }

    Ok(())
}

/// Recursively find directories that should have branch.toml
/// A branch directory is the parent of a directory containing .git or .jj
fn find_branch_dirs(
    base_path: &Path,
    current_path: &Path,
    results: &mut Vec<(PathBuf, String)>,
) -> Result<()> {
    let entries = fs::read_dir(current_path)
        .with_context(|| format!("Failed to read directory: {}", current_path.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if !path.is_dir() {
            continue;
        }

        let name = entry.file_name();
        // Skip hidden directories
        if name.to_string_lossy().starts_with('.') {
            continue;
        }

        // Check if this directory contains a .git or .jj (repo or workspace)
        let has_git = path.join(".git").exists();
        let has_jj = path.join(".jj").exists();
        if has_git || has_jj {
            // This is a repo - the branch name is the path from base to parent
            let branch_dir = current_path;
            let branch_name = branch_dir
                .strip_prefix(base_path)
                .unwrap_or(branch_dir.as_ref())
                .to_string_lossy()
                .to_string();

            // Only add if we haven't already added this branch dir
            if !results.iter().any(|(dir, _)| dir == branch_dir) {
                if branch_name.is_empty() {
                    // Repo is directly in trees_path, skip
                    continue;
                }
                results.push((branch_dir.to_path_buf(), branch_name));
            }
        } else {
            // Recurse into subdirectory
            find_branch_dirs(base_path, &path, results)?;
        }
    }

    Ok(())
}

/// Generate a remote git+ssh Nix URL for a sibling repo
fn cmd_nix_remote(repo: &str) -> Result<()> {
    let toml_path =
        find_branch_toml().context("No branch.toml found in current or parent directories")?;
    let config = read_branch_config(&toml_path)?;
    let branch_name = &config.branch;
    let branch_dir = toml_path.parent().unwrap();

    // Find the sibling repo
    let repo_path = branch_dir.join(repo);
    if !repo_path.exists() {
        bail!("Repo '{}' not found at {}", repo, repo_path.display());
    }

    let remote_url = get_remote_url(&repo_path)?;
    let nix_url = convert_to_nix_ssh_url(&remote_url, branch_name)?;
    print!("{}", nix_url);

    Ok(())
}

/// Convert a git remote URL to a Nix git+ssh URL
fn convert_to_nix_ssh_url(remote_url: &str, branch: &str) -> Result<String> {
    // Handle git@github.com:owner/repo.git format
    if let Some(rest) = remote_url.strip_prefix("git@github.com:") {
        let repo_path = rest.strip_suffix(".git").unwrap_or(rest);
        return Ok(format!(
            "git+ssh://git@github.com/{}?ref={}",
            repo_path, branch
        ));
    }

    // Handle ssh://git@github.com/owner/repo.git format
    if let Some(rest) = remote_url.strip_prefix("ssh://git@github.com/") {
        let repo_path = rest.strip_suffix(".git").unwrap_or(rest);
        return Ok(format!(
            "git+ssh://git@github.com/{}?ref={}",
            repo_path, branch
        ));
    }

    // Handle https://github.com/owner/repo.git format
    if let Some(rest) = remote_url.strip_prefix("https://github.com/") {
        let repo_path = rest.strip_suffix(".git").unwrap_or(rest);
        return Ok(format!(
            "git+ssh://git@github.com/{}?ref={}",
            repo_path, branch
        ));
    }

    bail!("Unsupported remote URL format: {}", remote_url);
}

/// Generate a local git+file Nix URL for a sibling repo
fn cmd_nix_local(repo: &str) -> Result<()> {
    let toml_path =
        find_branch_toml().context("No branch.toml found in current or parent directories")?;
    let config = read_branch_config(&toml_path)?;
    let branch_dir = toml_path.parent().unwrap();

    // Find the sibling repo
    let repo_path = branch_dir.join(repo);
    if !repo_path.exists() {
        bail!("Repo '{}' not found at {}", repo, repo_path.display());
    }

    if repo_path.join(".git").exists() {
        // Deep clone or colocated repo — has .git, use directly
        let canonical_path = repo_path
            .canonicalize()
            .with_context(|| format!("Failed to resolve path: {}", repo_path.display()))?;
        print!("git+file://{}", canonical_path.display());
    } else {
        // jj workspace — no .git, point to the source repo with ?ref=
        let remote_url = get_remote_url(&repo_path)?;
        let (namespace, repo_name) = parse_github_url(&remote_url)?;
        let home = env::var("HOME").context("HOME not set")?;
        let source_path = PathBuf::from(home)
            .join("sources")
            .join(&namespace)
            .join(&repo_name);

        if !source_path.exists() {
            bail!(
                "Source repo not found at {} (expected for workspace {})",
                source_path.display(),
                repo_path.display()
            );
        }

        let canonical_path = source_path
            .canonicalize()
            .with_context(|| format!("Failed to resolve path: {}", source_path.display()))?;
        print!(
            "git+file://{}?ref={}",
            canonical_path.display(),
            config.branch
        );
    }

    Ok(())
}

/// Print the directory containing branch.toml
fn cmd_root() -> Result<()> {
    match find_branch_toml() {
        Some(toml_path) => {
            let root_dir = toml_path.parent().unwrap();
            print!("{}", root_dir.display());
            Ok(())
        }
        None => {
            bail!("No branch.toml found in current or parent directories");
        }
    }
}

/// Show the current branch (when no subcommand given)
fn cmd_show_branch() -> Result<()> {
    match find_branch_toml() {
        Some(toml_path) => {
            let config = read_branch_config(&toml_path)?;
            print!("{}", config.branch);
            Ok(())
        }
        None => {
            print_usage();
            Ok(())
        }
    }
}

fn print_usage() {
    eprintln!("branch - A utility for managing jj workspaces across multiple repos");
    eprintln!();
    eprintln!("USAGE:");
    eprintln!("    branch                              Show current branch (from branch.toml)");
    eprintln!("    branch new <branch_name>            Create a new branch directory");
    eprintln!("    branch clone <ns> <repo>            Clone repo as jj workspace");
    eprintln!("    branch clone --deep <ns> <repo>     Clone repo fully (for submodules)");
    eprintln!("    branch migrate <trees_path>         Migrate existing structure to branch.toml");
    eprintln!("    branch migrate --dry-run <path>     Preview migration without changes");
    eprintln!("    branch root                         Print directory containing branch.toml");
    eprintln!("    branch nix remote <repo>            Generate git+ssh Nix URL for repo");
    eprintln!("    branch nix local <repo>             Generate git+file Nix URL for repo");
    eprintln!();
    eprintln!("Run 'branch --help' for more information.");
}
