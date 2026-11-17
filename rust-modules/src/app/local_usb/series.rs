use super::{art, continuity, disk::Kind, list_folder};
use std::io;
use std::path::Path;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Counts {
    pub seasons: usize,
    pub episodes: usize,
}

pub(super) fn count(path: &Path, mount: Option<u64>, known_series: bool) -> io::Result<Option<Counts>> {
    let mut counts = Counts::default();
    let mut seasons = Vec::new();
    let mut episodic = false;
    for entry in list_folder(path, mount)? {
        match entry.kind {
            Kind::Video => {
                counts.episodes += 1;
                episodic |= continuity::episode(&entry.name).is_some();
            }
            Kind::Folder if art::is_season(&entry.name.to_string_lossy().to_lowercase()) => {
                seasons.push(path.join(entry.name));
            }
            Kind::Folder => {}
        }
    }
    if !known_series && !episodic && seasons.is_empty() { return Ok(None); }
    counts.seasons = seasons.len();
    // The USB listing excludes symlinks. Each real folder is visited once;
    // unrelated folders at the series root are outside the episode tree.
    while let Some(folder) = seasons.pop() {
        for entry in list_folder(&folder, mount)? {
            match entry.kind {
                Kind::Video => counts.episodes += 1,
                Kind::Folder => seasons.push(folder.join(entry.name)),
            }
        }
    }
    Ok(Some(counts))
}

#[cfg(test)]
mod usb_browser_tests {
    use super::*;

    #[test]
    fn counts_direct_and_nested_season_videos_without_counting_unrelated_folders() {
        let root = plx_base::testscratch::process_dir("cineusb-series-structure");
        for file in [
            "Mixed/Primer.mkv", "Mixed/Season 1/Segon.mp4", "Mixed/Season 1/Arxiu/Tercer.mkv",
            "Mixed/Temporada 2/Quart.mkv", "Mixed/S03/Cinquè.mkv", "Mixed/S03/Cinquè.srt",
            "Mixed/Episodes from elsewhere/Another.mkv", "Mixed/Seasonal/Other.mp4",
            "Direct/One.mkv", "Direct/Two.mp4", "Episodes/Show.S01E01.mkv",
        ] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, []).unwrap();
        }
        assert_eq!(count(&root.join("Mixed"), None, false).unwrap(), Some(Counts { seasons: 3, episodes: 5 }));
        assert_eq!(count(&root.join("Direct"), None, true).unwrap(), Some(Counts { seasons: 0, episodes: 2 }));
        assert_eq!(count(&root.join("Direct"), None, false).unwrap(), None);
        assert_eq!(count(&root.join("Episodes"), None, false).unwrap(), Some(Counts { seasons: 0, episodes: 1 }));
        plx_base::testscratch::remove_tree(&root);
    }
}
