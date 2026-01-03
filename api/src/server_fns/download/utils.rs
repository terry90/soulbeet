#[cfg(feature = "server")]
pub fn resolve_download_path(filename: &str, download_base: &std::path::Path) -> Option<String> {
    // Normalize path separators (win -> linux)
    let filename_str = filename.replace('\\', "/");
    let path = std::path::Path::new(&filename_str);
    let components: Vec<_> = path.components().collect();

    // Keep only the last directory and filename (d1/d2/d3/file -> d3/file)
    if components.len() >= 2 {
        let len = components.len();
        let last_dir = components[len - 2].as_os_str();
        let file_name = components[len - 1].as_os_str();

        let relative_path = std::path::PathBuf::from(last_dir).join(file_name);
        let full_path = download_base.join(relative_path);

        Some(full_path.to_string_lossy().to_string())
    } else {
        // Fallback
        let full_path = download_base.join(path);
        Some(full_path.to_string_lossy().to_string())
    }
}
