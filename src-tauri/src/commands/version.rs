//! Noticing that the installed app is older than the checkout.
//!
//! Deliberately **not** an update check in the usual sense: nothing is fetched, no server
//! is contacted, and there is no version endpoint. The binary carries the commit it was
//! built from, and this compares that against the repository already on this machine —
//! the same repository the app needs anyway for transcription.
//!
//! So the flow for someone using a shared checkout is: `git pull`, and the next launch
//! says the app is behind and how to fix it. Nothing phones home to discover that.

use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Serialize)]
pub struct UpdateStatus {
    /// The checkout is ahead of what this binary was built from.
    pub behind: bool,
    /// How many commits behind, when that can be determined.
    pub commits: u32,
    /// Short commit this binary was built from.
    pub built: String,
    /// Short commit the checkout is on now.
    pub available: String,
    /// Where to run `./install.sh`.
    pub repo: String,
}

impl UpdateStatus {
    fn unknown() -> Self {
        Self {
            behind: false,
            commits: 0,
            built: String::new(),
            available: String::new(),
            repo: String::new(),
        }
    }
}

fn short(commit: &str) -> String {
    commit.chars().take(7).collect()
}

fn git(repo: &PathBuf, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Whether a newer build is available from the local checkout.
///
/// Silent about every failure. A missing checkout, a missing `git`, a build made outside
/// a repository — none of those are problems the user needs to hear about, and reporting
/// "unknown" as "out of date" would be worse than saying nothing.
#[tauri::command]
pub fn update_status() -> UpdateStatus {
    let built = env!("HEARSAY_BUILT_COMMIT").to_string();
    let repo = PathBuf::from(env!("HEARSAY_REPO_DIR"));

    if built.is_empty() || !repo.join(".git").exists() {
        return UpdateStatus::unknown();
    }

    let Some(current) = git(&repo, &["rev-parse", "HEAD"]) else {
        return UpdateStatus::unknown();
    };
    if current.is_empty() || current == built {
        return UpdateStatus::unknown();
    }

    // Only count it as behind if the built commit is genuinely an ancestor. A checkout
    // sitting on an older branch is not an update, and shouldn't be reported as one.
    let is_ancestor = Command::new("git")
        .args(["merge-base", "--is-ancestor", &built, &current])
        .current_dir(&repo)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !is_ancestor {
        return UpdateStatus::unknown();
    }

    let commits = git(&repo, &["rev-list", "--count", &format!("{built}..{current}")])
        .and_then(|n| n.parse::<u32>().ok())
        .unwrap_or(0);

    UpdateStatus {
        behind: true,
        commits,
        built: short(&built),
        available: short(&current),
        repo: repo.display().to_string(),
    }
}
