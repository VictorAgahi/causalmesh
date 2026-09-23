use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads/");
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output();
    let git_hash = match output {
        Ok(out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
        _ => "dev".to_string(),
    };
    println!("cargo:rustc-env=GIT_HASH={git_hash}");
}
