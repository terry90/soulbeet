//! Checks the beets config by running probe.py, which loads it the way
//! `beet -c` does, and turns the report into importer health.

use super::runner::{run_beet, RunEnd, RunLimits, RunSpec, Stream};
use serde::Deserialize;
use shared::system::{HealthIssue, HealthLevel, ImporterHealth};
use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

const PROBE_PY: &str = include_str!("probe.py");

/// beatport and tidal log in on `import_begin`, which the probe never fires.
const LOGIN_AT_IMPORT: [&str; 2] = ["beatport", "tidal"];
/// chroma's source; it looks matches up through MusicBrainz.
const ACOUSTID: &str = "Acoustid";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PluginError {
    pub plugin: String,
    pub error: String,
}

/// What probe.py found. The JSON keys are shared with the pytest suite and the
/// CI image check.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProbeReport {
    pub fatal: Option<String>,
    pub loaded: Vec<String>,
    pub plugin_errors: Vec<PluginError>,
    pub metadata_sources: Vec<String>,
    /// The base config beets reads under the `-c` overlay, whether or not it exists.
    pub user_config_path: Option<String>,
    /// Absolute, resolved against the cwd the way beets resolves them.
    pub pluginpath: Vec<String>,
    pub autotag: bool,
}

#[derive(Debug)]
pub enum ProbeFailure {
    NoPython,
    Timeout,
    CouldNotRun(String),
    NoReport { exit: Option<i32>, tail: String },
}

pub(crate) async fn run_probe(
    python: Option<&Path>,
    config: &Path,
) -> Result<ProbeReport, ProbeFailure> {
    probe_with(python, PROBE_PY, config, RunLimits::PROBE).await
}

async fn probe_with(
    python: Option<&Path>,
    script: &str,
    config: &Path,
    limits: RunLimits,
) -> Result<ProbeReport, ProbeFailure> {
    let python = python.ok_or(ProbeFailure::NoPython)?;
    let spec = RunSpec {
        program: python,
        args: vec!["-P".into(), "-c".into(), script.into(), config.into()],
        limits,
    };
    let mut last_stdout = String::new();
    let mut on_line = |stream: Stream, line: &str| {
        if stream == Stream::Stdout && !line.trim().is_empty() {
            last_stdout = line.to_string();
        }
    };
    let outcome = run_beet(spec, &mut on_line).await;
    match outcome.end {
        RunEnd::SpawnFailed(e) => Err(ProbeFailure::CouldNotRun(format!(
            "{}: {e}",
            python.display()
        ))),
        RunEnd::Idle | RunEnd::Total => Err(ProbeFailure::Timeout),
        RunEnd::Exited(status) => {
            serde_json::from_str(&last_stdout).map_err(|_| ProbeFailure::NoReport {
                exit: status.code(),
                tail: outcome.tail,
            })
        }
    }
}

fn issue(
    severity: HealthLevel,
    title: impl Into<String>,
    detail: impl Into<String>,
) -> HealthIssue {
    HealthIssue {
        severity,
        title: title.into(),
        detail: detail.into(),
    }
}

fn list_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_string()
    } else {
        items.join(", ")
    }
}

