//! Offline library inventory and recommendations. All filesystem work runs off-frame.
use super::{continuity, disk, entry_key, history_prefix, list_folder, matching_size_resume,
    media_folder_score, read_history, system_mount, unpack_progress, volume_facts, Kind, Volume, VolumeFacts,
    FAVORITE, WATCHED};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
pub(super) struct Stats {
    pub movies: usize,
    pub series: usize,
    pub documentaries: usize,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Video {
    pub path: PathBuf,
    pub size: u64,
    pub modified: i64,
    pub show: Option<PathBuf>,
    category: usize,
}

#[derive(Clone, Serialize, Deserialize)]
struct Inventory {
    identity: String,
    checked: u64,
    videos: Vec<Video>,
    stats: Stats,
    used_bytes: Option<u64>,
    directories: Vec<Directory>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Directory {
    path: PathBuf,
    stamp: (u64, i64, i64),
}

pub(super) struct Drive {
    pub volume: Volume,
    pub facts: VolumeFacts,
    pub stats: Option<Stats>,
    pub videos: Vec<Video>,
    pub history: Option<BTreeMap<String, u64>>,
}

#[derive(Clone)]
pub(super) struct Item {
    pub volume: Volume,
    pub path: PathBuf,
    pub title: String,
    pub episode: Option<(u32, u32)>,
    pub favorite: bool,
    pub progress: u64,
}

pub(super) struct Dashboard {
    pub drives: Vec<Drive>,
    pub items: Vec<Item>,
}

fn seconds() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn cancelled(flag: &AtomicBool) -> io::Result<()> {
    if flag.load(Ordering::Relaxed) { Err(io::ErrorKind::Interrupted.into()) } else { Ok(()) }
}

fn cache_paths(identity: &str) -> Vec<PathBuf> {
    let key = super::hex(identity.bytes());
    plx_base::paths::image_cache_candidates().into_iter()
        .map(|dir| dir.join(format!("cineusb-library-{key}.json"))).collect()
}

fn cached(identity: &str) -> Option<Inventory> {
    for path in cache_paths(identity) {
        let Ok(bytes) = std::fs::read(path) else { continue };
        if bytes.len() > 16 * 1024 * 1024 { continue; }
        let Ok(value) = serde_json::from_slice::<Inventory>(&bytes) else { continue };
        if value.identity == identity && value.videos.iter().all(|video| {
            !video.path.as_os_str().is_empty() && video.path.components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
                && video.show.as_ref().is_none_or(|show| !show.as_os_str().is_empty()
                    && show.components().all(|part| matches!(part, std::path::Component::Normal(_))))
        }) && value.directories.iter().all(|dir| dir.path.components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))) { return Some(value); }
    }
    None
}

fn save(inventory: &Inventory) {
    use std::os::unix::fs::DirBuilderExt;
    let Ok(bytes) = serde_json::to_vec(inventory) else { return };
    for path in cache_paths(&inventory.identity) {
        let Some(parent) = path.parent() else { continue };
        if std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent).is_err() { continue; }
        if plx_plex::plex::session::write_atomic(&path, &bytes).is_ok() { break; }
    }
}

