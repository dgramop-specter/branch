use anyhow::{Context, Result};
use clap::Parser;
use std::fs;
use std::net::IpAddr;
use std::path::PathBuf;

mod discovery;
mod gh;
mod jj;
mod tailscale;
mod web;

#[derive(Parser, Debug)]
#[command(name = "branchd")]
#[command(about = "Daemon that lists jj-spr PR stacks across branch workspaces")]
#[command(version)]
struct Cli {
    /// Root directory to scan for branch.toml workspaces.
    #[arg(long, default_value = "~/trees")]
    root: String,

    /// Root directory holding canonical `<ns>/<repo>` source clones.
    #[arg(long, default_value = "~/sources")]
    sources: String,

    /// Address to bind. Pass once per address. Default: tailscale0 IP +
    /// 127.0.0.1. Explicit `--bind` replaces the default set.
    #[arg(long, action = clap::ArgAction::Append)]
    bind: Vec<IpAddr>,

    /// Port to listen on. Binding to <1024 needs CAP_NET_BIND_SERVICE.
    #[arg(long, default_value_t = 80)]
    port: u16,

    /// Fork into background. Writes pid + logs to ~/.local/state/branchd/.
    #[arg(long)]
    daemon: bool,

    /// Run in foreground (default if --daemon not given).
    #[arg(long, conflicts_with = "daemon")]
    foreground: bool,
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(stripped) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(stripped);
        }
    }
    PathBuf::from(s)
}

fn state_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let dir = PathBuf::from(home).join(".local/state/branchd");
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(dir)
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let root = expand_tilde(&cli.root);
    if !root.exists() {
        anyhow::bail!("root does not exist: {}", root.display());
    }
    let sources = expand_tilde(&cli.sources);

    let bind_ips: Vec<IpAddr> = if cli.bind.is_empty() {
        let mut v = Vec::new();
        match tailscale::detect_ip() {
            Ok(ip) => v.push(ip),
            Err(e) => eprintln!("tailscale ip lookup failed ({e}); skipping tailscale bind"),
        }
        v.push(IpAddr::from([127, 0, 0, 1]));
        v
    } else {
        cli.bind
    };
    let addrs: Vec<std::net::SocketAddr> = bind_ips
        .iter()
        .map(|&ip| std::net::SocketAddr::new(ip, cli.port))
        .collect();

    if cli.daemon {
        let dir = state_dir()?;
        let pid_file = dir.join("branchd.pid");
        let log_file = dir.join("branchd.log");
        let err_file = dir.join("branchd.err");
        let stdout = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_file)
            .with_context(|| format!("open {}", log_file.display()))?;
        let stderr = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&err_file)
            .with_context(|| format!("open {}", err_file.display()))?;

        eprintln!("daemonizing; logs at {}", log_file.display());
        for addr in &addrs {
            eprintln!("listening on http://{}", addr);
        }

        daemonize::Daemonize::new()
            .pid_file(&pid_file)
            .working_directory(&dir)
            .stdout(stdout)
            .stderr(stderr)
            .start()
            .context("daemonize failed")?;
    } else {
        for addr in &addrs {
            eprintln!("listening on http://{}", addr);
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    rt.block_on(web::serve(addrs, root, sources))
}
