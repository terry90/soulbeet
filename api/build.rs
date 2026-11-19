use std::hash::{DefaultHasher, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let env_vars = get_env_vars();

    println!("cargo:rerun-if-changed=migrations");

    if let Some((path, env_vars)) = env_vars {
        println!("cargo:rerun-if-changed={}", path.display());
        for (key, value) in env_vars {
            println!("cargo:rustc-env={key}={value}");
        }
    }

    println!("cargo:rustc-env=SOULFUL_BUILD_ID={}", build_id());
}

fn get_env_vars() -> Option<(PathBuf, Vec<(String, String)>)> {
    let path = dotenvy::dotenv().ok()?;

    let env_vars: Vec<(String, String)> = dotenvy::vars().collect();
    Some((path, env_vars))
}

/// The id `api::build_id::BUILD_ID` carries: `BUILD_ID` when set (the Docker
/// build, which has no `.git`), else the git commit, marked `-dirty` when the
/// tree has changes, else the crate version and a hash of the sources. The
/// server and the WASM bundle each run this script, so the id must come out
/// the same for both: a clock time would not.
fn build_id() -> String {
    println!("cargo:rerun-if-env-changed=BUILD_ID");
    if let Some(id) = std::env::var("BUILD_ID").ok().filter(|id| !id.is_empty()) {
        return id;
    }
    git_build_id().unwrap_or_else(source_build_id)
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_build_id() -> Option<String> {
    let dirs = git(&[
        "rev-parse",
        "--path-format=absolute",
        "--git-dir",
        "--git-common-dir",
    ])?;
    let mut dirs = dirs.lines().map(PathBuf::from);
    let (git_dir, common_dir) = (dirs.next()?, dirs.next()?);
    let commit = git(&["rev-parse", "--short", "HEAD"])?;
    // Without optional locks, status leaves the index as it is, which would
    // otherwise rerun this script on the next build.
    let changes = git(&[
        "--no-optional-locks",
        "status",
        "--porcelain",
        "--untracked-files=no",
    ])?;

    // A commit, checkout or staged change. A path that does not exist would
    // rerun the script on every build, so only existing ones are named.
    for path in [
        git_dir.join("HEAD"),
        git_dir.join("index"),
        common_dir.join("refs/heads"),
        common_dir.join("packed-refs"),
    ] {
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    Some(if changes.is_empty() {
        commit
    } else {
        format!("{commit}-dirty")
    })
}

fn source_build_id() -> String {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest_dir.parent().unwrap();
    let mut files = Vec::new();
    collect_sources(root, &mut files);
    files.sort();

    let mut hasher = DefaultHasher::new();
    for file in &files {
        println!("cargo:rerun-if-changed={}", file.display());
        hasher.write(
            file.strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .as_bytes(),
        );
        hasher.write(&std::fs::read(file).unwrap());
    }
    format!(
        "{}-{:016x}",
        std::env::var("CARGO_PKG_VERSION").unwrap(),
        hasher.finish()
    )
}

/// The Rust sources and manifests under `dir`, outside build output and
/// hidden folders, and CHANGELOG.md, which the build embeds.
fn collect_sources(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if !name.starts_with('.') && name != "target" && name != "node_modules" {
                collect_sources(&entry.path(), files);
            }
        } else if kind.is_file()
            && (name.ends_with(".rs")
                || name == "Cargo.toml"
                || name == "Cargo.lock"
                || name == "CHANGELOG.md")
        {
            files.push(entry.path());
        }
    }
}
