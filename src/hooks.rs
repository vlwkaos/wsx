// Post-create hooks and .env file copying (ported from gtr).

use anyhow::{bail, Context, Result};
use glob::glob;
use std::path::Path;
use std::process::{Command, Stdio};
use crate::model::workspace::ProjectConfig;

pub fn copy_env_files(src: &Path, dest: &Path, config: &ProjectConfig) -> Result<()> {
    for pattern in &config.copy_includes {
        let full_pattern = src.join(pattern);
        let full_pattern_str = full_pattern.to_string_lossy();

        for entry in glob(&full_pattern_str).context("invalid glob pattern")? {
            let src_file = entry?;
            let rel = src_file.strip_prefix(src)?;

            let excluded = config.copy_excludes.iter().any(|ex| {
                let ex_pattern = src.join(ex);
                glob(&ex_pattern.to_string_lossy())
                    .ok()
                    .and_then(|mut g| g.next())
                    .and_then(|r| r.ok())
                    .map(|p| p == src_file)
                    .unwrap_or(false)
            });
            if excluded { continue; }

            let dest_file = dest.join(rel);
            if let Some(parent) = dest_file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&src_file, &dest_file)
                .with_context(|| format!("copying {} to {}", src_file.display(), dest_file.display()))?;
        }
    }
    Ok(())
}

pub fn run_post_create(dir: &Path, cmd: &str) -> Result<()> {
    let status = Command::new("sh")
        .arg("-c").arg(cmd)
        .current_dir(dir)
        .stdout(Stdio::null()).stderr(Stdio::null())
        .status()
        .with_context(|| format!("running postCreate: {}", cmd))?;
    if !status.success() { bail!("postCreate hook exited {}", status); }
    Ok(())
}
