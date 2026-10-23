//! Build information for Settings → App: the commit and when it was built.
//! Nix builds have no `.git` and pass `ORIEL_GIT_SHA`/`ORIEL_BUILD_DATE`.

use std::{path::Path, process::Command};

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let git = root.join(".git");
    // A new commit or staged change rebuilds this; edits alone do not.
    for f in ["HEAD", "index"] {
        println!("cargo:rerun-if-changed={}", git.join(f).display());
    }
    if let Ok(head) = std::fs::read_to_string(git.join("HEAD"))
        && let Some(r) = head.strip_prefix("ref: ")
    {
        println!("cargo:rerun-if-changed={}", git.join(r.trim()).display());
    }
    println!("cargo:rerun-if-env-changed=ORIEL_GIT_SHA");
    println!("cargo:rerun-if-env-changed=ORIEL_BUILD_DATE");

    let sha = std::env::var("ORIEL_GIT_SHA").ok().or_else(|| {
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        };
        let sha = run(&["rev-parse", "--short=10", "HEAD"])?;
        let dirty =
            run(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
        Some(if dirty { format!("{sha}-dirty") } else { sha })
    });
    let date = std::env::var("ORIEL_BUILD_DATE").unwrap_or_else(|_| {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        utc(secs)
    });
    println!(
        "cargo:rustc-env=ORIEL_GIT_SHA={}",
        sha.unwrap_or_else(|| "unknown".into())
    );
    println!("cargo:rustc-env=ORIEL_BUILD_DATE={date}");
}

/// "YYYY-MM-DD HH:MM UTC" (civil-from-days, Howard Hinnant's algorithm).
fn utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}
