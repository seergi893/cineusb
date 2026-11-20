//! Offline library inventory and recommendations. All filesystem work runs off-frame.
use super::{DetailKey, continuity, directory_stamps, disk, history_prefix, list_folder,
    media_folder_score, read_history, system_mount, unpack_progress, volume_facts, Kind, Volume, VolumeFacts,
    FAVORITE, WATCHED};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub(super) struct Stats {
    pub movies: usize,
    pub series: usize,
    pub documentaries: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Video {
    pub path: PathBuf,
    pub size: u64,
    pub modified: i64,
    pub show: Option<PathBuf>,
    category: usize,
}

#[derive(Clone, Serialize, Deserialize)]
struct Inventory {
    #[serde(default)]
    schema: u8,
    identity: String,
    checked: u64,
    videos: Vec<Video>,
    stats: Stats,
    used_bytes: Option<u64>,
    directories: Vec<Directory>,
    favorite_series: Vec<PathBuf>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Directory {
    path: PathBuf,
    stamp: (u64, i64, i64),
}

#[derive(Debug)]
pub(super) struct Drive {
    pub volume: Volume,
    pub facts: VolumeFacts,
    pub stats: Option<Stats>,
    pub videos: Vec<Video>,
    pub history: Option<BTreeMap<String, u64>>,
}

#[derive(Clone, Debug)]
pub(super) struct Item {
    pub volume: Volume,
    pub path: PathBuf,
    pub title: String,
    pub episode: Option<(u32, u32)>,
    pub show: bool,
    pub favorite: bool,
    pub progress: u64,
    pub detail_key: DetailKey,
}

#[derive(Debug)]
pub(super) struct Dashboard {
    pub drives: Vec<Drive>,
    pub items: Vec<Item>,
    pub refreshed_folder: Option<(Volume, PathBuf, Vec<disk::Entry>)>,
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
        if value.schema == 2 && value.identity == identity && value.videos.iter().all(|video| {
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
    let mut documentaries = BTreeSet::new();
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
                let next_category = if category == super::DOCUMENTALS_TAB { category } else {
                    [1, 2, super::DOCUMENTALS_TAB].into_iter()
                        .find(|&kind| media_folder_score(&name, kind) > 0).unwrap_or(category)
                };
                let next_show = if show.is_none() && next_category != 1 && continuity::season_number(&name).is_some() {
                    Some(path.clone())
                } else if next_category == 2 && next_category == category && show.is_none() {
                    Some(child.clone())
                } else {
                    show.clone().or_else(|| favorites.iter().find(|root| **root == child).cloned())
                };
                folders.push((child, next_category, next_show));
            } else {
                let relative = child.strip_prefix(&volume.path).map_err(io::Error::other)?.to_path_buf();
                let detected_show = show.clone().or_else(|| favorites.iter().find(|root| child.starts_with(root)).cloned())
                    .or_else(|| continuity::episode(&entry.name).and_then(|_| {
                        let name = path.file_name()?.to_string_lossy();
                        if super::art::is_season(&name.to_lowercase()) {
                            path.parent().map(Path::to_path_buf)
                        } else if category == 0 || media_folder_score(&name, category) == 0 {
                            Some(path.clone())
                        } else { None }
                    }));
                let relative_show = detected_show.filter(|_| category != 1).and_then(|show| show.strip_prefix(&volume.path).ok().map(Path::to_path_buf))
                    .filter(|show| !show.as_os_str().is_empty());
                if category == super::DOCUMENTALS_TAB {
                    if let Some(show) = &relative_show { documentaries.insert(show.clone()); }
                    else { stats.documentaries += 1; }
                }
                else if let Some(show) = &relative_show { shows.insert(show.clone()); }
                else if category != 1 && category != super::DOCUMENTALS_TAB && continuity::episode(&entry.name).is_some() {
                    let (prefix, _, _) = continuity::episode(&entry.name).expect("episode already detected");
                    shows.insert(PathBuf::from(String::from_utf8_lossy(prefix).as_ref()));
                }
                else { stats.movies += 1; }
                videos.push(Video { path: relative, size: entry.size.unwrap_or(0), modified: entry.modified.unwrap_or(0),
                    show: relative_show, category });
            }
        }
    }
    cancelled(stop)?;
    stats.series = shows.len();
    stats.documentaries += documentaries.len();
    videos.sort_by(episode_order);
    let favorite_series = favorites.iter().filter_map(|path| path.strip_prefix(&volume.path).ok().map(Path::to_path_buf)).collect();
    Ok(Inventory { schema: 2, identity: identity.into(), checked: seconds(), videos, stats, used_bytes: None, directories, favorite_series })
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

pub(super) fn episode_numbers(path: &Path, known: bool) -> Option<(u32, u32)> {
    continuity::identify(path, known).map(|episode| (episode.season, episode.first))
}

fn episode_order(a: &Video, b: &Video) -> std::cmp::Ordering {
    numbers(a).cmp(&numbers(b))
        .then_with(|| disk::natural(a.path.as_os_str().as_encoded_bytes(), b.path.as_os_str().as_encoded_bytes()))
        .then_with(|| a.path.cmp(&b.path))
}

fn numbers(video: &Video) -> Option<continuity::Episode> {
    let path = video.show.as_ref().and_then(|show| video.path.strip_prefix(show).ok()).unwrap_or(&video.path);
    (video.category != 1).then(|| continuity::identify(path, video.show.is_some())).flatten()
}

fn record_path(key: &str, identity: &str) -> Option<PathBuf> {
    let tail = key.strip_prefix(&history_prefix(identity))?;
    let end = tail.as_bytes().chunks_exact(2).position(|pair| pair == b"00")?;
    super::decode_relative(&tail[..end * 2])
}

fn progress(video: &Video, volume: &Volume, identity: &str, history: &BTreeMap<String, u64>) -> u64 {
    let key = (volume.path.join(&video.path), Some(video.size), Some(video.modified));
    super::file_record(history, identity, volume, &key)
}

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
struct SeriesKey { folder: PathBuf, flat_title: Option<String> }

impl SeriesKey {
    fn for_video(video: &Video) -> Option<Self> {
        numbers(video)?;
        if let Some(show) = &video.show {
            return Some(Self { folder: show.clone(), flat_title: None });
        }
        let (prefix, _, _) = continuity::episode(video.path.file_name()?)?;
        Some(Self { folder: video.path.parent()?.into(), flat_title: Some(normalized_prefix(prefix)?) })
    }

    fn identify(&self, path: &Path) -> Option<continuity::Episode> {
        if let Some(title) = &self.flat_title {
            if path.parent()? != self.folder { return None; }
            let (prefix, _, _) = continuity::episode(path.file_name()?)?;
            if normalized_prefix(prefix).as_ref() != Some(title) { return None; }
        } else if !path.starts_with(&self.folder) { return None; }
        let path = if self.flat_title.is_none() { path.strip_prefix(&self.folder).ok()? } else { path };
        continuity::identify(path, self.flat_title.is_none())
    }

    fn title(&self) -> Option<&str> {
        self.flat_title.as_deref().or_else(|| self.folder.file_name()?.to_str())
    }
}

fn normalized_prefix(prefix: &[u8]) -> Option<String> {
    let prefix = std::str::from_utf8(prefix).ok()?;
    let title = prefix.split(['.', '_', '-', ' ']).filter(|part| !part.is_empty())
        .collect::<Vec<_>>().join(" ").to_lowercase();
    (!title.is_empty()).then_some(title)
}

fn has_resume(saved: u64) -> bool {
    let (position, duration) = unpack_progress(saved);
    position > 0 && (duration == 0 || position < duration)
}

#[allow(clippy::too_many_arguments)]
fn next_in_series<'a>(drive: &Drive, series: &SeriesKey, candidates: &[&'a Video],
    identity: &str, records: &BTreeMap<String, u64>, history: &BTreeMap<PathBuf, u64>,
    completed: Option<&Path>, favorite: bool)
    -> Option<(&'a Video, u64)> {
    let mut watched = BTreeSet::new();
    let mut paused = Vec::new();
    let mut record = |path: &Path, saved: u64| {
        let Some(episode) = series.identify(path).filter(|episode| episode.season > 0) else { return; };
        if saved == WATCHED {
            watched.extend((episode.first..=episode.last).map(|number| (episode.season, number)));
        } else if has_resume(saved) { paused.push(episode); }
    };
    for (path, &saved) in history { record(path.as_path(), saved); }
    if let Some(path) = completed { record(path, WATCHED); }
    let mut best: Option<((u32, u32), u64, &Video)> = None;
    for &video in candidates {
        let Some(episode) = series.identify(&video.path).filter(|episode| episode.season > 0) else { continue; };
        let saved = progress(video, &drive.volume, identity, records);
        if saved == WATCHED { continue; }
        let Some(first_unwatched) = (episode.first..=episode.last).find(|number| !watched.contains(&(episode.season, *number))) else { continue; };
        let key = (episode.season, first_unwatched);
        if best.as_ref().is_none_or(|(old_key, old_saved, old_video)| {
            (key, !has_resume(saved), &video.path) < (*old_key, !has_resume(*old_saved), &old_video.path)
        }) {
            best = Some((key, saved, video));
        }
    }
    let (key, saved, video) = best?;
    // A removed but unfinished earlier episode cannot be treated as complete.
    if paused.iter().any(|episode| (episode.season, episode.first) < key
        && (episode.first..=episode.last).any(|number| !watched.contains(&(episode.season, number)))) { return None; }
    (has_resume(saved) || !watched.is_empty() || favorite && key == (1, 1)).then_some((video, saved))
}

