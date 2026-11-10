use super::disk::{Entry, Kind};
use plx_gfx::{gfx, svg};
use plx_ui::{Painter, Rect, theme, widgets};
use std::cell::Cell;
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Cover {
    Drive,
    Folder,
    Movie,
    Series,
}

thread_local! {
    // One texture per illustration, shared by every visible card of that kind.
    static TEXTURES: Cell<[Option<u32>; 4]> = const { Cell::new([None; 4]) };
}

impl Cover {
    pub(super) fn draw(self, p: Painter, card: Rect, radius: f32, lift: f32) {
        if p.is_recording() { return; }
        let texture = TEXTURES.with(|cache| {
            let mut textures = cache.get();
            let slot = &mut textures[self as usize];
            let texture = *slot.get_or_insert_with(|| {
                let source = match self {
                    Self::Drive => include_str!("../../../../assets/cineusb/drive.svg"),
                    Self::Folder => include_str!("../../../../assets/cineusb/folder.svg"),
                    Self::Movie => include_str!("../../../../assets/cineusb/movie.svg"),
                    Self::Series => include_str!("../../../../assets/cineusb/series.svg"),
                };
                const WIDTH: i32 = 300;
                const HEIGHT: i32 = 450;
                svg::rasterize(source, WIDTH, HEIGHT)
                    .map_or(0, |pixels| gfx::upload_rgba(0, WIDTH, HEIGHT, pixels.as_ptr()))
            });
            cache.set(textures);
            texture
        });
        if texture == 0 {
            widgets::absent_card(p, card, radius);
        } else {
            p.tex_carded(texture, [0.0, 0.0, 1.0, 1.0], card, radius, theme::TINT_WHITE, lift);
        }
    }
}

pub(super) fn collection(path: &Path) -> Option<Cover> {
    path.components().rev().find_map(|component| {
        let name = component.as_os_str().to_str()?.to_lowercase();
        let name = name.trim();
        match name {
            "películas" | "peliculas" | "pelis" | "cine" | "movies" | "films" => Some(Cover::Movie),
            "series" | "tv" | "tv shows" | "tv_shows" | "shows" | "episodios" => Some(Cover::Series),
            _ if is_season(name) => Some(Cover::Series),
            _ => None,
        }
    })
}

pub(super) fn entry_cover(entry: &Entry, parent: Option<Cover>) -> (Cover, &'static str) {
    let path = Path::new(&entry.name);
    match entry.kind {
        Kind::Folder => match collection(path).or(parent) {
            Some(Cover::Movie) => (Cover::Movie, "Películas"),
            Some(Cover::Series) => (Cover::Series, "Series"),
            _ => (Cover::Folder, "Carpeta"),
        },
        Kind::Video => {
            let episodic = path.file_stem().and_then(|stem| stem.to_str()).is_some_and(has_episode_number);
            if episodic || parent == Some(Cover::Series) {
                (Cover::Series, "Episodio")
            } else if parent == Some(Cover::Movie) {
                (Cover::Movie, "Película")
            } else {
                (Cover::Movie, "Vídeo")
            }
        }
    }
}

fn is_season(name: &str) -> bool {
    ["temporada", "season"].into_iter().any(|prefix| {
        name.strip_prefix(prefix).is_some_and(|suffix| small_number(suffix.trim_start_matches([' ', '.', '_', '-'])))
    }) || name.strip_prefix('s').is_some_and(small_number)
}

fn small_number(text: &str) -> bool {
    (1..=2).contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_digit())
}

fn has_episode_number(stem: &str) -> bool {
    stem.split(|c: char| !c.is_ascii_alphanumeric()).any(|token| {
        if let Some(number) = token.strip_prefix(['s', 'S']) {
            return number.split_once(['e', 'E']).is_some_and(|(season, episode)| {
                small_number(season) && small_number(episode)
            });
        }
        token.split_once(['x', 'X']).is_some_and(|(season, episode)| {
            small_number(season) && small_number(episode)
        })
    })
}
