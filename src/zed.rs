use crate::{
    bail,
    error::{Context, Result},
    scanner::safe_join,
};
use std::{
    env,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

fn executable_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(configured) = env::var_os("ZED_CLI_PATH") {
        candidates.push(configured.into());
    }
    #[cfg(target_os = "macos")]
    {
        candidates.push("/Applications/Zed.app/Contents/MacOS/cli".into());
        if let Some(home) = env::var_os("HOME") {
            candidates.push(PathBuf::from(home).join("Applications/Zed.app/Contents/MacOS/cli"));
        }
        candidates.push("/opt/homebrew/bin/zed".into());
        candidates.push("/usr/local/bin/zed".into());
    }
    if let Some(path) = env::var_os("PATH") {
        for directory in env::split_paths(&path) {
            candidates.push(directory.join(if cfg!(windows) { "zed.exe" } else { "zed" }));
        }
    }
    candidates
}

pub fn find_zed_cli() -> Option<PathBuf> {
    executable_candidates()
        .into_iter()
        .find(|candidate| candidate.is_file())
}

pub fn open_file_pair(left_root: &Path, right_root: &Path, relative_path: &Path) -> Result<()> {
    let left = safe_join(left_root, relative_path)?;
    let right = safe_join(right_root, relative_path)?;
    if !left.exists() || !right.exists() {
        bail!("both files must exist before Zed can open its native two-file diff");
    }
    let executable = find_zed_cli()
        .context("could not find the Zed CLI; run ‘zed: install cli’ or set ZED_CLI_PATH")?;
    Command::new(executable)
        .arg("--diff")
        .arg(left)
        .arg(right)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("could not start the Zed CLI")?;
    Ok(())
}
