//! Episode identity and ordering for the offline USB library.
use std::ffi::OsStr;
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct Episode {
    pub season: u32,
    pub first: u32,
    pub last: u32,
}

struct Parsed {
    season: Option<u32>,
    first: u32,
    last: u32,
    start: usize,
    end: usize,
}

#[derive(Clone, Copy)]
struct Word<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}

fn words(text: &str) -> Vec<Word<'_>> {
    let mut result = Vec::new();
    let mut start = None;
    let mut previous = None;
    for (at, ch) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        let kind = ch.is_alphanumeric().then_some(ch.is_ascii_digit());
        if kind != previous {
            if let Some(begin) = start.take() {
                result.push(Word {
                    text: &text[begin..at],
                    start: begin,
                    end: at,
                });
            }
            start = kind.map(|_| at);
            previous = kind;
        }
    }
    result
}

fn number(word: &Word<'_>) -> Option<u32> {
    (word.text.len() <= 3)
        .then(|| word.text.parse().ok())
        .flatten()
}

fn season_word(text: &str) -> bool {
    ["s", "t", "season", "temporada"]
        .iter()
        .any(|word| text.eq_ignore_ascii_case(word))
}

fn episode_word(text: &str) -> bool {
    [
        "e",
        "ep",
        "episode",
        "episodio",
        "episodi",
        "cap",
        "capítulo",
        "capítol",
    ]
    .contains(&text.to_lowercase().as_str())
}

fn special(text: &str) -> bool {
    matches!(
        text.to_lowercase().as_str(),
        "special" | "specials" | "especial" | "especiales" | "especials"
    )
}

fn separator(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '.' | '_' | '-' | '–' | '—' | '(' | ')' | '[' | ']')
}

pub(super) fn season_number(name: &str) -> Option<u32> {
    if special(name.trim()) {
        return Some(0);
    }
    let tokens = words(name);
    let [mark, value, ..] = tokens.as_slice() else {
        return None;
    };
    if mark.start != 0 || !season_word(mark.text) {
        return None;
    }
    if tokens
        .get(2)
        .is_some_and(|next| next.start == value.end || episode_word(next.text))
    {
        return None;
    }
    number(value)
}

