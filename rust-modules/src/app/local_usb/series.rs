use super::{art, continuity, disk::Kind, list_folder};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Counts {
    pub seasons: usize,
    pub episodes: usize,
}

#[cfg(test)]
pub(super) fn count(path: &Path, mount: Option<u64>, known_series: bool) -> io::Result<Option<Counts>> {
    count_cancellable(path, mount, known_series, &AtomicBool::new(false))
}

pub(super) fn count_cancellable(path: &Path, mount: Option<u64>, known_series: bool, cancelled: &AtomicBool) -> io::Result<Option<Counts>> {
    let mut counts = Counts::default();
    let mut folders = vec![(path.to_path_buf(), false, true)];
    let mut episodic = false;
    let mut recognized = false;
    while let Some((folder, in_season, is_root)) = folders.pop() {
        if cancelled.load(Ordering::Relaxed) { return Err(io::ErrorKind::Interrupted.into()); }
        let mut direct_episodes = 0;
        let mut children = Vec::new();
        for entry in list_folder(&folder, mount)? {
            match entry.kind {
                Kind::Video => {
                    counts.episodes += 1;
                    direct_episodes += 1;
                    episodic |= continuity::episode(&entry.name).is_some();
                }
                Kind::Folder => {
                    let season = art::is_season(&entry.name.to_string_lossy().to_lowercase());
                    recognized |= season;
                    if season && !in_season { counts.seasons += 1; }
                    children.push((folder.join(entry.name), in_season || season, false));
                }
            }
        }
        // In an established show, an unlabelled leaf container of episodes is
        // a season. Root-level videos never invent a season.
        if known_series && !is_root && !in_season && direct_episodes > 0 {
            counts.seasons += 1;
            for child in &mut children { child.1 = true; }
        }
        folders.extend(children);
    }
    if cancelled.load(Ordering::Relaxed) { return Err(io::ErrorKind::Interrupted.into()); }
    if !known_series && !episodic && !recognized { return Ok(None); }
    Ok(Some(counts))
}

#[cfg(test)]
mod usb_browser_tests {
    use super::*;

    #[test]
    fn counts_all_nested_videos_but_ignores_non_video_files() {
        let root = plx_base::testscratch::process_dir("cineusb-series-structure");
        for file in [
            "Mixed/Primer.mkv", "Mixed/Season 1/Segon.mp4", "Mixed/Season 1/Arxiu/Tercer.mkv",
            "Mixed/Temporada 2 - Títol/Quart.mkv", "Mixed/S03/Cinquè.mkv", "Mixed/S03/Cinquè.srt",
            "Mixed/Episodes from elsewhere/Another.mkv", "Mixed/Seasonal/Other.mp4",
            "Direct/One.mkv", "Direct/Two.mp4", "Episodes/Show.S01E01.mkv",
        ] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, []).unwrap();
        }
        assert_eq!(count(&root.join("Mixed"), None, false).unwrap(), Some(Counts { seasons: 3, episodes: 7 }));
        assert_eq!(count(&root.join("Direct"), None, true).unwrap(), Some(Counts { seasons: 0, episodes: 2 }));
        assert_eq!(count(&root.join("Direct"), None, false).unwrap(), None);
        assert_eq!(count(&root.join("Episodes"), None, false).unwrap(), Some(Counts { seasons: 0, episodes: 1 }));
        plx_base::testscratch::remove_tree(&root);
    }

    #[test]
    fn four_unconventional_season_folders_aggregate_into_the_show() {
        let root = plx_base::testscratch::process_dir("cineusb-unlabelled-seasons");
        for season in ["Primera entrega", "Segona entrega", "Tercera entrega", "Última entrega"] {
            for episode in ["Capítol 1.mkv", "Capítol 2.mp4"] {
                let path = root.join(season).join(episode);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, []).unwrap();
            }
        }
        assert_eq!(count(&root, None, true).unwrap(), Some(Counts { seasons: 4, episodes: 8 }));
        plx_base::testscratch::remove_tree(&root);
    }

    #[test]
    fn incomplete_or_cancelled_scans_never_publish_zero_episodes() {
        let root = plx_base::testscratch::process_dir("cineusb-cancelled-series");
        std::fs::create_dir_all(&root).unwrap();
        let cancelled = AtomicBool::new(true);
        assert_eq!(count_cancellable(&root, None, true, &cancelled).unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(count(&root.join("removed"), None, true).is_err());
        plx_base::testscratch::remove_tree(&root);
    }
}
