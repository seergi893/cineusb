use super::{DetailKey, art, continuity, disk::Kind, list_folder, media_card::Formats};
use plx_play_feed::LocalVideoDetails;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Counts {
    pub seasons: usize,
    pub episodes: usize,
}

#[derive(Debug)]
pub(super) struct Listing {
    pub counts: Counts,
    pub episodes: Vec<DetailKey>,
}

pub(super) struct Summary {
    pub listing: Listing,
    pub watched: usize,
    next: usize,
    resolution: Agreement<&'static str>,
    video: Agreement<&'static str>,
    audio: [Agreement<&'static str>; 2],
}

struct Agreement<T> {
    common: Option<T>,
    unknown: bool,
    mixed: bool,
}

impl<T: Copy + Eq> Agreement<T> {
    fn new() -> Self { Self { common: None, unknown: false, mixed: false } }

    fn add(&mut self, value: Option<T>) {
        match value {
            Some(value) => {
                let first = self.common.get_or_insert(value);
                self.mixed |= *first != value;
            }
            None => self.unknown = true,
        }
    }

    fn common(&self, complete: bool) -> Option<T> {
        self.common.filter(|_| complete && !self.unknown && !self.mixed)
    }
}

impl Summary {
    pub(super) fn new(listing: Listing) -> Self {
        Self { listing, watched: 0, next: 0, resolution: Agreement::new(), video: Agreement::new(),
            audio: std::array::from_fn(|_| Agreement::new()) }
    }

    /// Each file contributes once. Returning to a tab consumes the shared details cache.
    pub(super) fn next_probe(&mut self, cache: &BTreeMap<DetailKey, LocalVideoDetails>) -> Option<&DetailKey> {
        while let Some(key) = self.listing.episodes.get(self.next) {
            let Some(details) = cache.get(key) else { return Some(key); };
            let normal = Formats::file(details, false);
            self.resolution.add(normal.resolution);
            self.video.add(normal.video);
            for (index, format) in [normal, Formats::file(details, true)].into_iter().enumerate() {
                self.audio[index].add(format.audio);
            }
            self.next += 1;
        }
        None
    }