fn grouped_history<'a>(drive: &Drive, identity: &str, groups: impl Iterator<Item = &'a SeriesKey>,
    records: &BTreeMap<String, u64>) -> BTreeMap<SeriesKey, BTreeMap<PathBuf, u64>> {
    let groups: BTreeSet<_> = groups.cloned().collect();
    let mut partitioned = BTreeMap::<SeriesKey, BTreeMap<PathBuf, u64>>::new();
    for (key, &saved) in records {
        if saved != WATCHED && !has_resume(saved) { continue; }
        let Some(path) = record_path(key, identity) else { continue; };
        let flat = path.file_name().and_then(continuity::episode).and_then(|(prefix, _, _)| Some(SeriesKey {
            folder: path.parent()?.into(), flat_title: Some(normalized_prefix(prefix)?),
        }));
        let group = path.ancestors().skip(1).find_map(|folder|
            groups.get(&SeriesKey { folder: folder.into(), flat_title: None }))
            .or_else(|| groups.get(&flat?));
        if let Some(group) = group {
            let value = partitioned.entry(group.clone()).or_default().entry(path).or_default();
            *value = (*value).max(saved);
        }
    }
    // The exact current record wins over stale copies of the same path.
    for video in &drive.videos {
        if let Some(group) = SeriesKey::for_video(video).filter(|group| groups.contains(group)) {
            let saved = progress(video, &drive.volume, identity, records);
            let group = partitioned.entry(group).or_default();
            if saved == WATCHED || has_resume(saved) { group.insert(video.path.clone(), saved); }
            else { group.remove(&video.path); }
        }
    }
    partitioned
}

