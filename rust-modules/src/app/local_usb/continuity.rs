//! Local episode ordering and playhead policy.
use super::disk::{Entry, Kind};
use std::ffi::OsStr;
use std::path::Path;

pub(super) fn episode(name: &OsStr) -> Option<(&[u8], u32, u32)> {
    let bytes = Path::new(name).file_stem()?.as_encoded_bytes();
    let number = |digits: &[u8]| -> Option<u32> { std::str::from_utf8(digits).ok()?.parse().ok() };
    for start in 0..bytes.len() {
        if start > 0 && bytes[start - 1].is_ascii_alphanumeric() { continue; }
        let token = &bytes[start..];
        let prefixed = matches!(token[0], b'S' | b's' | b'T' | b't');
        let season = &token[usize::from(prefixed)..];
        let n = season.iter().take_while(|byte| byte.is_ascii_digit()).count();
        let Some(separator) = season.get(n).map(u8::to_ascii_lowercase) else { continue };
        if n == 0 || separator != if prefixed { b'e' } else { b'x' } { continue; }
        let tail = &season[n + 1..];
        let m = tail.iter().take_while(|byte| byte.is_ascii_digit()).count();
        if m == 0 || tail.get(m).is_some_and(u8::is_ascii_alphanumeric) { continue; }
        let (Some(season), Some(episode)) = (number(&season[..n]), number(&tail[..m])) else { continue };
        return Some((bytes[..start].trim_ascii_end(), season, episode));
    }
    None
}

pub(super) fn next_episode<'a>(current: &OsStr, entries: &'a [Entry]) -> Option<&'a OsStr> {
    let (prefix, season, episode_number) = episode(current)?;
    let next = episode_number.checked_add(1)?;
    let mut candidates = entries.iter().filter(|entry| entry.kind == Kind::Video).filter(|entry| {
        episode(&entry.name).is_some_and(|(title, s, e)| s == season && e == next && title.eq_ignore_ascii_case(prefix))
    });
    let entry = candidates.next()?;
    candidates.next().is_none().then_some(entry.name.as_os_str())
}

#[cfg(test)]
mod usb_browser_tests {
    use super::*;
    use crate::app::local_usb::disk;

    #[test]
    fn sequential_episodes_must_belong_to_the_same_series_and_season() {
        let entries: Vec<_> = ["Otra S01E02.mkv", "Sèrie S02E02.mkv", "Sèrie S01E10.mkv", "Sèrie S01E02.mkv"]
            .into_iter().filter_map(|name| disk::media_entry(name.into(), false)).collect();
        assert_eq!(next_episode(OsStr::new("Sèrie S01E01.mkv"), &entries), Some(OsStr::new("Sèrie S01E02.mkv")));
        assert_eq!(next_episode(OsStr::new("Película.mkv"), &entries), None);
        assert_eq!(next_episode(OsStr::new("Sèrie S01E02.mkv"), &entries), None);
    }

    #[test]
    fn labels_parse_all_supported_episode_notations() {
        for name in ["Serie S01E104.mkv", "Serie 1x104.mkv", "Serie T1E104.mkv"] {
            assert_eq!(episode(OsStr::new(name)).map(|(_, season, ep)| (season, ep)), Some((1, 104)));
        }
        assert!(episode(OsStr::new("Box1x01.mkv")).is_none());
    }
}