    pub(super) fn formats(&self, spanish: bool) -> Formats {
        let complete = self.next == self.listing.episodes.len();
        let audio = &self.audio[usize::from(spanish)];
        Formats {
            resolution: if self.resolution.mixed { Some("4K UHD / 1080p") } else { self.resolution.common(complete) },
            video: self.video.common(complete),
            audio: audio.common(complete),
            mixed: self.video.mixed || audio.mixed,
        }
    }
}

#[cfg(test)]
pub(super) fn count(path: &Path, mount: Option<u64>, known_series: bool) -> io::Result<Option<Counts>> {
    count_cancellable(path, mount, known_series, &AtomicBool::new(false)).map(|result| result.map(|listing| listing.counts))
}

pub(super) fn count_cancellable(path: &Path, mount: Option<u64>, known_series: bool, cancelled: &AtomicBool) -> io::Result<Option<Listing>> {
    let mut folders = vec![(path.to_path_buf(), None, false, true)];
    let mut episodic = false;
    let mut seasons = BTreeSet::new();
    let mut unnumbered = 0;
    let mut episodes = Vec::new();
    while let Some((folder, season, covered, is_root)) = folders.pop() {
        if cancelled.load(Ordering::Relaxed) { return Err(io::ErrorKind::Interrupted.into()); }
        let mut unnumbered_episode = false;
        let mut children = Vec::new();
        for entry in list_folder(&folder, mount)? {
            let child = folder.join(&entry.name);
            match entry.kind {
                Kind::Video => {
                    let stem = child.file_stem().unwrap_or_default().to_string_lossy();
                    if ["sample", "trailer", "muestra", "tráiler"].iter().any(|name| stem.eq_ignore_ascii_case(name)) {
                        continue;
                    }
                    let tagged = continuity::episode(&entry.name).map(|(_, s, _)| s);
                    episodic |= tagged.is_some();
                    if let Some(number) = season.or(tagged) {
                        seasons.insert(number);
                    } else {
                        unnumbered_episode = true;
                    }
                    episodes.push((child, entry.size, entry.modified));
                }
                Kind::Folder => {
                    let name = entry.name.to_string_lossy().to_lowercase();
                    if matches!(name.as_str(), "sample" | "samples" | "extras" | "trailers" | "featurettes") {
                        continue;
                    }
                    let named = art::is_season(&name).then(|| super::home::first_number(&name)).flatten();
                    children.push((child, season.or(named), covered, false));
                }
            }
        }
        // A populated, unnumbered container can stand for a season in a known show.
        if known_series && !is_root && !covered && season.is_none() && unnumbered_episode {
            unnumbered += 1;
            for child in &mut children { child.2 = true; }
        }
        folders.extend(children);
    }
    if cancelled.load(Ordering::Relaxed) { return Err(io::ErrorKind::Interrupted.into()); }
    if !known_series && !episodic && seasons.is_empty() { return Ok(None); }
    episodes.sort_unstable();
    let counts = Counts { seasons: seasons.len() + unnumbered, episodes: episodes.len() };
    Ok(Some(Listing { counts, episodes }))
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
            "Mixed/S03/sample.mkv", "Mixed/Extras/Entrevista.mp4",
            "Mixed/Episodes from elsewhere/Another.mkv", "Mixed/Seasonal/Other.mp4",
            "Direct/One.mkv", "Direct/Two.mp4", "Episodes/Show.S01E01.mkv",
        ] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, []).unwrap();
        }
        std::fs::create_dir_all(root.join("Mixed/Season 4")).unwrap();
        assert_eq!(count(&root.join("Mixed"), None, false).unwrap(), Some(Counts { seasons: 3, episodes: 7 }));
        assert_eq!(count(&root.join("Direct"), None, true).unwrap(), Some(Counts { seasons: 0, episodes: 2 }));
        assert_eq!(count(&root.join("Direct"), None, false).unwrap(), None);
        assert_eq!(count(&root.join("Episodes"), None, false).unwrap(), Some(Counts { seasons: 1, episodes: 1 }));
        plx_base::testscratch::remove_tree(&root);
    }

    #[test]
    fn formats_only_claim_agreement_across_inspected_episodes() {
        use plx_play_feed::{LocalAudioFormat, VideoRange};
        let a = ("Show S01E01.mkv".into(), None, None);
        let b = ("Show 1x02.mkv".into(), None, None);
        let c = ("Show S01E03.mkv".into(), None, None);
        let mut summary = Summary::new(Listing { counts: Counts { seasons: 1, episodes: 3 },
            episodes: vec![a.clone(), b.clone(), c.clone()] });
        let stereo = |label| Some(LocalAudioFormat { label, channels: Some(2) });
        let mut cache = BTreeMap::from([(a, LocalVideoDetails {
            resolution: Some((3840, 1608)), range: Some(VideoRange::DolbyVision),
            audio: [stereo("Dolby Atmos"), stereo("Dolby Digital")], ..Default::default()
        })]);
        assert_eq!(summary.next_probe(&cache), Some(&b));
        assert!(summary.formats(false).video.is_none() && summary.formats(false).audio.is_none());
        cache.insert(b, LocalVideoDetails { resolution: Some((2048, 858)), range: Some(VideoRange::Hdr10),
            audio: [stereo("AAC"), stereo("Dolby Digital")], ..Default::default() });
        assert_eq!(summary.next_probe(&cache), Some(&c));
        assert_eq!(summary.formats(false).resolution, Some("4K UHD / 1080p"));
        cache.insert(c, LocalVideoDetails { audio: [None, stereo("Dolby Digital")], ..Default::default() });
        assert_eq!(summary.next_probe(&cache), None);
        let automatic = summary.formats(false);
        assert!(automatic.mixed && automatic.video.is_none() && automatic.audio.is_none());
        let spanish = summary.formats(true);
        assert_eq!(spanish.audio, Some("Dolby Digital"));
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
        let archive = root.join("Primera entrega/Archive");
        std::fs::create_dir_all(&archive).unwrap();
        std::fs::write(archive.join("Capítol 3.mkv"), []).unwrap();
        assert_eq!(count(&root, None, true).unwrap(), Some(Counts { seasons: 4, episodes: 9 }));
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
