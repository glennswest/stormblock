//! Stamps the commit this binary was built from (#466), so a running engine
//! can say which build it is: the crate version moves only at a tag, and every
//! golden between two tags reported the same one.
//!
//! `STORMBLOCK_COMMIT` from the environment wins (a build from a tree with no
//! `.git`); else `git rev-parse --short=12 HEAD`; else `unknown`.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=STORMBLOCK_COMMIT");
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let commit = std::env::var("STORMBLOCK_COMMIT")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| git(&["rev-parse", "--short=12", "HEAD"]))
        .unwrap_or_else(|| "unknown".into());
    // A new commit moves HEAD or the ref it names; rebuild the stamp then.
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
            println!("cargo:rerun-if-changed={dir}/{r}");
        }
        println!("cargo:rerun-if-changed={dir}/packed-refs");
    }
    println!("cargo:rustc-env=STORMBLOCK_COMMIT={commit}");
}