pub(super) fn recommendations(drive: &Drive) -> Vec<Item> {
    let Some(identity) = drive.facts.1.as_deref() else { return vec![] };
    let Some(history) = &drive.history else { return vec![] };
    let favorites: BTreeSet<_> = history.iter().filter(|(_, value)| **value == FAVORITE)
        .filter_map(|(key, _)| record_path(key, identity)).collect();
    let mut groups = BTreeMap::<SeriesKey, Vec<&Video>>::new();
    let mut items = Vec::new();
    for video in &drive.videos {
        if let Some(key) = SeriesKey::for_video(video).filter(|_| numbers(video).is_some_and(|e| e.season > 0)) {
            groups.entry(key).or_default().push(video);
        } else {
            let saved = progress(video, &drive.volume, identity, history);
            if has_resume(saved) { items.push(item(drive, video, None, false, saved)); }
        }
    }
    let empty = BTreeMap::new();
    let histories = grouped_history(drive, identity, groups.keys(), history);
    for (series, videos) in groups {
        let favorite = series.flat_title.is_none() && favorites.contains(&series.folder);
        if let Some((video, saved)) = next_in_series(drive, &series, &videos, identity, history,
            histories.get(&series).unwrap_or(&empty), None, favorite) {
            items.push(item(drive, video, series.title(), favorite, saved));
        }
    }
    items.sort_by(|a, b| (!a.favorite, &a.path).cmp(&(!b.favorite, &b.path)));
    items
}

