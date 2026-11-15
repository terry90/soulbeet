//! The key that signs sessions and encrypts stored Navidrome passwords:
//! `SECRET_KEY` when it is set, else a key generated on first start and kept
//! in `secret.key` next to the database.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const KEY_FILE_NAME: &str = "secret.key";

/// A key shorter than this keeps OAuth and `/mcp` closed.
const MIN_KEY_BYTES: usize = 32;

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

    /// A key someone set that is shorter than 32 bytes.
    pub fn is_weak(&self) -> bool {
        self.0.len() < MIN_KEY_BYTES
    }
}

/// Where the key came from, for the startup log.
#[derive(Debug, Clone, PartialEq)]
pub enum KeySource {
    Environment,
    File(PathBuf),
    Generated(PathBuf),
}

/// The key: `env` (a non-empty `SECRET_KEY`) when given, else the one stored
/// next to the database file `database_url` names, generated and stored there
/// on first use. A relative database path is read against `base`, the working
/// directory at startup. Fails, naming the file, when the stored key cannot be
/// read or a new one cannot be written.
pub fn resolve(
    env: Option<&str>,
    base: &Path,
    database_url: &str,
) -> Result<(SecretKey, KeySource), String> {
    if let Some(value) = env.filter(|v| !v.is_empty()) {
        return Ok((SecretKey::new(value.to_string()), KeySource::Environment));
    }
    let dir = key_dir(base, database_url)?;
    let path = dir.join(KEY_FILE_NAME);
    match read_stored(&path)? {
        Some(key) => Ok((key, KeySource::File(path))),
        None => create(&dir, path),
    }
}

/// The key stored at `path`, `None` when there is no such file.
fn read_stored(path: &Path) -> Result<Option<SecretKey>, String> {
    match fs::read_to_string(path) {
        Ok(stored) => {
            let value = stored.trim();
            if value.is_empty() {
                return Err(format!(
                    "The secret key file {} is empty. Restore it from a backup, delete it to \
                     generate a new key (everyone is signed out and stored Navidrome passwords \
                     must be entered again), or set SECRET_KEY.",
                    path.display()
                ));
            }
            Ok(Some(SecretKey::new(value.to_string())))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!(
            "Cannot read the secret key file {}: {e}. Fix its permissions or set SECRET_KEY.",
            path.display()
        )),
    }
}

