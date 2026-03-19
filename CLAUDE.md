# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build Commands

```bash
cargo build --release    # Build release binary
cargo build              # Build debug binary
rustfmt src/main.rs      # Format code
```

## Architecture

Single-file Rust CLI (`src/main.rs`) using clap for argument parsing. The tool manages git worktrees across multiple repos, organized by branch name.

**Key concepts:**
- `branch.toml` - Config file storing the branch name, placed in each branch workspace directory
- Source repos cached at `~/sources/<namespace>/<repo>`
- Worktrees/clones created as siblings to `branch.toml`

**Command structure:**
- Subcommands defined via clap's derive macros (`Commands` enum, `NixCommands` nested enum)
- `find_branch_toml()` traverses up directories to locate config
- Git operations use `std::process::Command` rather than libgit2

**Main flows:**
- `clone` → ensures source repo exists → creates worktree or deep clone → checks out branch
- `nix remote/local` → finds sibling repo → extracts remote URL → converts to Nix flake format