fn scan(volume: &Volume, identity: &str, favorites: &[PathBuf], stop: &AtomicBool) -> io::Result<Inventory> {
    let mut videos = Vec::new();
    let mut folders = vec![(volume.path.clone(), 0, None)];
    let mut shows = BTreeSet::new();
    let mut stats = Stats::default();
    let mut directories = Vec::new();
    while let Some((path, category, show)) = folders.pop() {
        cancelled(stop)?;
        let stamp = directory_stamps(volume, std::slice::from_ref(&path))?[0];
        directories.push(Directory { path: path.strip_prefix(&volume.path).map_err(io::Error::other)?.to_path_buf(), stamp });
        for entry in list_folder(&path, system_mount(volume))? {
            let child = path.join(&entry.name);
            if entry.kind == Kind::Folder {
                let name = entry.name.to_string_lossy();
                let next_category = [1, 2, super::DOCUMENTALS_TAB].into_iter()
                    .find(|&kind| media_folder_score(&name, kind) > 0).unwrap_or(category);
                let next_show = if next_category == 2 && next_category == category && show.is_none() {
                    Some(child.clone())
                } else {
                    show.clone().or_else(|| favorites.iter().find(|root| **root == child).cloned())
                };
                folders.push((child, next_category, next_show));
            } else {
                let relative = child.strip_prefix(&volume.path).map_err(io::Error::other)?.to_path_buf();
                let detected_show = show.clone().or_else(|| favorites.iter().find(|root| child.starts_with(root)).cloned())
                    .or_else(|| (category != super::DOCUMENTALS_TAB && continuity::episode(&entry.name).is_some())
                        .then(|| if path.file_name().is_some_and(|name| super::art::is_season(&name.to_string_lossy().to_lowercase())) {
                            path.parent().unwrap_or(&path).to_path_buf()
                        } else { path.clone() }));
                let relative_show = detected_show.and_then(|show| show.strip_prefix(&volume.path).ok().map(Path::to_path_buf))
                    .filter(|show| !show.as_os_str().is_empty());
                if let Some(show) = &relative_show { shows.insert(show.clone()); }
                else if category == super::DOCUMENTALS_TAB { stats.documentaries += 1; }
                else { stats.movies += 1; }
                videos.push(Video { path: relative, size: entry.size.unwrap_or(0), modified: entry.modified.unwrap_or(0),
                    show: relative_show, category });
            }
        }
    }
    cancelled(stop)?;
    stats.series = shows.len();
    videos.sort_by(|a, b| episode_order(&a.path, &b.path));
    Ok(Inventory { identity: identity.into(), checked: seconds(), videos, stats, used_bytes: None, directories })
}

fn directory_stamps(volume: &Volume, paths: &[PathBuf]) -> io::Result<Vec<(u64, i64, i64)>> {
    #[cfg(not(feature = "hostsim"))]
    if let Some(mount) = system_mount(volume) {
        return crate::port::usb_folder_stamps(paths, mount).map_err(|error| io::Error::other(format!("{error:?}")));
    }
    use std::os::unix::fs::MetadataExt;
    let _ = volume;
    paths.iter().map(|path| {
        let meta = std::fs::symlink_metadata(path)?;
        if !meta.is_dir() || meta.file_type().is_symlink() { return Err(io::ErrorKind::InvalidData.into()); }
        Ok((meta.ino(), meta.mtime(), meta.mtime_nsec()))
    }).collect()
}

fn inventory_changed(volume: &Volume, inventory: &Inventory, stop: &AtomicBool) -> io::Result<bool> {
    for chunk in inventory.directories.chunks(64) {
        cancelled(stop)?;
        let paths: Vec<_> = chunk.iter().map(|dir| volume.path.join(&dir.path)).collect();
        let stamps = directory_stamps(volume, &paths)?;
        if chunk.iter().zip(stamps).any(|(dir, stamp)| dir.stamp != stamp) { return Ok(true); }
    }
    Ok(inventory.directories.is_empty())
}

fn episode_order(a: &Path, b: &Path) -> std::cmp::Ordering {
    let episode = |path: &Path| {
        if let Some((_, season, episode)) = path.file_name().and_then(continuity::episode) { return (season, episode); }
        let number = |text: &str| text.split(|ch: char| !ch.is_ascii_digit())
            .find(|part| !part.is_empty()).and_then(|part| part.parse::<u32>().ok()).unwrap_or(0);
        let season = path.parent().and_then(|parent| parent.components().rev().find_map(|part| {
            let name = part.as_os_str().to_string_lossy().to_lowercase();
            super::art::is_season(&name).then(|| number(&name))
        })).unwrap_or(0);
        (season, number(&path.file_stem().unwrap_or_default().to_string_lossy()))
    };
    episode(a).cmp(&episode(b))
        .then_with(|| disk::natural(a.as_os_str().as_encoded_bytes(), b.as_os_str().as_encoded_bytes()))
}

fn record_path(key: &str, identity: &str) -> Option<PathBuf> {
    let tail = key.strip_prefix(&history_prefix(identity))?;
    let end = tail.as_bytes().chunks_exact(2).position(|pair| pair == b"00")?;
    super::decode_relative(&tail[..end * 2])
}

