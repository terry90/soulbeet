use std::path::{Path, PathBuf};

/// Extensions soulbeet treats as audio, lowercase.
pub const AUDIO_EXTENSIONS: &[&str] = &["flac", "mp3", "m4a", "ogg", "aac", "wav", "wma", "opus"];

pub fn is_audio_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| AUDIO_EXTENSIONS.iter().any(|a| a.eq_ignore_ascii_case(ext)))
}

/// Audio files among `sources`. A file counts when it has an audio extension,
/// a directory is walked recursively (symlinked directories are not followed).
/// Unreadable entries count as 0.
pub async fn count_audio_files(sources: &[&Path]) -> usize {
    let mut count = 0;
    let mut dirs: Vec<PathBuf> = Vec::new();
    for source in sources {
        match tokio::fs::metadata(source).await {
            Ok(meta) if meta.is_dir() => dirs.push(source.to_path_buf()),
            Ok(_) if is_audio_path(source) => count += 1,
            _ => {}
        }
    }
    while let Some(dir) = dirs.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            match entry.file_type().await {
                Ok(kind) if kind.is_dir() => dirs.push(path),
                Ok(_) if is_audio_path(&path) => count += 1,
                _ => {}
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_extensions_match_case_insensitively() {
        assert!(is_audio_path(Path::new("a/01 - Song.FLAC")));
        assert!(is_audio_path(Path::new("b.opus")));
        assert!(!is_audio_path(Path::new("cover.jpg")));
        assert!(!is_audio_path(Path::new("noext")));
    }

    #[tokio::test]
    async fn count_audio_files_recurses_and_ignores_non_audio() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("dir");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.FLAC"), b"").unwrap();
        std::fs::write(dir.join("sub/b.opus"), b"").unwrap();
        std::fs::write(dir.join("cover.jpg"), b"").unwrap();
        let file = root.path().join("c.mp3");
        std::fs::write(&file, b"").unwrap();

        assert_eq!(count_audio_files(&[dir.as_path(), file.as_path()]).await, 3);
    }
}
