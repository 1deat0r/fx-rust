//! `fxrs upgrade`: check the latest release of fxrs and optionally rebuild
//! from source. Faithful to fx's self-update spirit (upstream auto-upgrades
//! from its release channel); since fxrs is a from-source Rust port, the
//! practical upgrade path is `cargo install --git`.

use anyhow::{Context, Result};

pub fn latest_release() -> Result<Option<String>> {
    let url = "https://api.github.com/repos/1deat0r/fx-rust/releases/latest";
    let body = ureq::get(url)
        .set("User-Agent", "fxrs-upgrade")
        .timeout(std::time::Duration::from_secs(15))
        .call()
        .context("checking GitHub releases")?
        .into_string()?;
    let v: serde_json::Value = serde_json::from_str(&body).context("parsing release")?;
    Ok(v.get("tag_name").and_then(|t| t.as_str()).map(|s| s.to_string()))
}

pub fn install_from_git() -> Result<()> {
    let status = std::process::Command::new("cargo")
        .args(["install", "--git", "https://github.com/1deat0r/fx-rust", "--force"])
        .status()
        .context("running cargo install")?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("cargo install failed with {status:?}; check the error above")
    }
}

pub fn version_tag() -> String {
    format!("v{}", crate::version::VERSION)
}
