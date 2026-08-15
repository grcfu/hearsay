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
    //
    // Watching `.git/HEAD` alone does not do that. While a branch is checked out HEAD
    // holds the text `ref: refs/heads/<branch>`, and committing does not change it —
    // the new commit is written to the ref file HEAD points at. So HEAD on its own
    // misses every commit made on the current branch, which is the ordinary case, and
    // the binary goes on reporting whatever commit it first compiled against. That
    // shows up as the app claiming to be out of date immediately after being rebuilt
    // from the very commit it is complaining about.
    let git_dir = repo.join(".git");
    println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());

    // A detached HEAD stores the commit in HEAD itself, so it is already covered above.
    if let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) {
        if let Some(reference) = head.trim().strip_prefix("ref:") {
            println!(
                "cargo:rerun-if-changed={}",
                git_dir.join(reference.trim()).display()
            );
        }
    }

    // Once git packs refs the loose file above disappears and the branch tip lives here
    // instead. Only watched when it exists: naming a missing path makes cargo treat the
    // build script as dirty on every single build.
    let packed_refs = git_dir.join("packed-refs");
    if packed_refs.is_file() {
        println!("cargo:rerun-if-changed={}", packed_refs.display());
    }

    tauri_build::build()
}
