use std::path::PathBuf;
use std::process::Command;

/// Records which commit this binary was built from, and where the checkout lives.
///
/// This is what lets the app notice it is out of date **without a network request**.
/// "No update checks" is a non-negotiable — an updater that polls a server on launch is
/// the behaviour this app exists to avoid. Comparing a compile-time commit against the
/// checkout already on disk answers the same question locally.
fn main() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(PathBuf::from)
        .unwrap_or_default();

    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&repo)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default();

    println!("cargo:rustc-env=HEARSAY_BUILT_COMMIT={commit}");
    println!("cargo:rustc-env=HEARSAY_REPO_DIR={}", repo.display());
    // Rebuild when the checked-out commit changes, so the stamp never goes stale.
    println!("cargo:rerun-if-changed={}", repo.join(".git/HEAD").display());

    tauri_build::build()
}
