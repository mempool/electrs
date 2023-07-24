use std::{path::Path, process::Command};

fn main() {
    // Specify re-run conditions

    // 1. Rerun build script if we pass a new GIT_HASH
    println!("cargo:rerun-if-env-changed=GIT_HASH");

    // 2. Only do git based reruns if git directory exists
    if Path::new(".git").exists() {
        // If we change the branch, rerun
        println!("cargo:rerun-if-changed=.git/HEAD");
        if let Ok(r) = std::fs::read_to_string(".git/HEAD") {
            if let Some(stripped) = r.strip_prefix("ref: ") {
                // If the HEAD is detached it will be a commit hash
                // so the HEAD changed directive above will pick it up,
                // otherwise it will point to a ref in the refs directory
                println!("cargo:rerun-if-changed=.git/{}", stripped);
            }
        }
    }

    // Getting git hash

    // Don't fetch git hash if it's already in the ENV
    let existing = std::env::var("GIT_HASH").unwrap_or_else(|_| String::new());
    if !existing.is_empty() {
        return;
    }

    // Get git hash from git and don't do anything if the command fails
    if let Some(rev_parse) = cmd("git", &["rev-parse", "--short", "HEAD"]) {
        // Add (dirty) to the GIT_HASH if the git status isn't clean
        // This includes untracked files
        let dirty = cmd("git", &["status", "--short"]).expect("git command works");

        let git_hash = if dirty.is_empty() {
            rev_parse
        } else {
            format!("{}(dirty)", rev_parse.trim())
        };

        println!("cargo:rustc-env=GIT_HASH={}", git_hash.trim());
    }
}

// Helper function, Command is verbose...
fn cmd(name: &str, args: &[&str]) -> Option<String> {
    Command::new(name).args(args).output().ok().and_then(|o| {
        if o.status.success() {
            String::from_utf8(o.stdout).ok()
        } else {
            None
        }
    })
}
