#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

const WEB_INPUTS: &[&str] = &[
    "src",
    "public",
    "index.html",
    "package.json",
    "package-lock.json",
    "vite.config.ts",
    "tsconfig.json",
];

const PLACEHOLDER_MARKER: &str = "<!-- mobile_server placeholder -->";

fn main() {
    let manifest_dir = match std::env::var_os("CARGO_MANIFEST_DIR") {
        Some(manifest_dir) => PathBuf::from(manifest_dir),
        None => {
            println!("cargo:warning=CARGO_MANIFEST_DIR is not set; skipping the PWA build");
            return;
        }
    };
    let web_dir = manifest_dir.join("web");
    let dist_dir = web_dir.join("dist");
    let dist_index = dist_dir.join("index.html");

    println!("cargo:rerun-if-changed=build.rs");
    // Watching all of `dist` would rerun this script after every build it does
    // itself, since Vite rewrites the whole directory.
    println!("cargo:rerun-if-changed=web/dist/index.html");
    // Cargo reruns the script on every build for paths that don't exist, so
    // only watch the optional inputs once they appear.
    println!("cargo:rerun-if-changed=web/package.json");
    for input in WEB_INPUTS {
        if web_dir.join(input).exists() {
            println!("cargo:rerun-if-changed=web/{input}");
        }
    }

    if web_dir.join("package.json").exists() {
        if needs_build(&web_dir, &dist_index) {
            if let Err(error) = build_web(&web_dir) {
                println!("cargo:warning=the mobile PWA was not built: {error}");
            }
        }
    } else {
        println!("cargo:warning=web/package.json not found; the mobile PWA was not built");
    }

    if !dist_index.exists() {
        if let Err(error) = write_placeholder(&dist_dir, &dist_index) {
            println!("cargo:warning=failed to write the PWA placeholder page: {error}");
        }
    }
}

fn needs_build(web_dir: &Path, dist_index: &Path) -> bool {
    let Some(built_at) = modified_time(dist_index) else {
        return true;
    };
    let is_placeholder = std::fs::read_to_string(dist_index)
        .map(|contents| contents.contains(PLACEHOLDER_MARKER))
        .unwrap_or(false);
    if is_placeholder {
        return true;
    }
    WEB_INPUTS
        .iter()
        .filter_map(|input| newest_modified_time(&web_dir.join(input)))
        .any(|modified_at| modified_at > built_at)
}

fn modified_time(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

fn newest_modified_time(path: &Path) -> Option<SystemTime> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_dir() {
        return metadata.modified().ok();
    }
    std::fs::read_dir(path)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| newest_modified_time(&entry.path()))
        .max()
}

fn build_web(web_dir: &Path) -> Result<(), String> {
    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };

    if run_npm(npm, web_dir, &["--version"]).is_err() {
        return Err("npm is not available".to_string());
    }

    if !web_dir.join("node_modules").exists() {
        let install_command: &[&str] = if web_dir.join("package-lock.json").exists() {
            &["ci"]
        } else {
            &["install"]
        };
        run_npm(npm, web_dir, install_command)?;
    }

    run_npm(npm, web_dir, &["run", "build"])
}

fn run_npm(npm: &str, web_dir: &Path, arguments: &[&str]) -> Result<(), String> {
    let output = Command::new(npm)
        .args(arguments)
        .current_dir(web_dir)
        .output()
        .map_err(|error| format!("failed to run `{npm} {}`: {error}", arguments.join(" ")))?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let tail = stderr
        .lines()
        .chain(stdout.lines())
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let tail = tail[tail.len().saturating_sub(10)..].join(" | ");
    Err(format!(
        "`{npm} {}` exited with {}: {tail}",
        arguments.join(" "),
        output.status
    ))
}

fn write_placeholder(dist_dir: &Path, dist_index: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dist_dir)?;
    std::fs::write(
        dist_index,
        "<!doctype html>\n<!-- mobile_server placeholder -->\n<html lang=\"en\">\n<head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Zed</title></head>\n<body><p>The Zed mobile web app wasn't built. Install Node.js and npm, then rebuild <code>mobile_server</code>.</p></body>\n</html>\n",
    )
}