/// Generates a key and stores it at `path`, in `dir`. When another start
/// stored one there first, that key is read and used instead, so every
/// process runs with the key the file holds.
fn create(dir: &Path, path: PathBuf) -> Result<(SecretKey, KeySource), String> {
    let value = generate()?;
    match store(dir, &path, &value) {
        Ok(()) => Ok((SecretKey::new(value), KeySource::Generated(path))),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match read_stored(&path)? {
            Some(key) => Ok((key, KeySource::File(path))),
            None => Err(format!(
                "The secret key file {} was removed right after another start created it. \
                 Start again or set SECRET_KEY.",
                path.display()
            )),
        },
        Err(e) => Err(format!(
            "Cannot write the secret key file {}: {e}. Make its directory writable or set \
             SECRET_KEY.",
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

static KEY: OnceLock<SecretKey> = OnceLock::new();

/// Loads the key once at startup. The database file comes from
/// `DATABASE_URL`; a relative one sits in the working directory.
pub fn load() -> Result<(), String> {
    if KEY.get().is_some() {
        return Ok(());
    }
    let env = std::env::var("SECRET_KEY").ok();
    let (key, source) = resolve(
        env.as_deref(),
        Path::new("."),
        crate::config::CONFIG.database_url(),
    )?;
    match source {
        KeySource::Environment => tracing::info!("Using SECRET_KEY from the environment"),
        KeySource::File(path) => tracing::info!("Using the secret key stored in {}", path.display()),
        KeySource::Generated(path) => tracing::info!(
            "SECRET_KEY is not set: generated a secret key and stored it in {}. Back it up with the database.",
            path.display()
        ),
    }
    // A concurrent first load stored the same key from the same file.
    let _ = KEY.set(key);
    Ok(())
}

/// The directory of the database file `database_url` names, a relative one
/// read against `base`.
fn key_dir(base: &Path, database_url: &str) -> Result<PathBuf, String> {
    let no_file = || {
        format!(
            "DATABASE_URL '{database_url}' names no database file to keep the secret key next \
             to. Set SECRET_KEY."
        )
    };
    let file = base.join(crate::db::database_file(database_url).ok_or_else(no_file)?);
    file.parent().map(Path::to_path_buf).ok_or_else(no_file)
}

/// The key loaded at startup.
pub fn current() -> &'static SecretKey {
    KEY.get()
        .expect("the secret key is loaded at startup, before requests are served")
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
        let (first, source) = resolve(None, dir.path(), DB).unwrap();
        let path = dir.path().join(KEY_FILE_NAME);
        assert_eq!(source, KeySource::Generated(path.clone()));
        assert!(!first.is_weak());
        assert_eq!(first.as_bytes().len(), 2 * GENERATED_KEY_BYTES);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(entries(dir.path()), vec![KEY_FILE_NAME.to_string()]);

        let (second, source) = resolve(None, dir.path(), DB).unwrap();
        assert_eq!(source, KeySource::File(path));
        assert_eq!(second.as_bytes(), first.as_bytes());
        assert_eq!(stored(dir.path()).trim().as_bytes(), first.as_bytes());
    }

    #[test]
    fn two_installs_get_different_keys() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (ka, _) = resolve(None, a.path(), DB).unwrap();
        let (kb, _) = resolve(None, b.path(), DB).unwrap();
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
        let (key, source) = resolve(Some("e".repeat(40).as_str()), dir.path(), DB).unwrap();
        assert_eq!(source, KeySource::Environment);
        assert_eq!(key.as_bytes(), "e".repeat(40).as_bytes());

        let empty = tempfile::tempdir().unwrap();
        resolve(Some("e".repeat(40).as_str()), empty.path(), DB).unwrap();
        assert!(!empty.path().join(KEY_FILE_NAME).exists());
    }

    #[test]
    fn an_empty_environment_key_counts_as_unset() {
        let dir = tempfile::tempdir().unwrap();
        let (_, source) = resolve(Some(""), dir.path(), DB).unwrap();
        assert!(matches!(source, KeySource::Generated(_)));
    }

    #[test]
    fn a_short_key_someone_set_is_weak() {
        let dir = tempfile::tempdir().unwrap();
        let weak = |v: &str| resolve(Some(v), dir.path(), DB).unwrap().0.is_weak();
        assert!(weak("secret"));
        assert!(weak(&"a".repeat(31)));
        assert!(!weak(&"a".repeat(32)));
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
    fn the_key_sits_next_to_the_database_file() {
        let here = Path::new(".");
        assert_eq!(
            key_dir(here, "sqlite:/data/soulbeet.db").unwrap(),
            PathBuf::from("/data")
        );
        // The default DATABASE_URL: the key sits in the working directory.
        assert_eq!(key_dir(here, "sqlite:soulbeet.db").unwrap(), here);
        assert_eq!(
            key_dir(here, "sqlite:sub/dir/x.db").unwrap(),
            PathBuf::from("./sub/dir")
        );
        assert!(key_dir(here, "sqlite::memory:")
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
            let (key, source) = resolve(None, base.path(), url).unwrap();
            let path = dir.join(KEY_FILE_NAME);
            assert_eq!(source, KeySource::Generated(path.clone()), "{url}");
            assert_eq!(stored(&dir).trim().as_bytes(), key.as_bytes(), "{url}");
            let (again, source) = resolve(None, base.path(), url).unwrap();
            assert_eq!(source, KeySource::File(path), "{url}");
            assert_eq!(again.as_bytes(), key.as_bytes(), "{url}");
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

        let (key, source) = create(dir.path(), path.clone()).unwrap();

        assert_eq!(source, KeySource::File(path));
        assert_eq!(key.as_bytes(), winner.as_bytes());
        assert_eq!(stored(dir.path()).trim(), winner);
        assert_eq!(entries(dir.path()), vec![KEY_FILE_NAME.to_string()]);
    }
}
