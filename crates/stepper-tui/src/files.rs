use std::path::Path;

/// One-level directory listing for the `@` picker: directories first (suffixed
/// `/` so the picker can drill into them), then files, both sorted, capped.
/// Works for any directory — inside the project, an absolute path, or `~`.
pub fn list_dir(dir: &Path, limit: usize) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if entry.path().is_dir() {
            dirs.push(format!("{name}/"));
        } else {
            files.push(name);
        }
    }
    dirs.sort();
    files.sort();
    dirs.extend(files);
    dirs.truncate(limit);
    dirs
}