fn parse(stem: &str, context: bool) -> Result<Option<Parsed>, ()> {
    let tokens = words(stem);
    let mut result: Option<Parsed> = None;
    let mut i = 0;
    while let Some(word) = tokens.get(i) {
        let at_boundary = stem[..word.start].chars().next_back().is_none_or(separator);
        let mut season = None;
        let mut episode_index = i;
        let mut cross = false;
        if at_boundary {
            if let [marker, value, ep, ..] = &tokens[i..] {
                if season_word(marker.text) && number(value).is_some() && episode_word(ep.text) {
                    season = number(value);
                    episode_index = i + 3;
                } else if value.text.eq_ignore_ascii_case("x") && number(marker).is_some() {
                    season = number(marker);
                    episode_index = i + 2;
                    cross = true;
                }
            }
            if episode_index == i && special(word.text) {
                season = Some(0);
                episode_index = i + 1;
            }
            if episode_index == i
                && context
                && result.is_none()
                && i == 0
                && episode_word(word.text)
            {
                episode_index = i + 1;
            }
            let leading = context && i == 0 && episode_index == i && number(word).is_some();
            if (episode_index != i || leading) && (season.is_some() || context) {
                if let Some(first) = tokens
                    .get(episode_index)
                    .and_then(number)
                    .filter(|&e| e > 0)
                {
                    let mut end_index = episode_index;
                    let mut last = first;
                    while let Some(next) = tokens.get(end_index + 1) {
                        let gap = &stem[tokens[end_index].end..next.start];
                        let tagged = next.text.eq_ignore_ascii_case("e")
                            && (gap.is_empty() || gap.trim() == "-");
                        let ranged = cross && gap == "-" && number(next).is_some();
                        if !tagged && !ranged {
                            break;
                        }
                        let n = end_index + if tagged { 2 } else { 1 };
                        let Some(value) = tokens.get(n).and_then(number) else {
                            break;
                        };
                        if value <= last {
                            return Err(());
                        }
                        last = value;
                        end_index = n;
                    }
                    let end = tokens[end_index].end;
                    if stem[end..].chars().next().is_none_or(separator) {
                        let candidate = Parsed {
                            season,
                            first,
                            last,
                            start: word.start,
                            end,
                        };
                        if let Some(previous) = &result {
                            if previous.season != season
                                || previous.first != first
                                || previous.last != last
                            {
                                if previous.season.is_none() && season.is_some() {
                                    result = Some(candidate);
                                } else {
                                    return Err(());
                                }
                            }
                        } else {
                            result = Some(candidate);
                        }
                        i = end_index + 1;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }
    Ok(result)
}

pub(super) fn episode_parts(stem: &OsStr) -> Option<(&[u8], u32, u32, &[u8])> {
    let text = stem.to_str()?;
    let parsed = parse(text, false).ok()??;
    Some((
        text[..parsed.start].trim_end_matches(separator).as_bytes(),
        parsed.season?,
        parsed.first,
        text[parsed.end..].as_bytes(),
    ))
}

pub(super) fn episode(name: &OsStr) -> Option<(&[u8], u32, u32)> {
    episode_parts(Path::new(name).file_stem()?)
        .map(|(prefix, season, first, _)| (prefix, season, first))
}

/// Resolve from a USB-relative path. Numeric stems require a known show or season folder.
pub(super) fn identify(path: &Path, known_show: bool) -> Option<Episode> {
    let parent = path.parent()?;
    let mut folders = parent
        .components()
        .rev()
        .filter_map(|part| season_number(part.as_os_str().to_str()?));
    let folder = folders.next();
    let implicit = known_show && parent.as_os_str().is_empty();
    let parsed = parse(path.file_stem()?.to_str()?, implicit || folder.is_some()).ok()??;
    let season = if let Some(explicit) = parsed.season {
        explicit
    } else {
        if folders.any(|value| Some(value) != folder) {
            return None;
        }
        folder.or(implicit.then_some(1))?
    };
    Some(Episode {
        season,
        first: parsed.first,
        last: parsed.last,
    })
}

#[cfg(test)]
mod usb_browser_tests {
    use super::*;

    #[test]
    fn episode_notations_and_ranges() {
        for (stem, season, first, last) in [
            ("S01E01", 1, 1, 1),
            ("S1E1", 1, 1, 1),
            ("S01-E01", 1, 1, 1),
            ("S01.E01", 1, 1, 1),
            ("S01_E01", 1, 1, 1),
            ("T1E1", 1, 1, 1),
            ("T1-E1", 1, 1, 1),
            ("T01_E02", 1, 2, 2),
            ("1x01", 1, 1, 1),
            ("01x02", 1, 2, 2),
            ("1X3", 1, 3, 3),
            ("Season 1 Episode 2", 1, 2, 2),
            ("Season 01 Ep 02", 1, 2, 2),
            ("Temporada 1 Episodio 2", 1, 2, 2),
            ("Temporada 1 Capítulo 2", 1, 2, 2),
            ("Temporada 1 Capítol 2", 1, 2, 2),
            ("T1 Cap 02", 1, 2, 2),
            ("Episode 03", 1, 3, 3),
            ("E03", 1, 3, 3),
            ("Capítulo 03", 1, 3, 3),
            ("Capítol 03", 1, 3, 3),
            ("03 - Episode title", 1, 3, 3),
            ("03.Episode.Title", 1, 3, 3),
            ("03 Episode title", 1, 3, 3),
            ("S01E01-E02", 1, 1, 2),
            ("S01E01E02", 1, 1, 2),
            ("1x01-02", 1, 1, 2),
            ("S00E03", 0, 3, 3),
            ("Special 3", 0, 3, 3),
        ] {
            assert_eq!(
                identify(Path::new(&format!("{stem}.mkv")), true),
                Some(Episode {
                    season,
                    first,
                    last
                }),
                "{stem}"
            );
        }
    }

    #[test]
    fn context_does_not_override_explicit_numbers_or_accept_conflicts() {
        assert_eq!(
            identify(Path::new("Show/Temporada 2/Archive/02.mkv"), true),
            Some(Episode {
                season: 2,
                first: 2,
                last: 2
            })
        );
        assert_eq!(
            identify(Path::new("Show/Temporada 2/S03E04.mkv"), true),
            Some(Episode {
                season: 3,
                first: 4,
                last: 4
            })
        );
        for stem in [
            "Box1x01",
            "2026 Pilot",
            "1080p",
            "Pilot 720p",
            "S01E01 S02E02",
            "S01E02-E01",
        ] {
            assert_eq!(
                identify(Path::new(&format!("{stem}.mkv")), true),
                None,
                "{stem}"
            );
        }
        assert!(identify(Path::new("Movies/03 Film.mkv"), false).is_none());
    }
}
