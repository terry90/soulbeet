//! The key that signs sessions and encrypts stored Navidrome passwords:
//! `SECRET_KEY` when it is set to at least 32 bytes, else a key generated on
//! first start and kept in `secret.key` next to the database. A shorter
//! `SECRET_KEY` signs nothing: the placeholders earlier versions shipped
//! (`secret`, `a_secret_key`, `change-me-in-production`) are all shorter, and
//! anyone who knows a key can sign a session for any user.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const KEY_FILE_NAME: &str = "secret.key";

/// A `SECRET_KEY` shorter than this is not used, and keeps OAuth and `/mcp`
/// closed until it is removed or replaced.
const MIN_KEY_BYTES: usize = 32;

/// The key earlier versions used when `SECRET_KEY` was not set. It is public,
/// so it only reads passwords stored under it, to store them again under the
/// key in use.
const BUILT_IN_KEY: &str = "secret";

/// Random bytes in a generated key.
const GENERATED_KEY_BYTES: usize = 32;

/// Never printed: no `Debug` or `Display`.
pub struct SecretKey(String);

impl SecretKey {
    pub(crate) fn new(value: String) -> SecretKey {
        SecretKey(value)
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

/// Where the key came from, for the startup log.
#[derive(Debug, Clone, PartialEq)]
pub enum KeySource {
    Environment,
    File(PathBuf),
    Generated(PathBuf),
}

/// The key in use, and the keys stored passwords may still be encrypted
/// under.
pub struct Resolved {
    pub key: SecretKey,
    pub source: KeySource,
    /// A `SECRET_KEY` that is set and shorter than 32 bytes, so not used.
    pub short_env: Option<SecretKey>,
}

/// The key: `env` (a non-empty `SECRET_KEY`) when it holds at least 32
/// bytes, else the one stored next to the database file `database_url`
/// names, generated and stored there on first use. A relative database path
/// is read against `base`, the working directory at startup. Fails, naming
/// the file, when the stored key cannot be read or a new one cannot be
/// written; the failure says to set `SECRET_KEY`, or, when the one set is
/// too short, to set it to at least 32 bytes.
pub fn resolve(env: Option<&str>, base: &Path, database_url: &str) -> Result<Resolved, String> {
    let env = env.filter(|v| !v.is_empty()).map(str::to_string);
    let short_env = match env {
        Some(value) if value.len() >= MIN_KEY_BYTES => {
            return Ok(Resolved {
                key: SecretKey::new(value),
                source: KeySource::Environment,
                short_env: None,
            })
        }
        short => short.map(SecretKey::new),
    };
    let set_key = set_key(short_env.is_some());
    let dir = key_dir(base, database_url, &set_key)?;
    let path = dir.join(KEY_FILE_NAME);
    let (key, source) = match read_stored(&path, &set_key)? {
        Some(key) => (key, KeySource::File(path)),
        None => create(&dir, path, &set_key)?,
    };
    Ok(Resolved {
        key,
        source,
        short_env,
    })
}

/// What a startup failure offers in place of the key file, after "set":
/// `SECRET_KEY`, or a longer one when the one set is too short to be used.
fn set_key(short_env: bool) -> String {
    if short_env {
        format!(
            "SECRET_KEY to a random value of at least {MIN_KEY_BYTES} bytes (the SECRET_KEY set \
             is too short, so it is not used)"
        )
    } else {
        "SECRET_KEY".to_string()
    }
}

/// The key stored at `path`, `None` when there is no such file. A failure
/// offers to set `set_key`.
fn read_stored(path: &Path, set_key: &str) -> Result<Option<SecretKey>, String> {
    match fs::read_to_string(path) {
        Ok(stored) => {
            let value = stored.trim();
            if value.is_empty() {
                return Err(format!(
                    "The secret key file {} is empty. Restore it from a backup, delete it to \
                     generate a new key (everyone is signed out and stored Navidrome passwords \
                     must be entered again), or set {set_key}.",
                    path.display()
                ));
            }
            Ok(Some(SecretKey::new(value.to_string())))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!(
            "Cannot read the secret key file {}: {e}. Fix its permissions or set {set_key}.",
            path.display()
        )),
    }
}

/// Generates a key and stores it at `path`, in `dir`. When another start
/// stored one there first, that key is read and used instead, so every
/// process runs with the key the file holds. A failure offers to set
/// `set_key`.
fn create(dir: &Path, path: PathBuf, set_key: &str) -> Result<(SecretKey, KeySource), String> {
    let value = generate()?;
    match store(dir, &path, &value) {
        Ok(()) => Ok((SecretKey::new(value), KeySource::Generated(path))),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            match read_stored(&path, set_key)? {
                Some(key) => Ok((key, KeySource::File(path))),
                None => Err(format!(
                    "The secret key file {} was removed right after another start created it. \
                 Start again or set {set_key}.",
                    path.display()
                )),
            }
        }
        Err(e) => Err(format!(
            "Cannot write the secret key file {}: {e}. Make its directory writable or set \
             {set_key}.",
            path.display()
        )),
    }
}

fn generate() -> Result<String, String> {
    use rand::TryRngCore;
    let mut bytes = [0u8; GENERATED_KEY_BYTES];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|e| format!("Cannot get random bytes from the system for a secret key: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Writes the key to a temporary file of its own that only the owner can
/// read, then links it in at `path` without replacing anything there, so a
/// crash never leaves a partial key behind and two first starts never both
/// win. Fails with `AlreadyExists` when `path` exists. The temporary file is
/// removed whatever happens.
fn store(dir: &Path, path: &Path, value: &str) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let temp = dir.join(format!(
        "{KEY_FILE_NAME}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let linked = write_and_link(&temp, path, value);
    let removed = match fs::remove_file(&temp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    };
    linked?;
    removed?;
    fs::File::open(dir)?.sync_all()
}

fn write_and_link(temp: &Path, path: &Path, value: &str) -> std::io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(temp)?;
    file.write_all(format!("{value}\n").as_bytes())?;
    file.sync_all()?;
    fs::hard_link(temp, path)
}

/// What `load` resolved at startup.
struct Loaded {
    key: SecretKey,
    /// Keys stored passwords may be encrypted under from before: the short
    /// `SECRET_KEY`, when one is set, and the built-in key.
    earlier: Vec<SecretKey>,
    short_env: bool,
}

static LOADED: OnceLock<Loaded> = OnceLock::new();

/// Loads the key once at startup. The database file comes from
/// `DATABASE_URL`; a relative one sits in the working directory.
pub fn load() -> Result<(), String> {
    if LOADED.get().is_some() {
        return Ok(());
    }
    let env = std::env::var("SECRET_KEY").ok();
    let resolved = resolve(
        env.as_deref(),
        Path::new("."),
        crate::config::CONFIG.database_url(),
    )?;
    if resolved.short_env.is_some() {
        tracing::warn!(
            "SECRET_KEY is shorter than {MIN_KEY_BYTES} bytes, so it is not used. Remove it, or set \
             it to a random value of at least {MIN_KEY_BYTES} bytes."
        );
    }
    match &resolved.source {
        KeySource::Environment => tracing::info!("Using SECRET_KEY from the environment"),
        KeySource::File(path) => {
            tracing::info!("Using the secret key stored in {}", path.display())
        }
        KeySource::Generated(path) => tracing::info!(
            "Generated a secret key and stored it in {}. Back it up with the database.",
            path.display()
        ),
    }
    let short_env = resolved.short_env.is_some();
    let earlier = resolved
        .short_env
        .into_iter()
        .chain([SecretKey::new(BUILT_IN_KEY.to_string())])
        .collect();
    // A concurrent first load stored the same key from the same file.
    let _ = LOADED.set(Loaded {
        key: resolved.key,
        earlier,
        short_env,
    });
    Ok(())
}

/// The directory of the database file `database_url` names, a relative one
/// read against `base`. A failure offers to set `set_key`.
fn key_dir(base: &Path, database_url: &str, set_key: &str) -> Result<PathBuf, String> {
    let no_file = || {
        format!(
            "DATABASE_URL '{database_url}' names no database file to keep the secret key next \
             to. Set {set_key}."
        )
    };
    let file = base.join(crate::db::database_file(database_url).ok_or_else(no_file)?);
    file.parent().map(Path::to_path_buf).ok_or_else(no_file)
}

fn loaded() -> &'static Loaded {
    LOADED
        .get()
        .expect("the secret key is loaded at startup, before requests are served")
}

/// The key loaded at startup.
pub fn current() -> &'static SecretKey {
    &loaded().key
}

/// The keys stored passwords may still be encrypted under.
pub fn earlier() -> &'static [SecretKey] {
    &loaded().earlier
}