pub(crate) fn assess(config: &Path, probe: &Result<ProbeReport, ProbeFailure>) -> ImporterHealth {
    use HealthLevel::{Broken, Degraded};
    let mut facts = vec![("Config".to_string(), config.display().to_string())];
    let mut issues = Vec::new();
    match probe {
        Err(ProbeFailure::NoPython) => issues.push(issue(
            Broken,
            "Cannot determine the Python that runs beet",
            "Set BEETS_PYTHON.",
        )),
        Err(ProbeFailure::Timeout) => issues.push(issue(
            Degraded,
            "beets config check timed out",
            "A plugin may be waiting on the network or for input.",
        )),
        Err(ProbeFailure::CouldNotRun(why)) => issues.push(issue(
            Broken,
            "beets config check failed",
            format!("Could not run {why}"),
        )),
        Err(ProbeFailure::NoReport { exit, tail }) => {
            let ended = match exit {
                Some(code) => format!("Exited with code {code}."),
                None => "Killed by a signal.".to_string(),
            };
            let detail = if tail.is_empty() {
                ended
            } else {
                format!("{ended}\n{tail}")
            };
            issues.push(issue(Broken, "beets config check failed", detail));
        }
        Ok(report) => {
            facts.push((
                "Base config".to_string(),
                report
                    .user_config_path
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
            ));
            facts.push(("Plugins".to_string(), list_or_none(&report.loaded)));
            facts.push((
                "Metadata sources".to_string(),
                list_or_none(&report.metadata_sources),
            ));
            if let Some(fatal) = &report.fatal {
                // Nothing else matters while beets cannot start.
                issues.push(issue(Broken, "beets cannot start", fatal.clone()));
            } else {
                // Acoustid needs MusicBrainz, which is a usable source itself.
                if !report.metadata_sources.iter().any(|s| s != ACOUSTID) {
                    issues.push(issue(
                        Broken,
                        "No metadata source plugin loaded",
                        format!(
                            "BEETS_CONFIG replaces /app/beets_config.yaml. Add musicbrainz to plugins: in {}.",
                            config.display()
                        ),
                    ));
                }
                for e in &report.plugin_errors {
                    issues.push(issue(
                        Degraded,
                        format!("Plugin {} failed to load", e.plugin),
                        e.error.clone(),
                    ));
                }
                if !report.autotag {
                    issues.push(issue(
                        Degraded,
                        "import.autotag is off",
                        "Imports will not be tagged.",
                    ));
                }
                for name in LOGIN_AT_IMPORT {
                    if report.loaded.iter().any(|p| p == name) {
                        issues.push(issue(
                            Degraded,
                            format!("{name} logs in when an import starts"),
                            "Its login is not checked here.",
                        ));
                    }
                }
            }
        }
    }
    let level = issues
        .iter()
        .map(|i| i.severity)
        .max()
        .unwrap_or(HealthLevel::Ready);
    ImporterHealth {
        level,
        issues,
        facts,
    }
}

/// File identity and change times, enough to see an edit or an atomic replace.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Stamp {
    fn of(meta: &std::fs::Metadata) -> Self {
        Stamp {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        }
    }
}

/// What a config check depends on: the `-c` file, the base config and the
/// pluginpath listings. beet runs fresh per import, so any change there can
/// change the result without the `-c` file being touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fingerprint(Vec<(PathBuf, Option<Stamp>)>);

impl Fingerprint {
    /// This fingerprint with the stamps `earlier` holds for the same paths, so a
    /// change made after `earlier` was taken still reads as a change.
    pub(crate) fn backdated(mut self, earlier: &Fingerprint) -> Fingerprint {
        for (path, stamp) in &mut self.0 {
            if let Some((_, old)) = earlier.0.iter().find(|(p, _)| p == path) {
                *stamp = old.clone();
            }
        }
        self
    }
}

async fn stamp(path: &Path) -> Option<Stamp> {
    tokio::fs::metadata(path).await.ok().map(|m| Stamp::of(&m))
}

pub(crate) async fn fingerprint(config: &Path, report: Option<&ProbeReport>) -> Fingerprint {
    let mut seen = vec![(config.to_path_buf(), stamp(config).await)];
    let Some(report) = report else {
        return Fingerprint(seen);
    };
    if let Some(base) = &report.user_config_path {
        let base = PathBuf::from(base);
        let s = stamp(&base).await;
        seen.push((base, s));
    }
    for dir in &report.pluginpath {
        let dir = PathBuf::from(dir);
        let s = stamp(&dir).await;
        seen.push((dir.clone(), s));
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        let mut listing = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let s = stamp(&path).await;
            listing.push((path, s));
        }
        listing.sort_by(|a, b| a.0.cmp(&b.0));
        seen.extend(listing);
    }
    Fingerprint(seen)
}

/// The interpreter of a `#!/abs/path/python...` line. `#!/usr/bin/env python3`
/// and distlib's `#!/bin/sh` launcher give None: the first would resolve
/// differently here than for beet, the second is not a python at all.
pub fn python_from_shebang(beet: &Path) -> Option<PathBuf> {
    let mut head = [0u8; 512];
    let n = std::fs::File::open(beet).ok()?.read(&mut head).ok()?;
    let first = head[..n].split(|&b| b == b'\n').next()?;
    let line = std::str::from_utf8(first).ok()?.strip_prefix("#!")?;
    let interpreter = Path::new(line.split_whitespace().next()?);
    let name = interpreter.file_name()?.to_str()?;
    (interpreter.is_absolute() && name.starts_with("python")).then(|| interpreter.to_path_buf())
}

/// The first executable file called `name` on PATH.
pub fn resolve_on_path(name: &str) -> Option<PathBuf> {
    resolve_in(name, &std::env::var_os("PATH")?)
}