fn progress(video: &Video, volume: &Volume, identity: &str, history: &BTreeMap<String, u64>) -> u64 {
    let Some(key) = entry_key(identity, volume, &volume.path.join(&video.path), video.size, video.modified) else { return 0 };
    history.get(&key).copied().or_else(|| matching_size_resume(history, &key).map(|(_, value)| value))
        .or_else(|| history.iter().find_map(|(key, value)| {
            (*value == WATCHED && record_path(key, identity).as_ref() == Some(&video.path)).then_some(WATCHED)
        })).unwrap_or(0)
}

pub(super) fn recommendations(drive: &Drive) -> Vec<Item> {
    let Some(identity) = drive.facts.1.as_deref() else { return vec![] };
    let Some(history) = &drive.history else { return vec![] };
    let mut items = Vec::new();
    let mut represented = BTreeSet::new();
    let favorites: Vec<_> = history.iter().filter(|(_, value)| **value == FAVORITE)
        .filter_map(|(key, _)| record_path(key, identity)).filter(|path| !super::is_video_path(path)).collect();
    for show in favorites {
        let candidates: Vec<_> = drive.videos.iter().filter(|video| video.path.starts_with(&show)).collect();
        if candidates.is_empty() { continue; }
        let last = history.iter().filter(|(_, value)| **value == WATCHED)
            .filter_map(|(key, _)| record_path(key, identity)).filter(|path| path.starts_with(&show))
            .max_by(|a, b| episode_order(a, b));
        let next = candidates.into_iter().filter(|video| progress(video, &drive.volume, identity, history) != WATCHED)
            .filter(|video| last.as_ref().is_none_or(|last| episode_order(&video.path, last).is_gt()))
            .min_by(|a, b| episode_order(&a.path, &b.path));
        if let Some(video) = next {
            represented.insert(show.clone());
            items.push(item(drive, video, Some(&show), true, progress(video, &drive.volume, identity, history)));
        }
    }
    for video in &drive.videos {
        if items.iter().any(|item| item.path == drive.volume.path.join(&video.path))
            || video.show.as_ref().is_some_and(|show| represented.contains(show)) { continue; }
        let saved = progress(video, &drive.volume, identity, history);
        let (position, duration) = unpack_progress(saved);
        if saved == WATCHED || saved == FAVORITE || position == 0 || (duration > 0 && position / 1_000_000_000 >= duration) { continue; }
        if let Some(show) = &video.show {
            if !represented.insert(show.clone()) { continue; }
        }
        items.push(item(drive, video, video.show.as_deref(), false, saved));
    }
    items
}

pub(super) fn next_episode(drive: &Drive, current: &Path) -> Option<PathBuf> {
    let current = current.strip_prefix(&drive.volume.path).ok()?;
    let show = drive.videos.iter().find(|video| video.path == current)?.show.as_ref()?;
    drive.videos.iter().filter(|video| video.show.as_ref() == Some(show) && episode_order(&video.path, current).is_gt())
        .min_by(|a, b| episode_order(&a.path, &b.path)).map(|video| drive.volume.path.join(&video.path))
}

fn item(drive: &Drive, video: &Video, show: Option<&Path>, favorite: bool, progress: u64) -> Item {
    let title = show.and_then(Path::file_name).or_else(|| video.path.file_stem()).unwrap_or_default().to_string_lossy().into_owned();
    Item { volume: drive.volume.clone(), path: drive.volume.path.join(&video.path), title, favorite, progress,
        episode: video.path.file_name().and_then(continuity::episode).map(|(_, s, e)| (s, e)) }
}