/// Whether a `SECRET_KEY` shorter than 32 bytes is set, and so not used.
pub fn short_env_key_set() -> bool {
    loaded().short_env
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A database file named relative to the test's base directory.
    const DB: &str = "sqlite:soulbeet.db";

    fn stored(dir: &Path) -> String {
        fs::read_to_string(dir.join(KEY_FILE_NAME)).unwrap()
    }

    #[test]
    fn a_key_is_generated_once_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let Resolved {
            key: first, source, ..
        } = resolve(None, dir.path(), DB).unwrap();
        let path = dir.path().join(KEY_FILE_NAME);
        assert_eq!(source, KeySource::Generated(path.clone()));
        assert_eq!(first.as_bytes().len(), 2 * GENERATED_KEY_BYTES);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(entries(dir.path()), vec![KEY_FILE_NAME.to_string()]);

        let Resolved {
            key: second,
            source,
            ..
        } = resolve(None, dir.path(), DB).unwrap();
        assert_eq!(source, KeySource::File(path));
        assert_eq!(second.as_bytes(), first.as_bytes());
        assert_eq!(stored(dir.path()).trim().as_bytes(), first.as_bytes());
    }

    #[test]
    fn two_installs_get_different_keys() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let ka = resolve(None, a.path(), DB).unwrap().key;
        let kb = resolve(None, b.path(), DB).unwrap().key;
        assert_ne!(ka.as_bytes(), kb.as_bytes());
    }

    #[test]
    fn the_environment_key_wins_and_nothing_is_written() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(KEY_FILE_NAME),
            "stored-key-of-at-least-32-bytes!!",
        )
        .unwrap();
        let resolved = resolve(Some("e".repeat(40).as_str()), dir.path(), DB).unwrap();
        assert_eq!(resolved.source, KeySource::Environment);
        assert_eq!(resolved.key.as_bytes(), "e".repeat(40).as_bytes());
        assert!(resolved.short_env.is_none());

        let empty = tempfile::tempdir().unwrap();
        resolve(Some("e".repeat(40).as_str()), empty.path(), DB).unwrap();
        assert!(!empty.path().join(KEY_FILE_NAME).exists());
    }

    #[test]
    fn an_empty_environment_key_counts_as_unset() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve(Some(""), dir.path(), DB).unwrap();
        assert!(matches!(resolved.source, KeySource::Generated(_)));
        assert!(resolved.short_env.is_none());
    }

    #[test]
    fn a_short_key_someone_set_signs_nothing() {
        // The placeholders earlier READMEs and compose files shipped, and any
        // other key under 32 bytes: the stored key is used instead.
        let dir = tempfile::tempdir().unwrap();
        let stored = resolve(None, dir.path(), DB).unwrap().key;
        let short = "a".repeat(31);
        for value in ["secret", "a_secret_key", "change-me-in-production", &short] {
            let resolved = resolve(Some(value), dir.path(), DB).unwrap();
            assert_eq!(resolved.key.as_bytes(), stored.as_bytes(), "{value}");
            assert!(matches!(resolved.source, KeySource::File(_)), "{value}");
            assert_eq!(
                resolved.short_env.map(|k| k.as_bytes().to_vec()),
                Some(value.as_bytes().to_vec())
            );
        }
        let long = resolve(Some(&"a".repeat(32)), dir.path(), DB).unwrap();
        assert_eq!(long.source, KeySource::Environment);
        assert_eq!(long.key.as_bytes(), "a".repeat(32).as_bytes());
    }

    #[test]
    fn a_key_file_that_cannot_be_used_stops_startup_and_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEY_FILE_NAME);
        fs::write(&path, "\n").unwrap();
        let err = resolve(None, dir.path(), DB).err().unwrap();
        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains("is empty"), "{err}");

        // A directory in place of the file cannot be read.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        let err = resolve(None, dir.path(), DB).err().unwrap();
        assert!(err.starts_with("Cannot read the secret key file"), "{err}");
        assert!(err.contains(&path.display().to_string()), "{err}");
    }

    #[test]
    fn a_directory_that_cannot_be_written_stops_startup_and_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("data");
        fs::create_dir(&blocked).unwrap();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o500)).unwrap();
        let err = resolve(None, &blocked, DB).err();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
        let err = err.unwrap();
        assert!(err.starts_with("Cannot write the secret key file"), "{err}");
        assert!(
            err.contains(&blocked.join(KEY_FILE_NAME).display().to_string()),
            "{err}"
        );
    }

    #[test]
    fn a_short_key_set_where_no_key_file_can_be_kept_is_named_too_short() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("data");
        fs::create_dir(&blocked).unwrap();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o500)).unwrap();
        let unwritable = resolve(Some("change-me-in-production"), &blocked, DB).err();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
        let no_file = resolve(
            Some("change-me-in-production"),
            dir.path(),
            "sqlite::memory:",
        );
        let too_short = "SECRET_KEY to a random value of at least 32 bytes (the SECRET_KEY set \
                         is too short, so it is not used).";
        for err in [unwritable.unwrap(), no_file.err().unwrap()] {
            assert!(err.ends_with(too_short), "{err}");
        }
        // Without SECRET_KEY, the failure offers to set one.
        let unset = resolve(None, dir.path(), "sqlite::memory:").err().unwrap();
        assert!(unset.ends_with(". Set SECRET_KEY."), "{unset}");
    }

    #[test]
    fn the_key_sits_next_to_the_database_file() {
        let here = Path::new(".");
        assert_eq!(
            key_dir(here, "sqlite:/data/soulbeet.db", "SECRET_KEY").unwrap(),
            PathBuf::from("/data")
        );
        // The default DATABASE_URL: the key sits in the working directory.
        assert_eq!(
            key_dir(here, "sqlite:soulbeet.db", "SECRET_KEY").unwrap(),
            here
        );
        assert_eq!(
            key_dir(here, "sqlite:sub/dir/x.db", "SECRET_KEY").unwrap(),
            PathBuf::from("./sub/dir")
        );
        assert!(key_dir(here, "sqlite::memory:", "SECRET_KEY")
            .unwrap_err()
            .contains("Set SECRET_KEY"));
    }

    #[test]
    fn a_relative_database_path_keeps_the_key_beside_it() {
        let base = tempfile::tempdir().unwrap();
        for (url, dir) in [
            ("sqlite:soulbeet.db", base.path().to_path_buf()),
            ("sqlite:sub/dir/x.db", base.path().join("sub/dir")),
        ] {
            let Resolved { key, source, .. } = resolve(None, base.path(), url).unwrap();
            let path = dir.join(KEY_FILE_NAME);
            assert_eq!(source, KeySource::Generated(path.clone()), "{url}");
            assert_eq!(stored(&dir).trim().as_bytes(), key.as_bytes(), "{url}");
            let again = resolve(None, base.path(), url).unwrap();
            assert_eq!(again.source, KeySource::File(path), "{url}");
            assert_eq!(again.key.as_bytes(), key.as_bytes(), "{url}");
        }
    }

    /// The names in `dir`, sorted.
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_key_file_another_start_wrote_meanwhile_wins() {
        // The file was missing when this start looked, and another start
        // stored its key before this one could.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEY_FILE_NAME);
        let winner = "w".repeat(64);
        fs::write(&path, format!("{winner}\n")).unwrap();

        let (key, source) = create(dir.path(), path.clone(), "SECRET_KEY").unwrap();

        assert_eq!(source, KeySource::File(path));
        assert_eq!(key.as_bytes(), winner.as_bytes());
        assert_eq!(stored(dir.path()).trim(), winner);
        assert_eq!(entries(dir.path()), vec![KEY_FILE_NAME.to_string()]);
    }
}
