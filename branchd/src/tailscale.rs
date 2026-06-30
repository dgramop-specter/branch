use anyhow::{Context, Result};
use std::net::IpAddr;
use std::process::Command;

/// Run `tailscale ip -4` and parse the first address.
pub fn detect_ip() -> Result<IpAddr> {
    let output = Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
        .context("spawn tailscale")?;
    if !output.status.success() {
        anyhow::bail!(
            "tailscale ip -4 failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .next()
        .context("tailscale ip -4 returned no output")?
        .trim();
    let ip: IpAddr = line
        .parse()
        .with_context(|| format!("parse tailscale ip {line:?}"))?;
    Ok(ip)
}
