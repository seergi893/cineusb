//! Bounded, asynchronous-only local USB filename search.
use super::{disk::{Kind, Volume}, folder_fold, list_folder, system_mount};
use std::path::PathBuf;

/// Enumerates only paths under the verified USB mount. The caller runs this off-frame.
/// No file contents are read, no mount is modified, and no symlink is followed by
/// the privileged USB listing backend.
pub(super) fn filenames(volume: &Volume, query: &str) -> Vec<PathBuf> {
    let needle = folder_fold(query);
    if needle.is_empty() { return Vec::new(); }
    let mut matches = Vec::new();
    let mut pending = vec![(volume.path.clone(), 0usize)];
    let mut visited = 0usize;
    while let Some((folder, depth)) = pending.pop() {
        if visited >= 512 || matches.len() >= 250 { break; }
        visited += 1;
        let Ok(entries) = list_folder(&folder, system_mount(volume)) else { continue };
        for entry in entries.into_iter().take(1500) {
            let path = folder.join(&entry.name);
            if entry.kind == Kind::Video && folder_fold(&entry.name.to_string_lossy()).contains(&needle) {
                matches.push(path.clone());
                if matches.len() >= 250 { break; }
            } else if entry.kind == Kind::Folder && depth < 5 && pending.len() < 512 {
                pending.push((path, depth + 1));
            }
        }
    }
    matches.sort();
    matches
}