pub(super) fn load(volumes: Vec<Volume>, refresh: bool, stop: &AtomicBool) -> io::Result<Dashboard> {
    let mut drives = Vec::new();
    let mut items = Vec::new();
    for volume in volumes {
        cancelled(stop)?;
        let facts = volume_facts(&volume);
        let identity = facts.1.clone();
        let history = identity.as_deref().and_then(|id| read_history(&history_prefix(id)));
        let favorites = identity.as_deref().zip(history.as_ref()).map(|(id, records)| records.iter()
            .filter(|(_, value)| **value == FAVORITE).filter_map(|(key, _)| record_path(key, id))
            .filter(|path| !super::is_video_path(path))
            .map(|path| volume.path.join(path)).collect::<Vec<_>>()).unwrap_or_default();
        let mut inventory = identity.as_deref().and_then(cached);
        let used_bytes = facts.0.map(|(_, used, _, _)| used);
        if refresh || inventory.as_ref().is_none_or(|value| value.used_bytes != used_bytes
            || inventory_changed(&volume, value, stop).unwrap_or(true)) {
                match scan(&volume, identity.as_deref().unwrap_or("volatile"), &favorites, stop) {
                    Ok(mut value) => {
                        value.used_bytes = used_bytes;
                        if identity.is_some() { save(&value); }
                        inventory = Some(value);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => return Err(error),
                    Err(_) => inventory = None,
                }
        }
        let mut drive = Drive { volume, facts, stats: inventory.as_ref().map(|value| value.stats),
            videos: inventory.map(|value| value.videos).unwrap_or_default(), history };
        // Recompute after pruning missing files so deleted seasons can expose
        // a recommendation in another parent directory immediately.
        let mut listings = BTreeMap::new();
        loop {
            let proposed = recommendations(&drive);
            for item in &proposed {
                cancelled(stop)?;
                if let Some(parent) = item.path.parent() {
                    listings.entry(parent.to_path_buf()).or_insert_with(|| list_folder(parent, system_mount(&drive.volume)));
                }
            }
            let before = drive.videos.len();
            drive.videos.retain(|video| {
                let path = drive.volume.path.join(&video.path);
                let Some(parent) = path.parent() else { return false };
                match listings.get(parent) {
                    Some(Ok(entries)) => entries.iter().any(|entry| entry.kind == Kind::Video && Some(entry.name.as_os_str()) == path.file_name()),
                    Some(Err(_)) => false,
                    None => true,
                }
            });
            if drive.videos.len() == before {
                items.extend(proposed);
                break;
            }
        }
        drives.push(drive);
    }
    items.sort_by_key(|item| !item.favorite);
    items.truncate(12);
    Ok(Dashboard { drives, items })
}

#[cfg(test)]
mod usb_browser_tests {
    use super::*;
    use crate::app::local_usb::{favorite_key, pack_progress};
    use std::ffi::OsString;

    fn fixture(label: &str) -> (PathBuf, Volume) {
        let root = plx_base::testscratch::process_dir(label);
        for name in [
            "Pel·lícules/Una película.mkv", "Documentals/Un documental.mp4",
            "Sèries/Una sèrie/Season 1/Show S01E01.mkv",
            "Sèries/Una sèrie/Season 2/Show S02E01.mkv",
            "Sèries/Una sèrie/Season 2/Show S02E10.mkv",
            "Sèries/Una sèrie/Season 3/Show S03E01.mkv",
        ] {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, []).unwrap();
        }
        let volume = Volume { path: root.clone(), label: Some(OsString::from("Biblioteca")), mount_id: None, system: false };
        (root, volume)
    }

    fn drive(volume: Volume, history: BTreeMap<String, u64>) -> Drive {
        let inventory = scan(&volume, "uuid:test", &[], &AtomicBool::new(false)).unwrap();
        Drive { volume, facts: (None, Some("uuid:test".into())), stats: Some(inventory.stats), videos: inventory.videos, history: Some(history) }
    }

    #[test]
    fn deleted_completed_seasons_advance_without_erasing_the_favorite() {
        let (root, volume) = fixture("cineusb-home-deleted-seasons");
        let show = root.join("Sèries/Una sèrie");
        let favorite = favorite_key("uuid:test", &volume, &show).unwrap();
        let watched = entry_key("uuid:test", &volume, &show.join("Season 1/Show S01E01.mkv"), 0, 0).unwrap();
        std::fs::remove_dir_all(show.join("Season 1")).unwrap();
        let mut drive = drive(volume, BTreeMap::from([(favorite.clone(), FAVORITE), (watched, WATCHED)]));
        let first = recommendations(&drive);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].episode, Some((2, 1)));
        let watched = entry_key("uuid:test", &drive.volume, &show.join("Season 2/Show S02E10.mkv"), 0, 0).unwrap();
        drive.history.as_mut().unwrap().insert(watched, WATCHED);
        drive.videos.retain(|video| !video.path.starts_with("Sèries/Una sèrie/Season 2"));
        assert_eq!(recommendations(&drive)[0].episode, Some((3, 1)));
        drive.videos.retain(|video| video.show.is_none());
        assert!(recommendations(&drive).is_empty());
        assert_eq!(drive.history.as_ref().unwrap().get(&favorite), Some(&FAVORITE));
        plx_base::testscratch::remove_tree(&root);
    }

    #[test]
    fn statistics_deduplicate_shows_and_recommendations_prioritize_favorites() {
        let (root, volume) = fixture("cineusb-home-library-stats");
        let show = root.join("Sèries/Una sèrie");
        let favorite = favorite_key("uuid:test", &volume, &show).unwrap();
        let mut drive = drive(volume, BTreeMap::from([(favorite, FAVORITE)]));
        let stats = drive.stats.unwrap();
        assert_eq!((stats.movies, stats.series, stats.documentaries), (1, 1, 1));
        let movie = drive.videos.iter().find(|video| video.category == 1).unwrap();
        let key = entry_key("uuid:test", &drive.volume, &root.join(&movie.path), movie.size, movie.modified).unwrap();
        drive.history.as_mut().unwrap().insert(key, pack_progress(60_000_000_000, Some(600_000_000_000)));
        let items = recommendations(&drive);
        assert_eq!(items.len(), 2);
        assert!(items[0].favorite);
        assert_eq!(items[0].episode, Some((1, 1)));
        assert_eq!(unpack_progress(items[1].progress), (60_000_000_000, 600));
        plx_base::testscratch::remove_tree(&root);
    }

    #[test]
    fn relative_inventory_and_history_survive_device_name_changes() {
        let (root, volume) = fixture("cineusb-home-device-identity");
        let favorite = favorite_key("uuid:test", &volume, &root.join("Sèries/Una sèrie")).unwrap();
        let mut drive = drive(volume, BTreeMap::from([(favorite, FAVORITE)]));
        let original = recommendations(&drive)[0].episode;
        drive.volume.path = "/tmp/usb/sdb/sdb1".into();
        assert_eq!(recommendations(&drive)[0].episode, original);
        assert!(recommendations(&drive)[0].path.starts_with("/tmp/usb/sdb/sdb1"));
        let inventory = Inventory { identity: "uuid:test".into(), checked: seconds(), stats: drive.stats.unwrap(),
            videos: drive.videos.clone(), used_bytes: Some(200), directories: vec![] };
        let restored: Inventory = serde_json::from_slice(&serde_json::to_vec(&inventory).unwrap()).unwrap();
        assert_eq!(restored.videos.len(), drive.videos.len());
        assert_eq!(restored.stats.series, 1);
        plx_base::testscratch::remove_tree(&root);
    }

    #[test]
    fn cancelled_home_inventory_does_not_publish_an_incomplete_library() {
        let (root, volume) = fixture("cineusb-home-cancelled");
        assert_eq!(scan(&volume, "uuid:test", &[], &AtomicBool::new(true)).err().unwrap().kind(), io::ErrorKind::Interrupted);
        plx_base::testscratch::remove_tree(&root);
    }

    #[test]
    fn directory_fingerprints_detect_added_and_deleted_content_without_a_full_scan() {
        let (root, volume) = fixture("cineusb-home-changes");
        let inventory = scan(&volume, "uuid:test", &[], &AtomicBool::new(false)).unwrap();
        assert!(!inventory_changed(&volume, &inventory, &AtomicBool::new(false)).unwrap());
        let added = root.join("Sèries/Una sèrie/Season 3/Show S03E02.mkv");
        std::fs::write(&added, []).unwrap();
        assert!(inventory_changed(&volume, &inventory, &AtomicBool::new(false)).unwrap());
        let refreshed = scan(&volume, "uuid:test", &[], &AtomicBool::new(false)).unwrap();
        assert_eq!(refreshed.videos.len(), inventory.videos.len() + 1);
        std::fs::remove_file(&added).unwrap();
        assert!(inventory_changed(&volume, &refreshed, &AtomicBool::new(false)).unwrap());
        plx_base::testscratch::remove_tree(&root);
    }
}