fn resolve_in(name: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::system::HealthLevel;
    use std::time::Duration;

    const CFG: &str = "/config/beets.yaml";

    fn report(sources: &[&str]) -> ProbeReport {
        ProbeReport {
            fatal: None,
            loaded: vec!["musicbrainz".into()],
            plugin_errors: vec![],
            metadata_sources: sources.iter().map(|s| s.to_string()).collect(),
            user_config_path: Some("/tmp/.config/beets/config.yaml".into()),
            pluginpath: vec![],
            autotag: true,
        }
    }

    fn assess_ok(r: ProbeReport) -> ImporterHealth {
        assess(Path::new(CFG), &Ok(r))
    }

    fn titles(h: &ImporterHealth) -> Vec<&str> {
        h.issues.iter().map(|i| i.title.as_str()).collect()
    }

    #[test]
    fn no_source_is_broken_and_names_the_config() {
        let h = assess_ok(report(&[]));
        assert_eq!(h.level, HealthLevel::Broken);
        assert_eq!(titles(&h), ["No metadata source plugin loaded"]);
        assert_eq!(
            h.issues[0].detail,
            "BEETS_CONFIG replaces /app/beets_config.yaml. Add musicbrainz to plugins: in /config/beets.yaml."
        );
    }

    #[test]
    fn acoustid_alone_is_not_a_usable_source() {
        let h = assess_ok(report(&["Acoustid"]));
        assert_eq!(h.level, HealthLevel::Broken);
        assert_eq!(titles(&h), ["No metadata source plugin loaded"]);
    }

    #[test]
    fn acoustid_with_musicbrainz_is_ready() {
        let h = assess_ok(report(&["Acoustid", "MusicBrainz"]));
        assert_eq!(h.level, HealthLevel::Ready);
        assert!(h.issues.is_empty());
    }

    #[test]
    fn fatal_is_broken_with_the_probe_text() {
        let mut r = report(&[]);
        r.fatal = Some("configuration error: x could not be read".into());
        let h = assess_ok(r);
        assert_eq!(h.level, HealthLevel::Broken);
        assert_eq!(titles(&h), ["beets cannot start"]);
        assert_eq!(
            h.issues[0].detail,
            "configuration error: x could not be read"
        );
    }

    #[test]
    fn plugin_errors_with_a_source_are_degraded() {
        let mut r = report(&["MusicBrainz"]);
        r.plugin_errors = vec![
            PluginError {
                plugin: "fetchart".into(),
                error: "UserError: setting x is not recognized".into(),
            },
            PluginError {
                plugin: "web".into(),
                error: "ModuleNotFoundError: No module named 'flask'".into(),
            },
        ];
        let h = assess_ok(r);
        assert_eq!(h.level, HealthLevel::Degraded);
        assert_eq!(
            titles(&h),
            [
                "Plugin fetchart failed to load",
                "Plugin web failed to load"
            ]
        );
        assert_eq!(
            h.issues[1].detail,
            "ModuleNotFoundError: No module named 'flask'"
        );
        assert!(h.issues.iter().all(|i| i.severity == HealthLevel::Degraded));
    }

    #[test]
    fn autotag_off_is_degraded() {
        let mut r = report(&["MusicBrainz"]);
        r.autotag = false;
        let h = assess_ok(r);
        assert_eq!(h.level, HealthLevel::Degraded);
        assert_eq!(titles(&h), ["import.autotag is off"]);
        assert_eq!(h.issues[0].detail, "Imports will not be tagged.");
    }

    #[test]
    fn beatport_and_tidal_are_degraded() {
        let mut r = report(&["MusicBrainz", "Beatport"]);
        r.loaded = vec!["musicbrainz".into(), "beatport".into(), "tidal".into()];
        let h = assess_ok(r);
        assert_eq!(h.level, HealthLevel::Degraded);
        assert_eq!(
            titles(&h),
            [
                "beatport logs in when an import starts",
                "tidal logs in when an import starts"
            ]
        );
        assert_eq!(h.issues[0].detail, "Its login is not checked here.");
    }

    #[test]
    fn facts_list_config_base_config_plugins_and_sources() {
        let mut r = report(&["MusicBrainz"]);
        r.loaded = vec!["musicbrainz".into(), "mbtwopass".into()];
        let h = assess_ok(r);
        assert_eq!(
            h.facts,
            [
                ("Config".to_string(), CFG.to_string()),
                (
                    "Base config".to_string(),
                    "/tmp/.config/beets/config.yaml".to_string()
                ),
                ("Plugins".to_string(), "musicbrainz, mbtwopass".to_string()),
                ("Metadata sources".to_string(), "MusicBrainz".to_string()),
            ]
        );
    }

    #[test]
    fn no_python_is_broken() {
        let h = assess(Path::new(CFG), &Err(ProbeFailure::NoPython));
        assert_eq!(h.level, HealthLevel::Broken);
        assert_eq!(titles(&h), ["Cannot determine the Python that runs beet"]);
        assert_eq!(h.issues[0].detail, "Set BEETS_PYTHON.");
    }

    #[test]
    fn timeout_is_degraded() {
        let h = assess(Path::new(CFG), &Err(ProbeFailure::Timeout));
        assert_eq!(h.level, HealthLevel::Degraded);
        assert_eq!(titles(&h), ["beets config check timed out"]);
        assert_eq!(
            h.issues[0].detail,
            "A plugin may be waiting on the network or for input."
        );
    }

    #[test]
    fn no_report_is_broken_with_exit_code_and_tail() {
        let failure = ProbeFailure::NoReport {
            exit: Some(1),
            tail: "ImportError: numpy.core.multiarray failed to import".into(),
        };
        let h = assess(Path::new(CFG), &Err(failure));
        assert_eq!(h.level, HealthLevel::Broken);
        assert_eq!(titles(&h), ["beets config check failed"]);
        assert!(
            h.issues[0].detail.contains("code 1"),
            "{}",
            h.issues[0].detail
        );
        assert!(h.issues[0].detail.contains("numpy.core.multiarray"));
    }

    #[test]
    fn could_not_run_is_broken() {
        let failure = ProbeFailure::CouldNotRun("/usr/bin/python3: not found".into());
        let h = assess(Path::new(CFG), &Err(failure));
        assert_eq!(h.level, HealthLevel::Broken);
        assert_eq!(titles(&h), ["beets config check failed"]);
        assert!(h.issues[0].detail.contains("/usr/bin/python3: not found"));
    }

    #[test]
    fn python_from_shebang_accepts_direct_paths_only() {
        let dir = tempfile::tempdir().unwrap();
        let script = |name: &str, body: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, body).unwrap();
            p
        };
        let direct = script("direct", "#!/usr/bin/python3\nimport sys\n");
        let env = script("env", "#!/usr/bin/env python3\nimport sys\n");
        let distlib = script(
            "distlib",
            "#!/bin/sh\n'''exec' \"/opt/venv/bin/python3\" \"$0\" \"$@\"\n' '''\nimport sys\n",
        );
        assert_eq!(
            python_from_shebang(&direct),
            Some(PathBuf::from("/usr/bin/python3"))
        );
        assert_eq!(python_from_shebang(&env), None);
        assert_eq!(python_from_shebang(&distlib), None);
        assert_eq!(python_from_shebang(&dir.path().join("missing")), None);
    }

    #[test]
    fn resolve_in_finds_the_first_executable() {
        use std::os::unix::fs::PermissionsExt;
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("beet"), "").unwrap();
        let exe = b.path().join("beet");
        std::fs::write(&exe, "").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([a.path(), b.path()]).unwrap();
        assert_eq!(resolve_in("beet", &path), Some(exe));
        assert_eq!(resolve_in("nothere", &path), None);
    }

    #[tokio::test]
    async fn fingerprint_changes_on_atomic_replace_and_new_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        let plugins = dir.path().join("plugins");
        std::fs::create_dir(&plugins).unwrap();
        std::fs::write(&config, "plugins: musicbrainz\n").unwrap();
        let mut r = report(&["MusicBrainz"]);
        r.pluginpath = vec![plugins.to_string_lossy().into_owned()];

        let first = fingerprint(&config, Some(&r)).await;
        assert_eq!(first, fingerprint(&config, Some(&r)).await);

        let tmp = dir.path().join("config.yaml.tmp");
        std::fs::write(&tmp, "plugins: chroma12345\n").unwrap();
        std::fs::rename(&tmp, &config).unwrap();
        let replaced = fingerprint(&config, Some(&r)).await;
        assert_ne!(first, replaced);

        std::fs::write(plugins.join("x.py"), "").unwrap();
        assert_ne!(replaced, fingerprint(&config, Some(&r)).await);
    }

    #[tokio::test]
    async fn fingerprint_sees_the_base_config_appear() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        std::fs::write(&config, "plugins: musicbrainz\n").unwrap();
        let base = dir.path().join("base.yaml");
        let mut r = report(&["MusicBrainz"]);
        r.user_config_path = Some(base.to_string_lossy().into_owned());

        let before = fingerprint(&config, Some(&r)).await;
        std::fs::write(&base, "import: {autotag: no}\n").unwrap();
        assert_ne!(before, fingerprint(&config, Some(&r)).await);
    }

    #[tokio::test]
    async fn a_backdated_fingerprint_keeps_a_change_made_in_between() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        let plugins = dir.path().join("plugins");
        std::fs::create_dir(&plugins).unwrap();
        std::fs::write(&config, "plugins: musicbrainz\n").unwrap();

        // Taken before the check knew about the pluginpath.
        let before = fingerprint(&config, None).await;
        std::fs::write(&config, "plugins: musicbrainz chroma\n").unwrap();
        let mut r = report(&["MusicBrainz"]);
        r.pluginpath = vec![plugins.to_string_lossy().into_owned()];
        let after = fingerprint(&config, Some(&r)).await;

        let stored = after.clone().backdated(&before);
        assert_ne!(stored, after);
        assert_ne!(stored, fingerprint(&config, Some(&r)).await);
        assert_eq!(after.clone().backdated(&after), after);
    }

    // A bash script stands in for python: `bash -P -c SCRIPT CFG` runs SCRIPT
    // with CFG as $0, the way python gets it as sys.argv[1].
    const BASH: &str = "/bin/bash";
    const SHORT: RunLimits = RunLimits {
        idle: Duration::from_secs(10),
        total: Duration::from_secs(30),
        drain: Duration::from_millis(300),
    };

    async fn fake(script: &str) -> Result<ProbeReport, ProbeFailure> {
        probe_with(Some(Path::new(BASH)), script, Path::new(CFG), SHORT).await
    }

    const REPORT_JSON: &str = r#"{"fatal": null, "loaded": ["musicbrainz"], "plugin_errors": [{"plugin": "web", "error": "boom"}], "metadata_sources": ["MusicBrainz"], "user_config_path": "/b/config.yaml", "pluginpath": ["/p"], "autotag": false}"#;

    #[tokio::test]
    async fn run_probe_parses_the_last_stdout_line() {
        let script = format!(
            "echo 'To authenticate, visit:'; echo noise >&2; [ \"$0\" = {CFG} ] || exit 9; echo '{REPORT_JSON}'"
        );
        let r = fake(&script).await.unwrap();
        assert_eq!(r.loaded, ["musicbrainz"]);
        assert_eq!(
            r.plugin_errors,
            [PluginError {
                plugin: "web".into(),
                error: "boom".into()
            }]
        );
        assert_eq!(r.user_config_path.as_deref(), Some("/b/config.yaml"));
        assert_eq!(r.pluginpath, ["/p"]);
        assert!(!r.autotag);
    }

    #[tokio::test]
    async fn run_probe_without_json_is_no_report() {
        match fake("echo 'Traceback: boom' >&2; exit 3").await {
            Err(ProbeFailure::NoReport { exit, tail }) => {
                assert_eq!(exit, Some(3));
                assert!(tail.contains("Traceback: boom"), "{tail}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn run_probe_times_out() {
        let limits = RunLimits {
            idle: Duration::from_millis(500),
            ..SHORT
        };
        let r = probe_with(Some(Path::new(BASH)), "sleep 5", Path::new(CFG), limits).await;
        assert!(matches!(r, Err(ProbeFailure::Timeout)));
    }

    #[tokio::test]
    async fn run_probe_without_python() {
        let r = run_probe(None, Path::new(CFG)).await;
        assert!(matches!(r, Err(ProbeFailure::NoPython)));
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("python3");
        match probe_with(Some(&missing), "", Path::new(CFG), SHORT).await {
            Err(ProbeFailure::CouldNotRun(msg)) => {
                assert!(msg.contains(&*missing.to_string_lossy()), "{msg}")
            }
            other => panic!("{other:?}"),
        }
    }

    // The real script through the real runner: whether or not this python has
    // beets, a missing config must come back as a parsed report with `fatal`.
    #[tokio::test]
    async fn the_real_probe_always_reports_json() {
        let python = resolve_on_path("python3").expect("python3 on PATH");
        let dir = tempfile::tempdir().unwrap();
        let r = run_probe(Some(&python), &dir.path().join("missing.yaml")).await;
        match r {
            Ok(report) => {
                let fatal = report.fatal.as_deref().unwrap_or_default();
                assert!(
                    fatal.starts_with("configuration error:")
                        || fatal.starts_with("ModuleNotFoundError:"),
                    "{report:?}"
                );
            }
            Err(e) => panic!("{e:?}"),
        }
    }
}