pub(super) fn next_episode(drive: &Drive, current: &Path, current_history: Option<&BTreeMap<String, u64>>) -> Option<PathBuf> {
    let current = current.strip_prefix(&drive.volume.path).ok()?;
    let video = drive.videos.iter().find(|video| video.path == current)?;
    let series = SeriesKey::for_video(video)?;
    series.identify(current).filter(|episode| episode.season > 0)?;
    let candidates: Vec<_> = drive.videos.iter().filter(|video| SeriesKey::for_video(video).as_ref() == Some(&series)).collect();
    let identity = drive.facts.1.as_deref()?;
    let records = current_history.or(drive.history.as_ref())?;
    let history = grouped_history(drive, identity, std::iter::once(&series), records);
    let empty = BTreeMap::new();
    let (next, _) = next_in_series(drive, &series, &candidates, identity, records,
        history.get(&series).unwrap_or(&empty), Some(current), false)?;
    Some(drive.volume.path.join(&next.path))
}

fn item(drive: &Drive, video: &Video, show: Option<&str>, favorite: bool, progress: u64) -> Item {
    let title = show.map(str::to_owned).unwrap_or_else(|| video.path.file_stem().unwrap_or_default().to_string_lossy().into_owned());
    let path = drive.volume.path.join(&video.path);
    let detail_key = (path.clone(), Some(video.size), Some(video.modified));
    Item { volume: drive.volume.clone(), path, title, favorite, progress, detail_key,
        episode: numbers(video).map(|e| (e.season, e.first)), show: show.is_some() }
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
        let relative_favorites: Vec<_> = favorites.iter().filter_map(|path| path.strip_prefix(&volume.path).ok().map(Path::to_path_buf)).collect();
        let used_bytes = facts.0.map(|(_, used, _, _)| used);
        if refresh || inventory.as_ref().is_none_or(|value| value.used_bytes != used_bytes || value.favorite_series != relative_favorites
            || inventory_changed(&volume, value, stop).unwrap_or(true)) {
                match scan(&volume, identity.as_deref().unwrap_or("volatile"), &favorites, stop) {
                    Ok(mut value) => {
                        value.used_bytes = used_bytes;
                        if identity.is_some() { save(&value); }
                        inventory = Some(value);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => return Err(error),
                    Err(error) => return Err(error),
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
    Ok(Dashboard { drives, items, refreshed_folder: None })
}

#[cfg(test)]
mod usb_browser_tests {
    use super::*;
    use crate::app::local_usb::{entry_key, favorite_key, pack_progress};
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
        assert_eq!(unpack_progress(items[1].progress), (60_000_000_000, 600_000_000_000));
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
        let inventory = Inventory { schema: 2, identity: "uuid:test".into(), checked: seconds(), stats: drive.stats.unwrap(),
            videos: drive.videos.clone(), used_bytes: Some(200), directories: vec![], favorite_series: vec![] };
        let cache = root.join("offline-inventory.json");
        plx_plex::plex::session::write_atomic(&cache, &serde_json::to_vec(&inventory).unwrap()).unwrap();
        let restored: Inventory = serde_json::from_slice(&std::fs::read(cache).unwrap()).unwrap();
        assert_eq!(restored.videos.len(), drive.videos.len());
        assert_eq!(restored.stats.series, 1);
        plx_base::testscratch::remove_tree(&root);
    }

    #[test]
    fn documentary_series_keeps_episode_identity_and_cross_season_order() {
        let (root, volume) = fixture("cineusb-home-documentary-series");
        let show = root.join("Documentals/Nuestro.Planeta.2019.1080p");
        let first = show.join("Season 1/1x01.mkv");
        let second = show.join("Season 2/El mundo 2x01.mkv");
        for episode in [&first, &second] {
            std::fs::create_dir_all(episode.parent().unwrap()).unwrap();
            std::fs::write(episode, []).unwrap();
        }
        let mut drive = drive(volume, BTreeMap::new());
        let first_entry = drive.videos.iter().find(|video| root.join(&video.path) == first).unwrap();
        assert_eq!(first_entry.category, super::super::DOCUMENTALS_TAB);
        assert_eq!(next_episode(&drive, &first, None), Some(second));
        let key = entry_key("uuid:test", &drive.volume, &first, first_entry.size, first_entry.modified).unwrap();
        drive.history.as_mut().unwrap().insert(key, pack_progress(60_000_000_000, Some(600_000_000_000)));
        let items = recommendations(&drive);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].episode, Some((1, 1)));
        assert!(items[0].show);
        assert_eq!(items[0].path, first);
        assert_eq!(drive.stats.unwrap().documentaries, 2);
        plx_base::testscratch::remove_tree(&root);
    }

    #[test]
    fn flat_episodes_from_home_advance_to_the_same_title() {
        let (root, volume) = fixture("cineusb-home-flat-episodes");
        let category = root.join("Sèries");
        let current = category.join("Una S01E01.mkv");
        let next = category.join("Una S01E02.mkv");
        for path in [&current, &next, &category.join("Otra S01E02.mkv")] {
            std::fs::write(path, []).unwrap();
        }
        assert_eq!(next_episode(&drive(volume, BTreeMap::new()), &current, None), Some(next));
        plx_base::testscratch::remove_tree(&root);
    }

    #[test]
    fn season_folder_inference_ignores_release_resolution_and_year() {
        for file in ["Season 1/Pilot 1080p.mkv", "Season 1/Pilot 2026.mkv"] {
            assert_eq!(episode_numbers(Path::new(file), false), None);
        }
        for file in ["Season 1/01 - Pilot.mkv", "Season 1/Capítulo 01.mkv", "Season 1/Pilot 1x01.mkv"] {
            assert_eq!(episode_numbers(Path::new(file), false), Some((1, 1)));
        }
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

    #[test]
    fn forced_inventory_refresh_tracks_renames_deletes_and_recursive_counts() {
        let (root, volume) = fixture("cineusb-home-manual-edits");
        let show = root.join("Sèries/Una sèrie");
        let favorite = favorite_key("uuid:test", &volume, &show).unwrap();
        let mut before = drive(volume.clone(), BTreeMap::from([(favorite, FAVORITE)]));
        let first = root.join("Sèries/Una sèrie/Season 1/Show S01E01.mkv");
        let video = before.videos.iter().find(|video| volume.path.join(&video.path) == first).unwrap();
        let watched = entry_key("uuid:test", &volume, &first, video.size, video.modified).unwrap();
        before.history.as_mut().unwrap().insert(watched.clone(), WATCHED);
        std::fs::remove_dir_all(show.join("Season 1")).unwrap();
        std::fs::rename(root.join("Documentals/Un documental.mp4"), root.join("Documentals/Nou nom.mp4")).unwrap();
        std::fs::write(show.join("Season 3/Show S03E02.mkv"), []).unwrap();
        let refreshed = scan(&volume, "uuid:test", &[show.clone()], &AtomicBool::new(false)).unwrap();
        assert_eq!((refreshed.stats.movies, refreshed.stats.series, refreshed.stats.documentaries), (1, 1, 1));
        assert!(refreshed.videos.iter().any(|video| video.path.ends_with("Nou nom.mp4")));
        assert!(!refreshed.videos.iter().any(|video| video.path.ends_with("Un documental.mp4")));
        let counts = super::super::series::count_cancellable(&show, None, true, &AtomicBool::new(false)).unwrap().unwrap();
        assert_eq!(counts.counts.episodes, 4);
        before.videos = refreshed.videos;
        assert_eq!(recommendations(&before)[0].episode, Some((2, 1)));
        assert_eq!(before.history.as_ref().unwrap()[&watched], WATCHED);
        plx_base::testscratch::remove_tree(&root);
    }

    #[test]
    fn loose_episodes_are_not_counted_as_movies_and_natural_order_crosses_seasons() {
        assert_eq!(episode_numbers(Path::new("Temporada 2/Capítol 01.mkv"), false), Some((2, 1)));
        assert!(disk::natural(b"Episode 2", b"Episode 10").is_lt());
        let root = plx_base::testscratch::process_dir("cineusb-home-loose-episodes");
        for name in ["Show S02E10.mkv", "Show S02E02.mkv", "Show S03E01.mkv"] {
            std::fs::write(root.join(name), []).unwrap();
        }
        let volume = Volume { path: root.clone(), label: None, mount_id: None, system: false };
        let inventory = scan(&volume, "uuid:loose", &[], &AtomicBool::new(false)).unwrap();
        assert_eq!(inventory.stats.movies, 0);
        assert_eq!(inventory.stats.series, 1);
        assert_eq!(inventory.videos[0].path, PathBuf::from("Show S02E02.mkv"));
        assert_eq!(inventory.videos[2].path, PathBuf::from("Show S03E01.mkv"));
        let mut drive = drive(volume, BTreeMap::new());
        for video in drive.videos.iter().take(2) {
            let key = entry_key("uuid:test", &drive.volume, &root.join(&video.path), video.size, video.modified).unwrap();
            drive.history.as_mut().unwrap().insert(key, pack_progress(60_000_000_000, Some(600_000_000_000)));
        }
        assert_eq!(recommendations(&drive).len(), 1);
        plx_base::testscratch::remove_tree(&root);
    }
}
