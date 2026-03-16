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
#[command(about = "A utility for managing git worktrees across multiple repos")]
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
    /// Clone a repo as a worktree (or full clone with --deep)
    Clone {
        /// GitHub namespace (e.g., "anthropics")
        namespace: String,
        /// GitHub repo name (e.g., "claude-code")
        repo: String,
        /// Do a full clone instead of a worktree
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

/// Clone a repo as a worktree or full clone
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
        // Worktree
        do_worktree(&source_repo_path, &target_dir, branch_name)?;
    }

    println!("Cloned {} to {}", repo, target_dir.display());
    println!("Branch: {}", branch_name);

    Ok(())
}

/// Ensure the source repo exists at ~/sources/<namespace>/<repo>
fn ensure_source_repo(source_repo_path: &Path, namespace: &str, repo: &str) -> Result<()> {
    if source_repo_path.exists() {
        // Repo already exists, fetch latest
        println!("Source repo exists, fetching latest...");
        let status = Command::new("git")
            .args(["fetch", "--all"])
            .current_dir(source_repo_path)
            .status()
            .context("Failed to run git fetch")?;
        if !status.success() {
            bail!("git fetch failed");
        }
        return Ok(());
    }

    // Create parent directories
    let parent = source_repo_path.parent().unwrap();
    fs::create_dir_all(parent).with_context(|| format!("Failed to create {}", parent.display()))?;

    // Clone the repo using SSH
    let ssh_url = format!("git@github.com:{}/{}.git", namespace, repo);
    println!("Cloning {} to {}...", ssh_url, source_repo_path.display());

    let status = Command::new("git")
        .args(["clone", &ssh_url, source_repo_path.to_str().unwrap()])
        .status()
        .context("Failed to run git clone")?;

    if !status.success() {
        bail!("git clone failed");
    }

    Ok(())
}

/// Create a worktree at target_dir for the given branch
fn do_worktree(source_repo_path: &Path, target_dir: &Path, branch_name: &str) -> Result<()> {
    // Check if branch exists remotely
    let branch_exists = Command::new("git")
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{}", branch_name),
        ])
        .current_dir(source_repo_path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    let remote_branch_exists = Command::new("git")
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/remotes/origin/{}", branch_name),
        ])
        .current_dir(source_repo_path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if branch_exists {
        // Branch exists locally, create worktree
        let status = Command::new("git")
            .args(["worktree", "add", target_dir.to_str().unwrap(), branch_name])
            .current_dir(source_repo_path)
            .status()
            .context("Failed to run git worktree add")?;

        if !status.success() {
            bail!("git worktree add failed");
        }
    } else if remote_branch_exists {
        // Branch exists on remote, create worktree tracking it
        let status = Command::new("git")
            .args([
                "worktree",
                "add",
                "--track",
                "-b",
                branch_name,
                target_dir.to_str().unwrap(),
                &format!("origin/{}", branch_name),
            ])
            .current_dir(source_repo_path)
            .status()
            .context("Failed to run git worktree add")?;

        if !status.success() {
            bail!("git worktree add failed");
        }
    } else {
        // Branch doesn't exist, create new branch from default branch
        // First, find the default branch
        let output = Command::new("git")
            .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
            .current_dir(source_repo_path)
            .output()
            .context("Failed to get default branch")?;

        let default_branch = if output.status.success() {
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .strip_prefix("refs/remotes/origin/")
                .unwrap_or("master")
                .to_string()
        } else {
            "master".to_string()
        };

        println!(
            "Branch '{}' doesn't exist, creating from '{}'...",
            branch_name, default_branch
        );

        let status = Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                branch_name,
                target_dir.to_str().unwrap(),
                &format!("origin/{}", default_branch),
            ])
            .current_dir(source_repo_path)
            .status()
            .context("Failed to run git worktree add")?;

        if !status.success() {
            bail!("git worktree add failed");
        }
    }

    Ok(())
}

/// Do a full (deep) clone at target_dir
fn do_deep_clone(source_repo_path: &Path, target_dir: &Path, branch_name: &str) -> Result<()> {
    // Get the remote URL from the source repo
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(source_repo_path)
        .output()
        .context("Failed to get remote URL")?;

    if !output.status.success() {
        bail!("Failed to get remote URL from source repo");
    }

    let remote_url = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Clone the repo
    println!("Deep cloning {} to {}...", remote_url, target_dir.display());
    let status = Command::new("git")
        .args(["clone", &remote_url, target_dir.to_str().unwrap()])
        .status()
        .context("Failed to run git clone")?;

    if !status.success() {
        bail!("git clone failed");
    }

    // Check if branch exists
    let branch_exists = Command::new("git")
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/remotes/origin/{}", branch_name),
        ])
        .current_dir(target_dir)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if branch_exists {
        // Checkout the branch
        let status = Command::new("git")
            .args([
                "checkout",
                "-b",
                branch_name,
                &format!("origin/{}", branch_name),
            ])
            .current_dir(target_dir)
            .status()
            .context("Failed to checkout branch")?;

        if !status.success() {
            // Maybe the branch exists locally already
            let status = Command::new("git")
                .args(["checkout", branch_name])
                .current_dir(target_dir)
                .status()
                .context("Failed to checkout branch")?;

            if !status.success() {
                bail!("Failed to checkout branch {}", branch_name);
            }
        }
    } else {
        // Create the branch
        println!(
            "Branch '{}' doesn't exist remotely, creating new branch...",
            branch_name
        );
        let status = Command::new("git")
            .args(["checkout", "-b", branch_name])
            .current_dir(target_dir)
            .status()
            .context("Failed to create branch")?;

        if !status.success() {
            bail!("Failed to create branch {}", branch_name);
        }
    }

    // Initialize submodules if present
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
/// A branch directory is the parent of a directory containing .git
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

        // Check if this directory contains a .git (worktree or full clone)
        let git_path = path.join(".git");
        if git_path.exists() {
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

    // Get the remote origin URL
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(&repo_path)
        .output()
        .context("Failed to run git remote get-url")?;

    if !output.status.success() {
        bail!("Failed to get remote URL for {}", repo);
    }

    let remote_url = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Convert git@github.com:owner/repo.git to git+ssh://git@github.com/owner/repo
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
    let branch_dir = toml_path.parent().unwrap();

    // Find the sibling repo
    let repo_path = branch_dir.join(repo);
    if !repo_path.exists() {
        bail!("Repo '{}' not found at {}", repo, repo_path.display());
    }

    let canonical_path = repo_path
        .canonicalize()
        .with_context(|| format!("Failed to resolve path: {}", repo_path.display()))?;

    print!("git+file://{}", canonical_path.display());

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
    eprintln!("branch - A utility for managing git worktrees across multiple repos");
    eprintln!();
    eprintln!("USAGE:");
    eprintln!("    branch                              Show current branch (from branch.toml)");
    eprintln!("    branch new <branch_name>            Create a new branch directory");
    eprintln!("    branch clone <ns> <repo>            Clone repo as worktree");
    eprintln!("    branch clone --deep <ns> <repo>     Clone repo fully (for submodules)");
    eprintln!("    branch migrate <trees_path>         Migrate existing structure to branch.toml");
    eprintln!("    branch migrate --dry-run <path>     Preview migration without changes");
    eprintln!("    branch root                         Print directory containing branch.toml");
    eprintln!("    branch nix remote <repo>            Generate git+ssh Nix URL for repo");
    eprintln!("    branch nix local <repo>             Generate git+file Nix URL for repo");
    eprintln!();
    eprintln!("Run 'branch --help' for more information.");
}
