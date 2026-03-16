# branch

A CLI utility for managing git worktrees across multiple repositories, organized by branch name.

## Concept

When working on a feature that spans multiple repositories, `branch` helps you maintain the same branch name across all of them. It organizes your work into a directory structure like:

```
~/trees/
  dhruv/make_repo_better/
    branch.toml              # stores branch name
    repo-a/                  # worktree checked out to dhruv/make_repo_better
    repo-b/                  # worktree checked out to dhruv/make_repo_better
  feature/auth/
    branch.toml
    frontend/
    backend/
```

Source repositories are cached in `~/sources/<namespace>/<repo>` and worktrees are created from them.

## Installation

```bash
cargo build --release
cp target/release/branch ~/.local/bin/  # or anywhere in your PATH
```

## Usage

### Create a new branch workspace

```bash
cd ~/trees
branch new dhruv/make_repo_better
```

Creates `~/trees/dhruv/make_repo_better/branch.toml` containing the branch name.

### Clone a repository

```bash
cd ~/trees/dhruv/make_repo_better
branch clone dgramop-specter branch
```

This will:
1. Clone `git@github.com:anthropics/claude-code.git` to `~/sources/anthropics/claude-code` (if not already present)
2. Create a worktree at `~/trees/dhruv/make_repo_better/claude-code` checked out to branch `dhruv/make_repo_better`
3. If the branch doesn't exist, it creates it from the default branch

### Deep clone (for submodules)

Git worktrees don't always play well with submodules. Use `--deep` for a full clone:

```bash
branch clone --deep dgramop-specter branch
```

This does a full `git clone` instead of a worktree and initializes submodules recursively.

### Show current branch

```bash
cd ~/trees/dhruv/make_repo_better/branch
branch
# Output: dhruv/make_repo_better
```

Traverses up to find `branch.toml` and prints the branch name.

Useful for command output injection in editors like vim or helix, in my use case for pointing flake pins to my branch

### Get branch root directory

```bash
cd ~/trees/dhruv/make_repo_better/branch/src
branch root
# Output: /home/user/trees/dhruv/make_repo_better
```

Returns the directory containing `branch.toml`. Useful for scripts that need to reference sibling repos.

### Migrate existing structure

If you already have a trees-like directory structure without `branch.toml` files:

```bash
branch migrate ~/trees --dry-run  # preview changes
branch migrate ~/trees            # create branch.toml files
```

The migration detects git repositories (by the presence of `.git`) and creates `branch.toml` in their parent directories.

## Commands

| Command | Description |
|---------|-------------|
| `branch` | Print current branch name (from nearest `branch.toml`) |
| `branch new <name>` | Create a new branch directory with `branch.toml` |
| `branch clone <ns> <repo>` | Clone repo as a worktree |
| `branch clone --deep <ns> <repo>` | Clone repo fully (for submodules) |
| `branch root` | Print directory containing `branch.toml` |
| `branch migrate <path>` | Add `branch.toml` to existing structure |
| `branch migrate --dry-run <path>` | Preview migration |
| `branch -h` | Show help |

## How it works

- **Source repos**: Cached at `~/sources/<namespace>/<repo>`, cloned via SSH
- **Worktrees**: Created from source repos, checked out to the branch specified in `branch.toml`
- **Branch creation**: If the branch doesn't exist locally or remotely, it's created from the default branch
- **Deep clones**: Full clones with submodule initialization for repos that don't work well as worktrees
