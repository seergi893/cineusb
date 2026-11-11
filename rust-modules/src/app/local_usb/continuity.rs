//! Local episode ordering and playhead policy.
use super::disk::{Entry, Kind};
use std::ffi::OsStr;
use std::path::Path;

fn episode(name: &OsStr) -> Option<(&[u8], u32, u32)> {
    let bytes = Path::new(name).file_stem()?.as_encoded_bytes();
    for (start, &byte) in bytes.iter().enumerate() {
        if !matches!(byte, b'S' | b's') || (start > 0 && bytes[start - 1].is_ascii_alphanumeric()) { continue; }
        let season = &bytes[start + 1..];
        let n = season.iter().take_while(|byte| byte.is_ascii_digit()).count();
        let Some(b'E' | b'e') = season.get(n) else { continue };
        let tail = &season[n + 1..];
        let m = tail.iter().take_while(|byte| byte.is_ascii_digit()).count();
        if n == 0 || m == 0 || tail.get(m).is_some_and(u8::is_ascii_alphanumeric) { continue; }
        let number = |digits: &[u8]| std::str::from_utf8(digits).ok()?.parse().ok();
        let prefix = bytes[..start].trim_ascii_end();
        return Some((prefix, number(&season[..n])?, number(&tail[..m])?));
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

pub(super) fn resumable(position: u64, duration: Option<u64>) -> bool {
    position >= 30 && duration.is_none_or(|duration|
        duration.saturating_sub(position) > (duration / 20).clamp(15, 120))
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
    fn playhead_skips_the_opening_and_credits() {
        assert!(!resumable(29, Some(1000)));
        assert!(resumable(30, Some(1000)));
        assert!(!resumable(951, Some(1000)));
    }
}
