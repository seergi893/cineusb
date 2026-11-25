//! Persistent metadata from successful USB video probes.
use std::fs::{self, File};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Read as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};

use plx_play_feed::LocalVideoDetails;
use serde::{Deserialize, Serialize};

use super::{DetailKey, Volume};

const MAX_RECORD: u64 = 64 * 1024;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Key {
    identity: String,
    relative_path: String,
    size: u64,
    modified: i64,
}

#[derive(Deserialize)]
struct Record {
    key: Key,
    details: LocalVideoDetails,
}

impl Key {
    pub(super) fn new(identity: &str, volume: &Volume, detail: &DetailKey) -> Option<Self> {
        let relative = detail.0.strip_prefix(&volume.path).ok()?;
        if identity.is_empty() || relative.as_os_str().is_empty() { return None; }
        Some(Self {
            identity: identity.into(),
            relative_path: super::hex(relative.as_os_str().as_encoded_bytes().iter().copied()),
            size: detail.1?,
            modified: detail.2?,
        })
    }

    fn paths(&self) -> Vec<PathBuf> {
        // Replacing a file at the same path replaces its record. The full key
        // in the record also guards against a collision in the filename hash.
        let mut hash = DefaultHasher::new();
        (&self.identity, &self.relative_path).hash(&mut hash);
        let name = format!("{:016x}.json", hash.finish());
        plx_base::paths::image_cache_candidates().into_iter()
            .map(|dir| dir.join("cineusb-video-details-v1").join(&name))
            .collect()
    }

    fn matches_file(&self, file: &File) -> bool {
        file.metadata().is_ok_and(|meta| meta.len() == self.size && meta.mtime() == self.modified)
    }

    fn read(&self, path: &Path) -> Option<LocalVideoDetails> {
        let mut bytes = Vec::new();
        File::open(path).ok()?.take(MAX_RECORD + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() as u64 > MAX_RECORD { return None; }
        let record: Record = serde_json::from_slice(&bytes).ok()?;
        (record.key == *self && record.details.resolution.is_some()).then_some(record.details)
    }

    fn encode(&self, details: &LocalVideoDetails) -> Option<Vec<u8>> {
        #[derive(Serialize)]
        struct BorrowedRecord<'a> { key: &'a Key, details: &'a LocalVideoDetails }
        let bytes = serde_json::to_vec(&BorrowedRecord { key: self, details }).ok()?;
        (bytes.len() as u64 <= MAX_RECORD).then_some(bytes)
    }
}

pub(super) fn probe(key: Option<&Key>, open: impl FnOnce() -> Result<File, String>) -> LocalVideoDetails {
    let paths = key.map_or_else(Vec::new, Key::paths);
    if let Some(details) = key.and_then(|key| paths.iter().find_map(|path| key.read(path))) {
        return details;
    }
    let Ok(file) = open() else { return LocalVideoDetails::default() };
    let cache_key = key.filter(|key| key.matches_file(&file));
    let details = plx_play_feed::probe_local_details(&file);
    if let Some(key) = cache_key.filter(|key| details.resolution.is_some() && key.matches_file(&file)) {
        if let Some(bytes) = key.encode(&details) {
            for path in paths {
                let Some(parent) = path.parent() else { continue };
                if fs::DirBuilder::new().recursive(true).mode(0o700).create(parent).is_err() { continue; }
                if plx_plex::plex::session::write_atomic(&path, &bytes).is_ok() { break; }
            }
        }
    }
    details
}

#[cfg(test)]
mod usb_browser_tests {
    use super::*;

    #[test]
    fn metadata_survives_a_new_mount_and_detects_changed_files() {
        let root = plx_base::testscratch::process_dir("cineusb-details-cache");
        let mount = |name| Volume { path: root.join(name), label: None, mount_id: None, system: false };
        let first = mount("first");
        let next = mount("next");
        let detail = |volume: &Volume, size, modified| (volume.path.join("Series/episode.mkv"), Some(size), Some(modified));
        let original = Key::new("disk-a", &first, &detail(&first, 1024, 40)).unwrap();
        let path = root.join("metadata.json");
        let details = LocalVideoDetails { resolution: Some((3840, 2160)), duration_secs: Some(3600), ..Default::default() };
        fs::write(&path, original.encode(&details).unwrap()).unwrap();
        let restored = Key::new("disk-a", &next, &detail(&next, 1024, 40)).unwrap();
        assert_eq!(restored.read(&path).unwrap().resolution, details.resolution);
        assert_eq!(restored.paths(), original.paths());
        for (identity, size, modified) in [("disk-b", 1024, 40), ("disk-a", 2048, 40), ("disk-a", 1024, 41)] {
            assert!(Key::new(identity, &next, &detail(&next, size, modified)).unwrap().read(&path).is_none());
        }
        plx_base::testscratch::remove_tree(&root);
    }
}
