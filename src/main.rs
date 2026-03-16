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
    eprintln!("    branch                          Show current branch (from branch.toml)");
    eprintln!("    branch new <branch_name>        Create a new branch directory");
    eprintln!("    branch clone <ns> <repo>        Clone repo as worktree");
    eprintln!("    branch clone --deep <ns> <repo> Clone repo fully (for submodules)");
    eprintln!();
    eprintln!("Run 'branch --help' for more information.");
}
