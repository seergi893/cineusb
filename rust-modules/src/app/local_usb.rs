//! USB browsing and its off-frame filesystem jobs.
mod art;
mod continuity;
mod disk;
mod demo;
mod series;
mod home;
mod home_screen;
mod media_card;
mod details_cache;

use art::Cover;
use super::{App, Key, playback};
use disk::{Entry, Kind, Scan, Volume};
use media_card::{Category, Readout};
use plx_gfx::{gfx, text::TtfMeasure};
use plx_machine::machine::Measure;
use plx_ui::{
    icons::{self, Icon},
    label::{HAlign, Label},
    poster_grid::{self, Card, Geom, GridPop, USB_STYLE},
    screen::Dir,
    theme, widgets, Env, Painter, Rect, Spring, View,
};
use std::fs::File;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use plx_platform::i18n::{self, Language, Preference};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

const MOTION: Duration = Duration::from_millis(160);
const FAVORITE_MOTION: Duration = Duration::from_millis(340);
const SETTINGS_MOTION: Duration = Duration::from_millis(220);
const FAVORITE_YELLOW: [f32; 4] = [0.98, 0.75, 0.24, 1.0];

fn favorite_ease(progress: f32) -> f32 {
    let t = progress.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn favorite_lerp(from: [f32; 4], to: [f32; 4], alpha: f32) -> [f32; 4] {
    std::array::from_fn(|index| from[index] + (to[index] - from[index]) * alpha)
}

const WATCHED: u64 = u64::MAX;
const FAVORITE: u64 = WATCHED - 1;
const PACKED: u64 = 1 << 63;
const PLAYHEAD_MASK: u64 = (1 << 43) - 1;
const DURATION_MASK: u64 = (1 << 20) - 1;

fn motion_alpha(started: Instant) -> f32 {
    let t = (started.elapsed().as_secs_f32() / MOTION.as_secs_f32()).min(1.0);
    t * t * (3.0 - 2.0 * t)
}

fn settings_ease(started: Instant) -> f32 {
    let t = (started.elapsed().as_secs_f32() / SETTINGS_MOTION.as_secs_f32()).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Compact clock notation for the fixed-width metadata badge. Never elide
/// the minutes of an hour-long movie (e.g. 1h55, not 1 h 55...).
fn card_duration(seconds: u64, available_width: f32) -> String {
    let minutes = seconds / 60;
    if minutes < 1 { return "<1m".into(); }
    if minutes < 60 {
        return if available_width < 95.0 { format!("{minutes}m") }
            else { format!("{minutes} min") };
    }
    let hours = minutes / 60;
    let remainder = minutes % 60;
    if available_width < 125.0 {
        format!("{hours}h{remainder:02}")
    } else {
        format!("{hours} h {remainder:02} min")
    }
}

fn nav_glyph_ink(index: usize, focus: f32, selected: f32) -> [f32; 4] {
    if index == FAVORITES_TAB { theme::USB_FAVORITES }
    else { widgets::TabPill::mixed_ink(focus, selected) }
}

fn pack_progress(position: u64, duration: Option<u64>) -> u64 {
    let position_us = (position / 1000).min(PLAYHEAD_MASK - 1);
    let duration_secs = (duration.unwrap_or(0) / 1_000_000_000).min(DURATION_MASK - 1);
    PACKED | (duration_secs << 43) | position_us
}

fn unpack_progress(value: u64) -> (u64, u64) {
    if matches!(value, WATCHED | FAVORITE) { return (0, 0); }
    if value & PACKED == 0 { return (value.saturating_mul(1_000_000_000), 0); }
    ((value & PLAYHEAD_MASK) * 1000, ((value >> 43) & DURATION_MASK) * 1_000_000_000)
}

const TOP: f32 = 284.0;
const MEDIA_GRID_TOP: f32 = 204.0;
const HOME_INTRO_BOTTOM: f32 = 244.0;
const HOME_HEADER_TOP: f32 = HOME_INTRO_BOTTOM + 40.0;
const HOME_HEADING_PITCH: f32 = 58.0;
const HOME_GRID_TOP: f32 = HOME_HEADER_TOP + HOME_HEADING_PITCH;
const HOME_CARD_W: f32 = 428.0;
const HOME_CARD_H: f32 = 240.0;
const HOME_RECENT_TOP: f32 = HOME_GRID_TOP + HOME_CARD_H + 29.0 + HOME_HEADING_PITCH;
const HOME_CARD_PITCH: f32 = 446.0;
const HOME_DISK_X: f32 = 1450.0;
const HOME_DISK_BOTTOM: f32 = HOME_RECENT_TOP + HOME_CARD_H;
const GRID_BOTTOM: f32 = 965.0;
const GRID_WIDTH: f32 = 1920.0 - 2.0 * plx_ui::consts::MARGIN_X;

fn grid_geometry(len: usize, size: usize, media_library: bool) -> Geom {
    let (limit, gap, max_width) = [(8, 22.0, 214.0), (10, 18.0, 174.0), (6, 26.0, 270.0)][size];
    let rows = len.max(1).div_ceil(limit);
    let cols = if media_library { limit } else { len.max(1).div_ceil(rows) };
    let width = ((GRID_WIDTH - (cols - 1) as f32 * gap) / cols as f32).min(max_width);
    Geom { cols, left: plx_ui::consts::MARGIN_X, w: width, h: width * 1.5, gap, value_line: false }
}

fn tr(es: &'static str, ca: &'static str) -> &'static str {
    if i18n::current().language() == Language::Ca { ca } else { es }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action { ToggleFavorite, Select, DeleteFolders }

enum Mutation { Delete, Eject }

const PANEL: Rect = Rect::new(510.0, 292.0, 900.0, 496.0);
const OPTIONS: [Rect; 2] = [
    Rect::new(555.0, 676.0, 390.0, 66.0),
    Rect::new(975.0, 676.0, 390.0, 66.0),
];
const VIDEO_DIALOG_INSET: f32 = 28.0;
const VIDEO_ACTION_Y: f32 = 650.0;
const VIDEO_ACTION_H: f32 = 60.0;
const EPISODE_VIDEO_OPTIONS: [Rect; 3] = [
    Rect::new(512.0, VIDEO_ACTION_Y, 250.0, VIDEO_ACTION_H),
    Rect::new(774.0, VIDEO_ACTION_Y, 276.0, VIDEO_ACTION_H),
    Rect::new(1062.0, VIDEO_ACTION_Y, 346.0, VIDEO_ACTION_H),
];
const VIDEO_OPTIONS: [Rect; 4] = [
    Rect::new(425.0, VIDEO_ACTION_Y, 220.0, VIDEO_ACTION_H),
    Rect::new(657.0, VIDEO_ACTION_Y, 266.0, VIDEO_ACTION_H),
    Rect::new(935.0, VIDEO_ACTION_Y, 346.0, VIDEO_ACTION_H),
    Rect::new(1293.0, VIDEO_ACTION_Y, 202.0, VIDEO_ACTION_H),
];
const SETTINGS_OPTIONS: [Rect; 6] = [
    Rect::new(550.0, 401.0, 820.0, 49.0),
    Rect::new(550.0, 458.0, 820.0, 49.0),
    Rect::new(550.0, 515.0, 820.0, 49.0),
    Rect::new(550.0, 572.0, 820.0, 49.0),
    Rect::new(550.0, 629.0, 820.0, 49.0),
    Rect::new(550.0, 686.0, 820.0, 49.0),
];

// Rooted-webOS drives can use labels in different languages and Unicode forms.
// Only fold for matching; the real path is never changed.
fn folder_fold(name: &str) -> String {
    name.chars().filter_map(|ch| match ch.to_lowercase().next()? {
        'à' | 'á' | 'â' | 'ä' | 'ã' => Some('a'),
        'è' | 'é' | 'ê' | 'ë' => Some('e'),
        'ì' | 'í' | 'î' | 'ï' => Some('i'),
        'ò' | 'ó' | 'ô' | 'ö' | 'õ' => Some('o'),
        'ù' | 'ú' | 'û' | 'ü' => Some('u'),
        'ç' => Some('c'),
        'ñ' => Some('n'),
        ch if ch.is_ascii_alphanumeric() => Some(ch),
        _ => None,
    }).collect()
}

fn media_folder_score(name: &str, kind: usize) -> u8 {
    if kind == DOCUMENTALS_TAB {
        return if name.eq_ignore_ascii_case("documentals") || name.eq_ignore_ascii_case("documentales") {
            3
        } else {
            0
        };
    }
    let name = folder_fold(name);
    let exact: &[&str] = if kind == 1 {
        &["pelis", "peliculas", "movies", "films", "film", "cine", "cinema", "moviecollection", "pelicules", "pellicules"]
    } else {
        &["series", "seriestv", "tvseries", "tvshows", "shows", "temporadas", "episodios", "temporades", "episodis"]
    };
    if exact.contains(&name.as_str()) { 3 }
    else if kind == 1 && (name.starts_with("peliculas") || name.starts_with("movies") || name.starts_with("films")) { 2 }
    else if kind == 2 && (name.starts_with("series") || name.starts_with("tvshows")) { 2 }
    else { 0 }
}

fn is_video_path(path: &std::path::Path) -> bool {
    path.file_name().and_then(|name| disk::media_entry(name.to_os_string(), false))
        .is_some_and(|entry| entry.kind == Kind::Video)
}

fn season_folder(name: &str) -> bool {
    let name = folder_fold(name);
    name.starts_with("season") || name.starts_with("temporada") || name.starts_with("temporades")
        || (name.len() > 1 && name.starts_with('s') && name[1..].bytes().all(|b| b.is_ascii_digit()))
}

fn discover_media_folders(volume: &Volume, kind: usize) -> Vec<PathBuf> {
    let mut matched = Vec::new();
    let mut level = vec![volume.path.clone()];
    // Bounded root-first traversal, never on the drawing thread.
    for depth in 0..3 {
        let mut next = Vec::new();
        for parent in level.into_iter().take(96) {
            let Ok(entries) = list_folder(&parent, system_mount(volume)) else { continue };
            for entry in entries.into_iter().filter(|entry| entry.kind == Kind::Folder).take(250) {
                let path = parent.join(&entry.name);
                let name = entry.name.to_string_lossy();
                if media_folder_score(&name, kind) > 0 {
                    matched.push((depth, media_folder_score(&name, kind), path.clone()));
                }
                // Restrict nested scanning to likely container directories.
                if depth == 0 || (depth == 1 && ["media", "videos", "video", "multimedia", "biblioteca", "library"]
                        .contains(&folder_fold(&parent.file_name().unwrap_or_default().to_string_lossy()).as_str())) {
                    if next.len() < 96 { next.push(path); }
                }
            }
        }
        level = next;
    }
    matched.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)).then_with(|| a.2.cmp(&b.2)));
    matched.into_iter().map(|(_, _, path)| path).collect()
}

const MOVIES_TAB: usize = 1;
const SERIES_TAB: usize = 2;
const DOCUMENTALS_TAB: usize = 3;
const QUALITY_PILL: Rect = Rect::new(1430.0, 128.0, 394.0, 54.0);
const QUALITY_TABS: [Rect; 3] = [
    Rect::new(1436.0, 133.0, 120.0, 44.0),
    Rect::new(1561.0, 133.0, 128.0, 44.0),
    Rect::new(1694.0, 133.0, 124.0, 44.0),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MovieQuality { Uhd, FullHd }

impl MovieQuality {
    fn of(width: u32, height: u32) -> Option<Self> {
        // Midpoint between 1920 and 3840. Use the long edge for rotated sources
        // and ignore the short edge for cropped cinema once both are known.
        if width == 0 || height == 0 { return None; }
        Some(if width.max(height) >= 2880 { Self::Uhd } else { Self::FullHd })
    }

    fn label(self) -> &'static str {
        match self { Self::Uhd => "4K UHD", Self::FullHd => "1080p" }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum MovieFilter { #[default] All, Uhd, FullHd }

impl MovieFilter {
    const TABS: [Self; 3] = [Self::All, Self::Uhd, Self::FullHd];

    fn label(self) -> &'static str {
        match self {
            Self::All => tr("Todas", "Totes"),
            Self::Uhd => MovieQuality::Uhd.label(),
            Self::FullHd => MovieQuality::FullHd.label(),
        }
    }

    fn matches(self, quality: Option<MovieQuality>) -> bool {
        match self {
            Self::All => true,
            Self::Uhd => quality == Some(MovieQuality::Uhd),
            Self::FullHd => quality == Some(MovieQuality::FullHd),
        }
    }
}
const FAVORITES_TAB: usize = 4;
const SETTINGS_TAB: usize = 5;
const EJECT_TAB: usize = 6;

// Stable PlxNative-style navigation track.
const NAV_PILL: Rect = Rect::new(247.0, 32.0, 1386.0, 70.0);
const NAV_TABS: [Rect; EJECT_TAB + 1] = [
    Rect::new(260.0, 42.0, 125.0, 48.0),
    Rect::new(395.0, 42.0, 205.0, 48.0),
    Rect::new(610.0, 42.0, 155.0, 48.0),
    Rect::new(775.0, 42.0, 200.0, 48.0),
    Rect::new(985.0, 42.0, 195.0, 48.0),
    Rect::new(1190.0, 42.0, 170.0, 48.0),
    Rect::new(1370.0, 42.0, 250.0, 48.0),
];

fn media_tab_title(tab: usize) -> &'static str {
    match tab {
        1 => tr("Películas", "Pel·lícules"),
        2 => tr("Series", "Sèries"),
        DOCUMENTALS_TAB => "Documentals",
        _ => unreachable!("expected a media tab"),
    }
}
const CLOCK_RECT: Rect = Rect::new(1654.0, 42.0, 170.0, 48.0);

fn clock_font_size(size: usize) -> i32 {
    [theme::size::MICRO, theme::size::LABEL, theme::size::HEADLINE][size]
}

fn clock_label(time: Option<(i32, i32)>, settings: &Settings, automatic: i18n::Clock) -> Option<String> {
    if !settings.show_clock { return None; }
    let (hour, minute) = time?;
    let twelve = settings.clock_format == 1 || (settings.clock_format == 0 && automatic == i18n::Clock::H12);
    Some(if twelve {
        format!("{}:{minute:02} {}", if hour % 12 == 0 { 12 } else { hour % 12 }, if hour < 12 { "AM" } else { "PM" })
    } else {
        format!("{hour:02}:{minute:02}")
    })
}

// Folder-specific controls sit in the content header, not the navigation.
const ACTIONS: [(Action, Rect); 3] = [
    (Action::ToggleFavorite, Rect::new(684.0, 218.0, 396.0, 55.0)),
    (Action::Select, Rect::new(1094.0, 218.0, 376.0, 55.0)),
    (Action::DeleteFolders, Rect::new(1484.0, 218.0, 340.0, 55.0)),
];

const DEMO_SETTING: usize = 13;
const UNIFIED_SETTINGS_COUNT: usize = 14;
const SETTINGS_ORDER: [usize; UNIFIED_SETTINGS_COUNT] = [0, 1, 8, 2, 3, 4, 12, 5, 6, 7, 9, 10, 11, DEMO_SETTING];
const SETTINGS_GROUPS: [(&[usize], f32); 4] = [
    (&[0, 1, 8], 100.0), (&[2, 3, 4, 12], 305.0), (&[5], 560.0), (&[6, 7, 9, 10, 11, DEMO_SETTING], 667.0),
];
const SETTING_ROW_PITCH: f32 = 54.0;
fn unified_setting_rect(index: usize) -> Rect {
    let (indices, top) = SETTINGS_GROUPS.iter().copied()
        .find(|&(indices, _)| indices.contains(&index)).expect("valid setting");
    let row = indices.iter().position(|&item| item == index).expect("setting belongs to group");
    Rect::new(700.0, top + 38.0 + row as f32 * SETTING_ROW_PITCH, 1108.0, 50.0)
}

fn unified_setting_text(index: usize, settings: &Settings, demo: bool) -> (&'static str, &'static str, String) {
    let on = |enabled: bool| if enabled { tr("Activado", "Activat") } else { tr("Desactivado", "Desactivat") };
    match index {
        0 => (tr("Ordenar biblioteca", "Ordena la biblioteca"),
            tr("Cambia el orden de archivos y carpetas.", "Canvia l'ordre dels fitxers i les carpetes."),
            [tr("Nombre", "Nom"), tr("Fecha", "Data"), tr("Tamaño", "Mida")][settings.sort].into()),
        1 => (tr("Mostrar extensiones", "Mostra les extensions"),
            tr("Muestra extensiones en los diálogos de archivo.", "Mostra les extensions als diàlegs de fitxer."),
            on(settings.extensions).into()),
        2 => (tr("Siguiente episodio automático", "Episodi següent automàtic"),
            tr("Reproduce el siguiente episodio de la carpeta.", "Reprodueix l'episodi següent de la carpeta."),
            on(settings.auto_next).into()),
        3 => (tr("Subtítulos forzados", "Subtítols forçats"),
            tr("Activa las pistas forzadas disponibles.", "Activa els subtítols forçats disponibles."),
            on(settings.forced_subtitles).into()),
        4 => (tr("Idioma de audio", "Idioma de l'àudio"),
            tr("Elige el idioma al comenzar cada vídeo.", "Tria l'idioma quan comences un vídeo."),
            if settings.spanish_audio { tr("Español", "Castellà") } else { tr("Automático", "Automàtic") }.into()),
        5 => (tr("Idioma de CineUSB", "Idioma de CineUSB"),
            tr("Cambia el idioma de la interfaz.", "Canvia l'idioma de la interfície."),
            match settings.language { Preference::Ca => "Català", Preference::En => "English", _ => "Español" }.into()),
        6 => (tr("Color de acento", "Color de ressaltat"),
            tr("Personaliza el color de selección.", "Personalitza el color de selecció."),
            [tr("Morado", "Lila"), tr("Azul", "Blau"), tr("Gris neutro", "Gris neutre")][settings.accent].into()),
        7 => (tr("Tamaño de tarjetas", "Mida de les targetes"),
            tr("Ajusta el tamaño de los pósteres.", "Ajusta la mida dels pòsters."),
            [tr("Normal", "Normal"), tr("Compacto", "Compacte"), tr("Grande", "Gran")][settings.cards].into()),
        8 => (tr("Actualizar biblioteca", "Actualitzar biblioteca"),
            tr("Busca cambios en el USB y actualiza la biblioteca y sus recomendaciones.",
                "Cerca canvis a l’USB i actualitza la biblioteca i les recomanacions."), String::new()),
        9 => (tr("Mostrar reloj", "Mostrar rellotge"),
            tr("Muestra la hora del televisor en la barra superior.", "Mostra l’hora del televisor a la barra superior."),
            on(settings.show_clock).into()),
        10 => (tr("Formato del reloj", "Format del rellotge"),
            tr("Automático sigue el formato del televisor.", "Automàtic segueix el format del televisor."),
            [tr("Automático", "Automàtic"), tr("12 horas", "12 hores"), tr("24 horas", "24 hores")][settings.clock_format].into()),
        11 => (tr("Tamaño del reloj", "Mida del rellotge"),
            tr("Ajusta la hora a tres tamaños. Pequeño conserva el tamaño original.",
                "Ajusta l’hora a tres mides. Petit conserva la mida original."),
            [tr("Pequeño", "Petit"), tr("Mediano", "Mitjà"), tr("Grande", "Gran")][settings.clock_size].into()),
        12 => (tr("Convertir DTS (experimental)", "Converteix DTS (experimental)"),
            tr("Convierte DTS 5.1 de 48 kHz a Dolby Digital al abrir el vídeo.", "Converteix DTS 5.1 de 48 kHz a Dolby Digital en obrir el vídeo."),
            on(settings.convert_dts).into()),
        DEMO_SETTING => (if demo { tr("Salir del modo demo", "Surt del mode demo") }
            else { tr("Activar modo demo", "Activa el mode demo") },
            tr("Explora una biblioteca USB ficticia. Cada sesión empieza de nuevo.",
                "Explora una biblioteca USB fictícia. Cada sessió comença de nou."), String::new()),
        _ => unreachable!("valid setting"),
    }
}

enum Dialog {
    Settings,
    Appearance,
    Accent,
    Cards,
    Extensions,
    Resume(u64),
    Next { volume: Volume, path: PathBuf, deadline: Instant },
    Video { volume: Volume, path: PathBuf },
    Delete { volume: Volume, path: PathBuf, finished_episode: bool },
    DeleteFolders,
    Eject(Volume),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Settings {
    language: Preference,
    forced_subtitles: bool,
    spanish_audio: bool,
    auto_next: bool,
    accent: usize,
    cards: usize,
    sort: usize,
    extensions: bool,
    show_clock: bool,
    clock_format: usize,
    clock_size: usize,
    convert_dts: bool,
}

enum SettingsJob {
    Load(Receiver<Result<Option<Settings>, String>>),
    Save(Receiver<Result<Settings, String>>),
}

struct Folder {
    volume: Volume,
    path: PathBuf,
    entries: Vec<Entry>,
    collection: Option<Cover>,
    selected: usize,
    scroll: f32,
}

struct FavoriteView {
    volume: Volume,
    paths: Vec<PathBuf>,
    depth: usize,
    selected: usize,
    scroll: f32,
    library_selected: usize,
    library_scroll: f32,
}

enum DiskResult {
    Demo(io::Result<demo::Library>),
    Folder(Volume, PathBuf, io::Result<Vec<Entry>>),
    Video(Result<PreparedVideo, String>),
    Delete(PathBuf, Result<(), String>),
    DeleteFolders { removed: Vec<OsString>, total: usize, error: Option<String>, entries: io::Result<Vec<Entry>> },
    Eject(Volume, Result<(), String>),
    Favorites(Volume, Vec<PathBuf>),
    Discovered(Volume, usize, Vec<PathBuf>),
}

type VolumeFacts = (Option<(u64, u64, u64, bool)>, Option<String>);
type VolumeFactsJob = (Volume, Receiver<VolumeFacts>);

struct PreparedVideo {
    file: File,
    info: plx_play_feed::LocalMedia,
    volume: Volume,
    path: PathBuf,
    title: String,
    key: Option<String>,
    resume: u64,
    saved: u64,
    direct_resume: bool,
}

struct PlayingVideo {
    volume: Volume,
    path: PathBuf,
    key: Option<String>,
    position: u64,
    duration: Option<u64>,
    saved: u64,
    next_save: Instant,
    seen_player: bool,
    observed_playhead: bool,
    paused: bool,
}

type DetailKey = (PathBuf, Option<u64>, Option<i64>);

enum HomeFocus {
    Drive(Volume),
    Resume(PathBuf),
    Recent(PathBuf),
    More,
}

struct CardLabel {
    show: Option<PathBuf>,
    movie: bool,
    source_title: Option<String>,
    title: String,
    category: Category,
    numbers: Option<(u32, u32)>,
    episodic: bool,
}

struct DetailsRequest {
    volume: Volume,
    key: DetailKey,
    reply: Receiver<plx_play_feed::LocalVideoDetails>,
}

const SERIES_REFRESH: Duration = Duration::from_secs(60);

struct SeriesCard<'a> {
    volume: &'a Volume,
    path: PathBuf,
    modified: Option<i64>,
    known: bool,
}

struct SeriesRequest {
    volume: Volume,
    path: PathBuf,
    modified: Option<i64>,
    known: bool,
    invalidated: bool,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    reply: Receiver<Option<series::Listing>>,
}

struct SeriesCacheEntry {
    checked: Instant,
    modified: Option<i64>,
    known: bool,
    summary: Option<series::Summary>,
}

struct HomeRequest {
    preview: Option<Receiver<Vec<home::Item>>>,
    reply: Receiver<io::Result<home::Dashboard>>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    volumes: Vec<Volume>,
    manual: bool,
}

struct ScanReply {
    scan: Scan,
    clock: Option<i18n::Clock>,
}

impl SeriesCacheEntry {
    fn fresh(&self, card: &SeriesCard<'_>, now: Instant) -> bool {
        now.duration_since(self.checked) < SERIES_REFRESH
            && (card.modified.is_none() || card.modified == self.modified)
            && (!card.known || self.known || self.summary.is_some())
    }
}

pub(super) struct Browser {
    demo: Option<demo::Library>,
    real_browser: Option<Box<Browser>>,
    scan: Scan,
    folders: Vec<Folder>,
    selected: usize,
    scroll: Spring,
    pop: GridPop,
    status: String,
    hour: Option<i32>,
    clock_time: Option<(i32, i32)>,
    tv_clock: i18n::Clock,
    next_clock_tick: Instant,
    next_clock_format: Instant,
    scan_phase: u32,
    action: Option<Action>,
    action_hover: Option<Action>,
    dialog: Option<Dialog>,
    option: usize,
    mutating: bool,
    selection: Option<BTreeSet<OsString>>,
    progress: Option<Receiver<(usize, usize)>>,
    settings: Settings,
    settings_job: Option<SettingsJob>,
    settings_started: bool,
    pending: Option<Receiver<DiskResult>>,
    retired_reads: Vec<Receiver<DiskResult>>,
    scanning: Option<Receiver<ScanReply>>,
    retired_scans: Vec<Receiver<ScanReply>>,
    details_job: Option<DetailsRequest>,
    details: BTreeMap<DetailKey, plx_play_feed::LocalVideoDetails>,
    media_art: media_card::Art,
    series_job: Option<SeriesRequest>,
    series_cache: BTreeMap<PathBuf, SeriesCacheEntry>,
    dashboard: home::Dashboard,
    inventory_paths: BTreeMap<PathBuf, (usize, usize)>,
    home_recent: Vec<home::Recent>,
    home_resume_start: usize,
    home_resume_scroll: Spring,
    home_disk_start: usize,
    home_all: bool,
    card_labels: std::cell::RefCell<BTreeMap<DetailKey, CardLabel>>,
    home_job: Option<HomeRequest>,
    home_refresh: Instant,
    home_first_open: Option<Instant>,
    home_force_refresh: bool,
    manual_refresh: Option<Volume>,
    refresh_notice: Option<(String, Instant)>,
    facts: Option<(Volume, VolumeFacts)>,
    facts_job: Option<VolumeFactsJob>,
    prepared: Option<PreparedVideo>,
    ready: Option<PreparedVideo>,
    playing: Option<PlayingVideo>,
    progress_job: Option<std::thread::JoinHandle<()>>,
    queued_progress: std::collections::BTreeMap<String, u64>,
    countdown_display: u64,
    history: BTreeMap<String, BTreeMap<String, u64>>,
    history_job: Option<(String, Receiver<Option<BTreeMap<String, u64>>>)>,
    history_loaded: BTreeSet<String>,
    queued_history_clears: BTreeSet<String>,
    loading_favorites: bool,
    favorite_view: Option<FavoriteView>,
    media_view: Option<FavoriteView>,
    nav_tab: usize,
    nav_focus: Option<usize>,
    nav_hover: Option<usize>,
    nav_strip: widgets::TabStrip,
    movie_filter: MovieFilter,
    movie_entries: Vec<usize>,
    movie_count: usize,
    movie_pending: usize,
    quality_focus: Option<usize>,
    quality_hover: Option<usize>,
    quality_strip: widgets::TabStrip,
    discovery_cache: BTreeMap<(PathBuf, u64, usize), Vec<PathBuf>>,
    favorite_section: (bool, Instant, f32),
    dialog_started: Option<Instant>,
    settings_exit: Option<Instant>,
    settings_return_focus: Option<usize>,
    button_motion: Option<(Rect, Instant)>,
    settings_focus_y: Spring,
    favorite_motion: Option<(PathBuf, bool, Instant)>,
    ejecting: Option<(Volume, Instant)>,
    eject_feedback: Option<(Volume, bool, Instant)>,
    eject_uncertain: bool,
    estimate: Option<(i64, plx_appkit::player_hud::model::FinishClock)>,
    next_estimate: Option<Instant>,
    estimate_anchor: Option<(i64, i32)>,
    next_scan: Instant,
    last_tick: Instant,
}

impl Browser {
    pub(super) fn new() -> Self {
        let now = Instant::now();
        Self {
            demo: None, real_browser: None,
            scan: Scan::default(), folders: Vec::new(), selected: 0,
            scroll: Spring::at(0.0), pop: GridPop::new(), status: String::new(),
            hour: None, clock_time: None, tv_clock: i18n::current().clock(), next_clock_tick: now,
            next_clock_format: now, scan_phase: 0,
            action: None, action_hover: None, dialog: None, option: 0, mutating: false,
            selection: None, progress: None,
            settings: Settings { language: Preference::Es, forced_subtitles: false, spanish_audio: false, auto_next: true, accent: 0, cards: 0, sort: 0, extensions: false, show_clock: true, clock_format: 0, clock_size: 1, convert_dts: false },
            settings_job: None, settings_started: false,
            pending: None, scanning: None, next_scan: now, last_tick: now,
            retired_reads: Vec::new(), retired_scans: Vec::new(),
            details_job: None, details: BTreeMap::new(),
            series_job: None, series_cache: BTreeMap::new(),
            media_art: Default::default(),
            dashboard: home::Dashboard { drives: vec![], items: vec![], refreshed_folder: None }, home_job: None,
            inventory_paths: BTreeMap::new(), card_labels: Default::default(),
            home_recent: Vec::new(), home_resume_start: 0, home_resume_scroll: Spring::at(0.0), home_disk_start: 0, home_all: false,
            home_refresh: now, home_first_open: Some(now), home_force_refresh: false,
            manual_refresh: None, refresh_notice: None,
            facts: None, facts_job: None, prepared: None, ready: None, playing: None,
            progress_job: None, queued_progress: Default::default(), countdown_display: 0,
            history: BTreeMap::new(), history_job: None, history_loaded: BTreeSet::new(),
            queued_history_clears: BTreeSet::new(), loading_favorites: false, favorite_view: None,
            media_view: None,
            nav_tab: 0, nav_focus: None, nav_hover: None, nav_strip: widgets::TabStrip::new(), discovery_cache: BTreeMap::new(),
            movie_filter: MovieFilter::All, movie_entries: Vec::new(), movie_count: 0, movie_pending: 0,
            quality_focus: None, quality_hover: None, quality_strip: widgets::TabStrip::new(),
            favorite_section: (false, now, 0.0),
            dialog_started: None, settings_exit: None, settings_return_focus: None,
            button_motion: None, settings_focus_y: Spring::at(unified_setting_rect(0).y), favorite_motion: None,
            ejecting: None, eject_feedback: None, eject_uncertain: false,
            estimate: None, next_estimate: None, estimate_anchor: None,
        }
    }

    fn len(&self) -> usize {
        if self.drive_picker() {
            return self.home_more_index() + usize::from(self.dashboard.items.len() > 3);
        }
        if self.tab_waiting_for_volume() { return 0; }
        if let Some(view) = self.media_cards() {
            return if self.documental_folder_picker(view) { view.paths.len() } else { 0 };
        }
        if self.movie_grid() { return self.movie_entries.len(); }
        self.favorite_cards().map_or_else(|| self.folders.last().map_or(self.scan.volumes.len(), |folder| folder.entries.len()), |view| view.paths.len())
    }

    fn movie_grid(&self) -> bool {
        self.nav_tab == MOVIES_TAB && self.media_view.as_ref().is_some_and(|view| self.folders.len() > view.depth)
    }

    fn folder_index(&self, index: usize) -> Option<usize> {
        if self.movie_grid() { self.movie_entries.get(index).copied() } else { Some(index) }
    }

    fn folder_entry(&self, index: usize) -> Option<&Entry> {
        self.folders.last()?.entries.get(self.folder_index(index)?)
    }

    fn visible_index_of(&self, name: Option<&std::ffi::OsStr>) -> Option<usize> {
        let name = name?;
        (0..self.len()).find(|&index| self.folder_entry(index).is_some_and(|entry| entry.name == name))
    }

    fn rebuild_movie_grid(&mut self) {
        self.movie_entries.clear();
        self.movie_count = 0;
        self.movie_pending = 0;
        if !self.movie_grid() { return; }
        let folder = self.folders.last().expect("movie directory is open");
        for (index, entry) in folder.entries.iter().enumerate() {
            // Keep subfolders navigable under every filter. Count films only.
            if entry.kind == Kind::Folder {
                self.movie_entries.push(index);
                continue;
            }
            let details = self.details.get(&Self::detail_key(folder, entry));
            let quality = details.and_then(|d| d.resolution)
                .and_then(|(width, height)| MovieQuality::of(width, height));
            self.movie_pending += usize::from(details.is_none());
            if self.movie_filter.matches(quality) {
                self.movie_entries.push(index);
                self.movie_count += 1;
            }
        }
    }

    fn accept_video_details(&mut self, key: DetailKey, details: plx_play_feed::LocalVideoDetails) {
        let quality = |details: &plx_play_feed::LocalVideoDetails| {
            details.resolution.and_then(|(width, height)| MovieQuality::of(width, height))
        };
        let new_quality = quality(&details);
        let previous = self.details.insert(key.clone(), details);
        if !self.movie_grid() { return; }
        let folder = self.folders.last().expect("movie directory is open");
        if key.0.parent() != Some(folder.path.as_path()) || !folder.entries.iter().any(|entry| {
            entry.kind == Kind::Video && key.0.file_name() == Some(entry.name.as_os_str())
                && key.1 == entry.size && key.2 == entry.modified
        }) { return; }
        if self.movie_filter.matches(previous.as_ref().and_then(quality)) != self.movie_filter.matches(new_quality) {
            self.refresh_movie_grid();
        } else if previous.is_none() {
            self.movie_pending = self.movie_pending.saturating_sub(1);
        }
    }

    fn refresh_movie_grid(&mut self) {
        if !self.movie_grid() { return; }
        let selected = self.folder_entry(self.selected).map(|entry| entry.name.clone());
        self.rebuild_movie_grid();
        let index = self.visible_index_of(selected.as_deref()).unwrap_or(self.selected);
        if index != self.selected || self.selected >= self.len() { self.select(index, true); }
    }

    fn choose_movie_filter(&mut self, index: usize) {
        let filter = MovieFilter::TABS[index];
        if self.movie_filter == filter { return; }
        self.movie_filter = filter;
        self.rebuild_movie_grid();
        self.restore_view(0, 0.0);
    }

    fn focus_movie_filter(&mut self) -> bool {
        if !self.movie_grid() { return false; }
        self.quality_focus = MovieFilter::TABS.iter().position(|&value| value == self.movie_filter);
        true
    }

    fn request(&mut self, run: impl FnOnce() -> DiskResult + Send + 'static) {
        let (send, receive) = mpsc::sync_channel(1);
        if plx_base::task::spawn_off_frame("cineusb-disk", move |_| { let _ = send.send(run()); }) {
            self.pending = Some(receive);
        } else {
            self.status = tr("No se pudo iniciar la operación.", "No s’ha pogut iniciar l’operació.").into();
        }
    }

    fn update(&mut self) -> Option<PreparedVideo> {
        self.retired_reads.retain(|reply| matches!(reply.try_recv(), Err(TryRecvError::Empty)));
        self.retired_scans.retain(|reply| matches!(reply.try_recv(), Err(TryRecvError::Empty)));
        let now = Instant::now();
        let dt = (now - self.last_tick).as_secs_f32().min(0.05);
        self.last_tick = now;
        let mut changed = false;
        let mut video = self.ready.take();
        if let Some(Dialog::Next { deadline, .. }) = self.dialog {
            let seconds = deadline.saturating_duration_since(now).as_millis().div_ceil(1000) as u64;
            if self.countdown_display != seconds {
                self.countdown_display = seconds;
                changed = true;
            }
            if seconds == 0 {
                self.option = 0;
                self.choose_option();
            }
        }
        self.flush_progress(false);
        changed |= self.update_settings();
        if let Some((processed, total)) = self.progress.as_ref().and_then(|progress| progress.try_iter().last()) {
            self.status = format!("{} {processed}/{total}", tr("Elementos procesados", "Elements processats"));
            changed = true;
        }
        match self.pending.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => {
                self.pending = None;
                self.mutating = false;
                self.progress = None;
                changed = true;
                match result {
                    DiskResult::Demo(Ok(library)) => {
                        self.enter_demo(library);
                        plx_machine::idle::invalidate();
                        return None;
                    }
                    DiskResult::Demo(Err(error)) => {
                        plx_base::eventlog::log(&format!("cineusb: demo setup failed: {error}"));
                        self.status = tr("No se pudo abrir el modo demo.", "No s’ha pogut obrir el mode demo.").into();
                    }
                    DiskResult::Folder(volume, path, Ok(mut entries)) => {
                        disk::sort_entries_in(&mut entries, self.settings.sort, path.strip_prefix(&volume.path).unwrap_or(&path));
                        let collection = path.strip_prefix(&volume.path).ok().and_then(art::collection);
                        self.folders.push(Folder { volume, path, entries, collection, selected: 0, scroll: 0.0 });
                        self.rebuild_movie_grid();
                        self.select(0, true);
                        self.status.clear();
                    }
                    DiskResult::Folder(volume, path, Err(error)) => {
                        plx_base::eventlog::log(&format!("cineusb: USB folder listing failed: {error}"));
                        // A directory renamed on Windows can remain in the
                        // navigation stack after the same USB disk is reinserted.
                        // Recover manual browsing at the live mount root. A media
                        // tab retains its own navigation floor and reports the error.
                        if self.media_view.is_none() && path != volume.path && self.scan.volumes.contains(&volume) {
                            self.discovery_cache.retain(|(root, _, _), _| *root != volume.path);
                            self.invalidate_series_counts();
                            self.leave_media();
                            self.leave_favorites();
                            self.folders.clear();
                            self.nav_tab = 0;
                            self.open_folder(volume.clone(), volume.path.clone());
                        } else {
                            self.status = tr("No se pudo abrir la carpeta. Comprueba el USB.",
                                "No s’ha pogut obrir la carpeta. Comprova l’USB.").into();
                        }
                    }
                    DiskResult::Video(Ok(media)) => {
                        if media.resume > 0 && !media.direct_resume {
                            self.dialog = Some(Dialog::Resume(media.resume));
                            self.option = 0;
                            self.prepared = Some(media);
                        } else { video = Some(media); }
                        self.status.clear();
                    }
                    DiskResult::Video(Err(error)) => {
                        plx_base::eventlog::log(&format!("cineusb: video probe failed: {error}"));
                        self.status = tr("No se pudo abrir el vídeo.", "No s’ha pogut obrir el vídeo.").into();
                        self.refresh_home(true);
                    }
                    DiskResult::Delete(path, result) => {
                        if result.is_ok() {
                            if let Some(folder) = self.folders.last_mut().filter(|folder| path.parent() == Some(folder.path.as_path())) {
                                folder.entries.retain(|entry| path.file_name() != Some(entry.name.as_os_str()));
                            }
                            self.rebuild_movie_grid();
                            self.select(self.selected, true);
                        }
                        self.facts = None;
                        self.finish_mutation(result, tr("Archivo eliminado.", "Fitxer esborrat."),
                            tr("No se pudo eliminar el archivo. Comprueba si el disco admite escritura.",
                                "No s’ha pogut esborrar el fitxer. Comprova si es pot escriure al disc."));
                    }
                    DiskResult::DeleteFolders { removed, total, error, entries } => {
                        self.invalidate_series_counts();
                        self.refresh_home(true);
                        self.facts = None;
                        if let Some(folder) = self.folders.last_mut() {
                            match entries {
                                Ok(mut entries) => {
                                    disk::sort_entries_in(&mut entries, self.settings.sort,
                                        folder.path.strip_prefix(&folder.volume.path).unwrap_or(&folder.path));
                                    folder.entries = entries;
                                }
                                Err(_) => folder.entries.retain(|entry| !removed.contains(&entry.name)),
                            }
                        }
                        self.selection = None;
                        self.action = None;
                        self.rebuild_movie_grid();
                        self.select(self.selected, true);
                        self.status = format!("{}: {}/{total}", tr("Elementos eliminados", "Elements esborrats"), removed.len());
                        if let Some(error) = error {
                            self.status.push_str(" · ");
                            self.status.push_str(&error);
                        }
                        self.retire_scan();
                        self.next_scan = Instant::now();
                    }
                    DiskResult::Discovered(volume, kind, paths) => {
                        if self.scan.volumes.contains(&volume) {
                            self.discovery_cache.insert((volume.path.clone(), volume.mount_id.unwrap_or(0), kind), paths.clone());
                            self.enter_media(volume, kind, paths);
                            if self.media_cards().is_some_and(|view| view.paths.is_empty()) {
                                self.status = tr("No se encontraron carpetas. Puedes navegar desde Inicio.",
                                    "No s'han trobat carpetes. Pots navegar des d'Inici.").into();
                            } else { self.status.clear(); }
                        }
                    }
                    DiskResult::Favorites(volume, paths) => {
                        self.loading_favorites = false;
                        if self.nav_tab == FAVORITES_TAB && self.volume() == Some(&volume) {
                            self.enter_favorites(volume, paths);
                            self.status.clear();
                        } else if self.favorite_view.is_none() {
                            self.set_favorite_section(false);
                        }
                    }
                    DiskResult::Eject(volume, result) => {
                        let success = result.is_ok();
                        self.eject_uncertain = result.as_ref().err()
                            .is_some_and(|error| error.starts_with("safe eject unconfirmed:"));
                        if success {
                            self.scan.volumes.retain(|item| *item != volume);
                            self.discovery_cache.retain(|(path, _, _), _| *path != volume.path);
                            if self.media_view.as_ref().is_some_and(|view| view.volume == volume) { self.leave_media(); }
                            self.leave_favorites();
                            self.folders.clear();
                            self.action = None;
                            self.select(0, true);
                        }
                        self.finish_mutation(result, tr("USB expulsado correctamente.", "USB expulsat correctament."),
                            tr("No se pudo expulsar el USB. Cierra las apps que lo usan y vuelve a intentarlo.",
                                "No s'ha pogut expulsar l'USB. Tanca les aplicacions que el fan servir i torna-ho a provar."));
                        self.ejecting = None;
                        self.eject_feedback = Some((volume, success, Instant::now()));
                    }
                }
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.pending = None;
                self.loading_favorites = false;
                self.mutating = false;
                self.progress = None;
                if let Some((volume, _)) = self.ejecting.take() {
                    self.eject_feedback = Some((volume, false, Instant::now()));
                }
                self.status = tr("Se interrumpió la operación.", "L’operació s’ha interromput.").into();
                changed = true;
            }
            _ => {}
        }
        if self.pending.is_none() {
            match self.scanning.as_ref().map(Receiver::try_recv) {
                Some(Ok(ScanReply { scan, clock })) => {
                    if let Some(clock) = clock {
                        changed |= self.settings.show_clock && self.tv_clock != clock;
                        self.tv_clock = clock;
                    }
                    self.scanning = None;
                    // A temporary Homebrew failure must not bring back an old mount from the jail.
                    if (!self.scan.system || scan.system)
                        && (self.scan != scan || self.status == tr("Buscando discos USB…", "Buscant discos USB…")) {
                        let home_focus = self.home_focus();
                        self.details.clear();
                        self.refresh_movie_grid();
                        self.invalidate_series_counts();
                        self.scan = scan;
                        self.discovery_cache.retain(|(path, id, _), _| self.scan.volumes.iter()
                            .any(|volume| volume.path == *path && volume.mount_id.unwrap_or(0) == *id));
                        if self.media_view.as_ref().is_some_and(|view| !self.scan.volumes.contains(&view.volume)) {
                            self.leave_media();
                        }
                        if self.favorite_view.as_ref().is_some_and(|view| !self.scan.volumes.contains(&view.volume))
                            || self.folders.last().is_some_and(|folder| !self.scan.volumes.contains(&folder.volume)) {
                            self.leave_favorites();
                            self.folders.clear();
                            self.dialog = None;
                            self.prepared = None;
                            self.ready = None;
                            self.loading_favorites = false;
                            self.selection = None;
                            if !self.status.is_empty() { self.status.push_str(" · "); }
                            self.status.push_str(tr("Se ha retirado el disco USB.", "S’ha retirat el disc USB."));
                            video = None;
                        }
                        if self.drive_picker() {
                            self.restore_home_selection(home_focus);
                        }
                        if self.status == tr("Buscando discos USB…", "Buscant discos USB…") { self.status.clear(); }
                        changed = true;
                    }
                }
                Some(Err(TryRecvError::Disconnected)) => self.scanning = None,
                _ => {}
            }
            if self.demo.is_none() && !self.mutating && self.scanning.is_none() && now >= self.next_scan {
                self.next_scan = now + Duration::from_secs(5);
                let hour = i18n::local_hour();
                changed |= hour != self.hour;
                self.hour = hour;
                let read_clock = now >= self.next_clock_format;
                if read_clock { self.next_clock_format = now + Duration::from_secs(60); }
                let (send, receive) = mpsc::sync_channel(1);
                if plx_base::task::spawn_off_frame("cineusb-volumes", move |_| {
                    let _ = send.send(ScanReply { scan: scan_volumes(),
                        clock: read_clock.then(i18n::system_clock_preference) });
                }) {
                    self.scanning = Some(receive);
                }
            }
        }
        changed |= self.update_volume_facts();
        changed |= self.update_history();
        if self.loading_favorites && self.pending.is_none() && self.facts.as_ref().is_some_and(|(disk, (_, identity))|
            self.volume() == Some(disk) && identity.as_ref().is_none_or(|identity| self.history_loaded.contains(&history_prefix(identity)))) {
            self.loading_favorites = false;
            self.show_favorites();
        }
        if self.dialog.is_some() {
            let started = *self.dialog_started.get_or_insert(now);
            let duration = if matches!(self.dialog, Some(Dialog::Settings)) { SETTINGS_MOTION } else { MOTION };
            changed |= now.duration_since(started) < duration;
        } else { self.dialog_started = None; }
        if self.settings_exit.is_some_and(|started| now.duration_since(started) < SETTINGS_MOTION) {
            changed = true;
        } else { self.settings_exit = None; }
        changed |= self.button_motion.is_some_and(|(_, started)| now.duration_since(started) < MOTION);
        if matches!(self.dialog, Some(Dialog::Settings)) {
            let target = unified_setting_rect(self.option).y;
            let previous = self.settings_focus_y.pos;
            self.settings_focus_y.step(target, 240.0, dt);
            self.settings_focus_y.land(target, dt);
            changed |= previous != self.settings_focus_y.pos;
        }
        changed |= self.favorite_motion.as_ref().is_some_and(|(_, _, started)| now.duration_since(*started) < FAVORITE_MOTION);
        changed |= self.favorite_section.1.elapsed() < MOTION;
        let selected_tab = if self.favorite_section.0 { FAVORITES_TAB } else { self.nav_tab };
        let focused_tab = self.nav_hover.or(self.nav_focus).unwrap_or(selected_tab);
        self.nav_strip.update(selected_tab as i32, focused_tab as i32,
            |index| NAV_TABS.get(index).map(|rect| (rect.x, rect.w)), widgets::SelMark::Travels, dt);
        changed |= self.nav_strip.moving();
        changed |= self.step_movie_filter(dt);
        if self.eject_feedback.as_ref().is_some_and(|(_, success, start)| *success && start.elapsed() >= Duration::from_secs(3)) {
            self.eject_feedback = None;
            changed = true;
        }
        changed |= self.eject_feedback.as_ref().is_some_and(|(_, success, start)|
            *success && start.elapsed() < Duration::from_secs(3));
        changed |= self.ejecting.as_ref().is_some_and(|(_, start)| start.elapsed() < Duration::from_millis(800));
        changed |= self.update_video_details();
        changed |= self.update_series_counts(now);
        changed |= self.update_home(now);
        if self.drive_picker() && !self.home_all {
            let target = self.home_resume_start.min(self.dashboard.items.len().saturating_sub(3)) as f32 * HOME_CARD_PITCH;
            let previous = self.home_resume_scroll.pos;
            self.home_resume_scroll.step(target, 240.0, dt);
            self.home_resume_scroll.land(target, dt);
            changed |= previous != self.home_resume_scroll.pos;
        }
        if self.demo.is_none() && self.drive_picker() && !self.dashboard.items.is_empty() {
            if let Some(opened) = self.home_first_open.take() {
                plx_base::eventlog::log(&format!("cineusb: first Continue Watching cards ready after {} ms", opened.elapsed().as_millis()));
            }
        }
        if now >= self.next_clock_tick {
            self.next_clock_tick = now + Duration::from_secs(1);
            let time = i18n::local_clock_time();
            changed |= self.settings.show_clock && self.clock_time != time;
            self.clock_time = time;
        }
        if self.library_scanning() {
            self.scan_phase = self.scan_phase.wrapping_add((dt * 1000.0) as u32);
        }
        if self.refresh_notice.as_ref().is_some_and(|(_, start)| start.elapsed() >= Duration::from_secs(5)) {
            self.refresh_notice = None;
            changed = true;
        }
        if self.mutating && self.ejecting.is_some() && self.usb_reads_drained() {
            let volume = self.ejecting.as_ref().expect("eject requested").0.clone();
            self.request(move || {
                let result = mutate_usb(Mutation::Eject, &volume, &volume.path);
                DiskResult::Eject(volume, result)
            });
            if self.pending.is_none() {
                self.mutating = false;
                if let Some((volume, _)) = self.ejecting.take() {
                    self.eject_feedback = Some((volume, false, Instant::now()));
                }
            }
        }
        if self.action.is_some_and(|action| !self.action_available(action)) {
            self.action = None;
            changed = true;
        }

        let target = self.scroll_target();
        let before = self.scroll.pos;
        self.scroll.step(target, 240.0, dt);
        self.scroll.land(target, dt);
        let grid_focused = self.len() > 0 && self.grid_has_focus();
        let focused = grid_focused.then_some(Card { cell: self.selected, key: self.selected as u64 });
        let scale = self.pop.scale(self.selected, self.pop.cell() == Some(self.selected), &USB_STYLE);
        if let Some(card) = focused.filter(|card| self.drive_picker() && self.pop.cell() != Some(card.cell)) {
            self.pop.arm(card);
        }
        self.pop.tick(focused, &USB_STYLE, dt);
        if changed || before != self.scroll.pos || scale != self.pop.scale(self.selected, grid_focused, &USB_STYLE) {
            plx_machine::idle::invalidate();
        }
        video
    }

    fn scroll_target(&self) -> f32 {
        if self.drive_picker() { return 0.0; }
        let grid = self.grid();
        let row = self.selected / grid.cols;
        let viewport = GRID_BOTTOM - self.grid_top() - 22.0;
        let top = row as f32 * grid.pitch();
        self.scroll.pos.clamp((top + grid.h - viewport).max(0.0), top)
    }

    fn drive_picker(&self) -> bool {
        self.nav_tab == 0 && self.folders.is_empty() && self.media_view.is_none() && self.favorite_view.is_none()
    }

    fn tab_waiting_for_volume(&self) -> bool {
        matches!(self.nav_tab, 1 | 2 | DOCUMENTALS_TAB | FAVORITES_TAB)
            && self.media_view.is_none() && self.favorite_view.is_none() && self.folders.is_empty()
            && !self.loading_favorites && !self.favorite_section.0
    }

    fn grid_has_focus(&self) -> bool {
        if self.action_hover.is_some() { return false; }
        if self.movie_grid() {
            return self.quality_focus.is_none() && self.quality_hover.is_none()
                && self.nav_focus.is_none() && self.nav_hover.is_none();
        }
        !self.drive_picker() || (self.nav_focus.is_none() && self.nav_hover.is_none())
    }

    fn usb_reads_drained(&self) -> bool {
        self.pending.is_none() && self.details_job.is_none() && self.series_job.is_none()
            && self.facts_job.is_none() && self.scanning.is_none()
            && self.retired_reads.is_empty() && self.retired_scans.is_empty()
            && self.home_job.is_none()
    }

    fn retire_read(&mut self) -> bool {
        if let Some(reply) = self.pending.take() {
            self.retired_reads.push(reply);
            return true;
        }
        false
    }

    fn retire_scan(&mut self) {
        if let Some(reply) = self.scanning.take() {
            self.retired_scans.push(reply);
        }
    }

    fn grid_top(&self) -> f32 {
        if self.media_library() { MEDIA_GRID_TOP }
        else if self.drive_picker() { HOME_GRID_TOP }
        else { TOP }
    }

    fn media_library(&self) -> bool {
        matches!(self.nav_tab, 1 | 2 | DOCUMENTALS_TAB)
    }

    fn grid(&self) -> Geom {
        if self.drive_picker() {
            return Geom { cols: 3, left: plx_ui::consts::MARGIN_X, w: 560.0, h: 210.0, gap: 24.0, value_line: false };
        }
        grid_geometry(self.len(), self.settings.cards, self.media_library())
    }

    fn visible_cards(&self) -> std::ops::Range<usize> {
        if self.drive_picker() { return 0..self.len(); }
        poster_grid::visible_in(&self.grid(), self.len(), self.grid_top(), self.scroll.pos)
    }

    fn media_series_name(&self) -> Option<std::borrow::Cow<'_, str>> {
        if self.nav_tab != 2 { return None; }
        let category = self.media_view.as_ref()?.paths.first()?;
        let current = &self.folders.last()?.path;
        let show = current.strip_prefix(category).ok()?.components().next()?;
        Some(show.as_os_str().to_string_lossy())
    }

    fn content_actions(&self) -> [(Action, Rect); 3] {
        if self.media_library() {
            ACTIONS.map(|(action, mut rect)| {
                rect.y -= 90.0;
                if !self.movie_grid() && self.action_available(Action::ToggleFavorite) {
                    let right = QUALITY_PILL.x + QUALITY_PILL.w;
                    if action == Action::ToggleFavorite {
                        rect.x = right - rect.w;
                        if self.action_available(Action::Select) {
                            rect.x -= ACTIONS[1].1.w + 14.0;
                        }
                    } else if action == Action::Select {
                        rect.x = right - rect.w;
                    }
                }
                if self.movie_grid() {
                    let right = QUALITY_PILL.x - 28.0;
                    match action {
                        Action::Select => {
                            rect.w = 344.0;
                            rect.x = right - rect.w;
                            if self.selection.as_ref().is_some_and(|marked| !marked.is_empty()) {
                                rect.x -= 372.0;
                            }
                        }
                        Action::DeleteFolders => { rect.x = right - 360.0; rect.w = 360.0; }
                        Action::ToggleFavorite => {}
                    }
                }
                (action, rect)
            })
        } else {
            ACTIONS
        }
    }

    fn tile_rect(&self, index: usize) -> Rect {
        if self.drive_picker() { return self.home_rect(index); }
        let mut grid = self.grid();
        if !self.media_library() {
            let first = index / grid.cols * grid.cols;
            let count = self.len().saturating_sub(first).min(grid.cols).max(1);
            let row_width = count as f32 * grid.w + (count - 1) as f32 * grid.gap;
            grid.left = (1920.0 - row_width) * 0.5;
        }
        poster_grid::cell_in(&grid, index, self.grid_top(), self.scroll.pos, &[])
    }

    fn update_volume_facts(&mut self) -> bool {
        if let Some(library) = &self.demo {
            let changed = self.facts.is_none();
            if changed { self.facts = Some((library.volume.clone(), demo::facts())); }
            return changed;
        }
        let mut changed = false;
        if let Some((volume, job)) = &self.facts_job {
            match job.try_recv() {
                Err(TryRecvError::Empty) => return false,
                result => {
                    self.facts = Some((volume.clone(), result.unwrap_or_default()));
                    self.facts_job = None;
                    changed = true;
                }
            }
        }
        if self.mutating { return changed; }
        let Some(volume) = self.volume().cloned() else { self.facts = None; return changed };
        if self.facts.as_ref().is_some_and(|(last, _)| *last == volume) { return changed; }
        let requested = volume.clone();
        let (send, receive) = mpsc::sync_channel(1);
        if plx_base::task::spawn_off_frame("cineusb-volume-details", move |_| {
            let _ = send.send(volume_facts(&requested));
        }) {
            self.facts_job = Some((volume, receive));
        }
        changed
    }

    fn flush_progress(&mut self, closing: bool) {
        if self.demo.is_some() { return; }
        if self.progress_job.as_ref().is_some_and(|job| closing || job.is_finished()) {
            if let Some(job) = self.progress_job.take() { let _ = job.join(); }
        }
        if self.progress_job.is_some() || (self.queued_progress.is_empty() && self.queued_history_clears.is_empty()) { return; }
        let updates = std::mem::take(&mut self.queued_progress);
        let clears = std::mem::take(&mut self.queued_history_clears);
        let job = plx_base::task::spawn_off_frame_keeping("cineusb-playhead", move |_| {
            for base in clears {
                if let Err(error) = clear_history(&base) {
                    plx_base::eventlog::log(&format!("cineusb: could not clear local history: {error}"));
                }
            }
            for (key, position) in updates {
                if let Err(error) = resume_record(&key, Some(position)) {
                    plx_base::eventlog::log(&format!("cineusb: could not save local playhead: {error}"));
                }
            }
        });
        if closing {
            if let Some(job) = job { let _ = job.join(); }
        } else {
            self.progress_job = job;
        }
    }

    fn store_playhead(&mut self, completed: bool, leaving: bool) {
        let Some(playing) = &mut self.playing else { return };
        let now = Instant::now();
        if !leaving && now < playing.next_save { return; }
        playing.next_save = now + Duration::from_secs(30);
        let position = if completed || playing.saved == WATCHED {
            WATCHED
        } else if playing.position > 0 {
            pack_progress(playing.position, playing.duration)
        } else { playing.saved };
        if position != playing.saved {
            playing.saved = position;
            if let Some(key) = playing.key.clone() { self.change_history(key, position); }
        }
    }

    fn finish_video(&mut self, completed: bool) {
        self.store_playhead(completed, true);
        self.estimate = None;
        self.next_estimate = None;
        self.estimate_anchor = None;
        let Some(playing) = self.playing.take() else { return };
        if !completed { return; }
        if self.scan.volumes.contains(&playing.volume)
            && system_mount(&playing.volume).is_some()
            && self.video_is_episode(&playing.volume, &playing.path)
        {
            self.dialog = Some(Dialog::Delete { volume: playing.volume, path: playing.path, finished_episode: true });
            self.option = 0;
            self.action = None;
            self.nav_focus = None;
            self.nav_hover = None;
            plx_machine::idle::invalidate();
            return;
        }
        if !self.settings.auto_next { return; }
        let identity = self.identity(&playing.volume);
        let history = identity.and_then(|identity| self.history.get(&history_prefix(identity)));
        let next = match self.dashboard.drives.iter().find(|drive| drive.volume == playing.volume && drive.stats.is_some()) {
            Some(drive) => home::next_episode(drive, &playing.path, history),
            None if !self.is_library_movie(&playing.volume, &playing.path) => {
                let show = self.episode_show(&playing.volume, &playing.path);
                self.folders.last().filter(|folder| folder.volume == playing.volume
                    && playing.path.parent() == Some(folder.path.as_path()))
                    .and_then(|folder| home::next_listed_episode(folder, &playing.path, show.as_deref(), identity?, history?))
            }
            None => None,
        };
        let Some(next) = next else { return };
        self.countdown_display = 10;
        self.dialog = Some(Dialog::Next { volume: playing.volume, path: next,
            deadline: Instant::now() + Duration::from_secs(10) });
        self.option = 1;
        self.action = None;
        plx_machine::idle::invalidate();
    }

    fn identity(&self, volume: &Volume) -> Option<&str> {
        self.facts.as_ref().filter(|(disk, _)| disk == volume).and_then(|(_, (_, identity))| identity.as_deref())
            .or_else(|| self.dashboard.drives.iter().find(|drive| &drive.volume == volume).and_then(|drive| drive.facts.1.as_deref()))
    }

    fn update_history(&mut self) -> bool {
        let mut changed = false;
        if let Some((prefix, job)) = &self.history_job {
            match job.try_recv() {
                Err(TryRecvError::Empty) => return false,
                result => {
                    let records = self.history.entry(prefix.clone()).or_default();
                    match result {
                        Ok(Some(values)) => {
                            for (key, value) in values {
                                records.entry(key).or_insert(value);
                            }
                            self.history_loaded.insert(prefix.clone());
                        }
                        // A transport or helper failure does not mean empty history.
                        // Keep the feature unavailable until we can read saved data;
                        // otherwise a later write could overwrite the actual resume point.
                        _ => {
                            plx_base::eventlog::log("cineusb: history unavailable; keeping saved data intact");
                            self.status = tr("No se pudo cargar el historial USB.", "No s'ha pogut carregar l'historial USB.").into();
                        }
                    }
                    self.history_job = None;
                    self.refresh_series_watched(None);
                    changed = true;
                }
            }
        }
        let Some(prefix) = self.volume().and_then(|volume| self.identity(volume)).map(history_prefix) else { return changed };
        if self.history_loaded.contains(&prefix) { return changed; }
        let key = prefix.clone();
        let (send, receive) = mpsc::sync_channel(1);
        if plx_base::task::spawn_off_frame("cineusb-history", move |_| { let _ = send.send(read_history(&key)); }) {
            self.history_job = Some((prefix, receive));
        }
        changed
    }

    fn history_value(&self, key: &str) -> Option<u64> {
        let end = key.as_bytes().chunks_exact(2).position(|pair| pair == b"00")?;
        self.history.get(&key[..2 * (end + 1)])?.get(key).copied()
    }

    fn file_record(&self, volume: &Volume, key: &DetailKey) -> u64 {
        self.identity(volume).and_then(|identity| {
            let records = self.history.get(&history_prefix(identity))?;
            Some(file_record(records, identity, volume, key))
        }).unwrap_or(0)
    }

    fn watched_episodes(&self, volume: &Volume, listing: &series::Listing) -> usize {
        let Some(identity) = self.identity(volume) else { return 0; };
        let Some(records) = self.history.get(&history_prefix(identity)) else { return 0; };
        listing.watched(|key| file_record(records, identity, volume, key) == WATCHED)
    }

    fn refresh_series_watched(&mut self, changed: Option<&str>) {
        for volume in self.scan.volumes.clone() {
            let Some(identity) = self.identity(&volume).map(str::to_owned) else { continue; };
            let prefix = history_prefix(&identity);
            let Some(records) = self.history.get(&prefix) else { continue; };
            if changed.is_some_and(|key| !key.starts_with(&prefix)) { continue; }
            for (path, summary) in self.series_cache.iter_mut().filter_map(|(path, entry)| {
                path.starts_with(&volume.path).then_some((path, entry.summary.as_mut()?))
            }) {
                if let Some(key) = changed {
                    let Some(base) = path_key(&identity, &volume, path) else { continue; };
                    if !key.strip_prefix(&base).is_some_and(|tail| tail.starts_with("2f")) { continue; }
                }
                summary.watched = summary.listing.watched(|key| file_record(records, &identity, &volume, key) == WATCHED);
            }
        }
    }

    fn change_history(&mut self, key: String, value: u64) {
        // The identity is separated from the filename by an encoded NUL. Only look at byte pairs.
        let Some(end) = key.as_bytes().chunks_exact(2).position(|pair| pair == b"00") else { return };
        let prefix = &key[..2 * (end + 1)];
        let records = self.history.entry(prefix.to_owned()).or_default();
        let previous = records.insert(key.clone(), value);
        if previous != Some(value) && (value == WATCHED || previous == Some(WATCHED)) {
            self.refresh_series_watched(Some(&key));
        }
        if previous == Some(FAVORITE) && value != FAVORITE {
            // Drop the visible recommendation immediately while the refreshed inventory is pending.
            if let Some(relative) = key.strip_prefix(prefix).and_then(|tail| tail.strip_suffix("00")).and_then(decode_relative) {
                let volumes: Vec<_> = self.scan.volumes.iter()
                    .filter(|volume| self.identity(volume).is_some_and(|identity| history_prefix(identity) == prefix))
                    .cloned().collect();
                let selected_path = self.home_focus();
                self.dashboard.items.retain(|item| {
                    !item.favorite || !volumes.contains(&item.volume) || !item.path.starts_with(item.volume.path.join(&relative))
                });
                self.restore_home_selection(selected_path);
            }
        }
        self.home_force_refresh |= value == FAVORITE || previous == Some(FAVORITE);
        if self.demo.is_none() {
            self.queued_progress.insert(key, value);
            self.flush_progress(false);
        }
        let delay = if value == FAVORITE { Duration::ZERO } else { Duration::from_secs(1) };
        self.home_refresh = Instant::now() + delay;
        if let Some(job) = &self.home_job {
            job.cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        plx_machine::idle::invalidate();
    }

    fn favorites(&self, volume: &Volume) -> Vec<PathBuf> {
        let Some(identity) = self.identity(volume) else { return Vec::new() };
        let prefix = history_prefix(identity);
        self.history.get(&prefix).into_iter().flatten().filter_map(|(key, value)| {
            if *value != FAVORITE { return None; }
            let relative = decode_relative(key.strip_prefix(&prefix)?.strip_suffix("00")?)?;
            (relative.components().count() >= 1).then(|| volume.path.join(relative))
        }).collect()
    }

    fn show_favorites(&mut self) {
        let Some(volume) = self.volume().cloned() else { return };
        if let Some(identity) = self.identity(&volume) {
            if !self.history_loaded.contains(&history_prefix(identity)) {
                self.status = tr("Cargando favoritos…", "Carregant els favorits…").into();
                self.loading_favorites = true;
                return;
            }
        } else if self.facts.as_ref().is_none_or(|(disk, _)| disk != &volume) {
            self.status = tr("Cargando favoritos…", "Carregant els favorits…").into();
            self.loading_favorites = true;
            return;
        }
        let favorites = self.favorites(&volume);
        self.status = tr("Cargando favoritos…", "Carregant els favorits…").into();
        self.request(move || {
            let mut paths = Vec::new();
            let mut listings = BTreeMap::new();
            for path in favorites {
                let Some(parent) = path.parent() else { continue };
                // Walk from the mount through the ancestors. Only a successful listing proves absence.
                let mut ancestors: Vec<_> = parent.ancestors().take_while(|p| p.starts_with(&volume.path)).collect();
                ancestors.reverse();
                let mut proven = true;
                for (index, directory) in ancestors.iter().enumerate() {
                    let child = ancestors.get(index + 1).copied().unwrap_or(&path);
                    let required = if index + 1 == ancestors.len() && is_video_path(&path) { Kind::Video } else { Kind::Folder };
                    let entries = listings.entry(directory.to_path_buf()).or_insert_with(|| list_folder(directory, system_mount(&volume)));
                    match entries {
                        Ok(entries) if !entries.iter().any(|entry| entry.kind == required && Some(entry.name.as_os_str()) == child.file_name()) => {
                            proven = false; break;
                        }
                        Ok(_) => {}
                        Err(_) => { proven = false; break; }
                    }
                }
                if proven { paths.push(path); }
            }
            DiskResult::Favorites(volume, paths)
        });
        self.loading_favorites = self.pending.is_some();
    }

    fn media_cards(&self) -> Option<&FavoriteView> {
        self.media_view.as_ref().filter(|view| view.depth == self.folders.len())
    }

    fn documental_folder_picker(&self, view: &FavoriteView) -> bool {
        self.nav_tab == DOCUMENTALS_TAB && view.paths.len() > 1
    }

    fn leave_media(&mut self) {
        self.movie_filter = MovieFilter::All;
        self.quality_focus = None;
        self.quality_hover = None;
        self.quality_strip = widgets::TabStrip::new();
        self.movie_entries.clear();
        self.movie_count = 0;
        self.movie_pending = 0;
        if let Some(view) = self.media_view.take() {
            self.folders.truncate(view.depth);
            self.restore_view(view.library_selected, view.library_scroll);
        }
        self.nav_tab = 0;
    }

    fn enter_media(&mut self, volume: Volume, kind: usize, paths: Vec<PathBuf>) {
        self.leave_favorites();
        self.leave_media();
        self.nav_tab = kind;
        let selected = self.selected;
        let scroll = self.scroll.pos;
        self.media_view = Some(FavoriteView {
            volume, paths, depth: self.folders.len(), selected: 0, scroll: 0.0,
            library_selected: selected, library_scroll: scroll,
        });
        self.action = None;
        self.restore_view(0, 0.0);
        // A media tab opens the *contents* of the discovered category folder,
        // not another card representing PELIS or SERIES. Keep media_view as
        // the navigation floor so Back never exposes that container card.
        if let Some(view) = self.media_cards().filter(|view| !self.documental_folder_picker(view)) {
            if let Some(path) = view.paths.first() {
                self.open_folder(view.volume.clone(), path.clone());
            }
        }
    }

    fn open_nav_tab(&mut self, tab: usize) {
        if self.mutating { return; }
        if tab == MOVIES_TAB && self.movie_grid() {
            self.nav_focus = None;
            self.nav_hover = None;
            return;
        }
        self.quality_focus = None;
        self.quality_hover = None;
        if matches!(tab, 0 | 1 | 2 | DOCUMENTALS_TAB | FAVORITES_TAB) {
            // A new section supersedes reads from the previous section.
            self.retire_read();
        } else if self.pending.is_some() {
            return;
        }
        self.selection = None;
        if matches!(self.dialog, Some(Dialog::DeleteFolders)) { self.dialog = None; }
        let previous_focus = self.nav_hover.or(self.nav_focus);
        self.nav_focus = None;
        self.nav_hover = None;
        self.action = None;
        self.option = 0;
        match tab {
            0 => {
                let selected = self.volume().and_then(|volume|
                    self.scan.volumes.iter().position(|item| item == volume)).unwrap_or(self.selected);
                self.leave_media();
                self.leave_favorites();
                self.folders.clear();
                // External renames and NTFS remounts must be rediscovered.
                // Do not reuse a previously cached PELIS/SERIES path.
                self.discovery_cache.clear();
                self.invalidate_series_counts();
                self.refresh_home(false);
                self.next_scan = Instant::now();
                self.loading_favorites = false;
                self.nav_tab = 0;
                self.restore_view(selected, 0.0);
                self.status.clear();
            }
            1 | 2 | DOCUMENTALS_TAB => {
                let Some(volume) = self.volume().cloned() else {
                    self.leave_favorites();
                    self.leave_media();
                    self.nav_tab = tab;
                    self.status = tr("Selecciona primero un disco USB.", "Tria primer un disc USB.").into();
                    return;
                };
                // Always discover against the current directory tree. A
                // mount ID or USB serial can be reused after unplug/replug;
                // caching by either one retains paths renamed on Windows.
                self.discovery_cache.remove(&(volume.path.clone(), volume.mount_id.unwrap_or(0), tab));
                self.enter_media(volume.clone(), tab, Vec::new());
                self.status = tr("Buscando carpetas multimedia…", "Buscant carpetes multimèdia…").into();
                self.request(move || DiskResult::Discovered(volume.clone(), tab, discover_media_folders(&volume, tab)));
            }
            FAVORITES_TAB => {
                let volume = self.volume().cloned();
                if volume.is_none() {
                    self.leave_media();
                    self.nav_tab = FAVORITES_TAB;
                    self.status = tr("Selecciona primero un disco USB.", "Tria primer un disc USB.").into();
                    return;
                }
                self.leave_media();
                self.enter_favorites(volume.expect("connected favorites volume"), Vec::new());
                self.show_favorites();
            }
            SETTINGS_TAB => {
                self.settings_exit = None;
                self.settings_return_focus = previous_focus.or(Some(SETTINGS_TAB));
                self.settings_focus_y = Spring::at(unified_setting_rect(0).y);
                self.dialog = Some(Dialog::Settings);
                self.dialog_started = Some(Instant::now());
                self.status.clear();
            }
            EJECT_TAB => {
                if let Some(volume) = self.volume().cloned().filter(|volume| system_mount(volume).is_some()) {
                    self.dialog = Some(Dialog::Eject(volume));
                    self.status.clear();
                } else {
                    self.status = tr("Selecciona un disco que se pueda expulsar.", "Tria un disc que es pugui expulsar.").into();
                }
            }
            _ => {}
        }
    }

    fn favorite_cards(&self) -> Option<&FavoriteView> {
        self.favorite_view.as_ref().filter(|view| view.depth == self.folders.len())
    }

    fn favorite_highlight(&self) -> f32 {
        let (active, changed, from) = self.favorite_section;
        from + (f32::from(u8::from(active)) - from) * motion_alpha(changed)
    }

    fn set_favorite_section(&mut self, active: bool) {
        if self.favorite_section.0 != active {
            self.favorite_section = (active, Instant::now(), self.favorite_highlight());
        }
    }

    fn enter_favorites(&mut self, volume: Volume, paths: Vec<PathBuf>) {
        let previous = self.favorite_view.take();
        let (library_selected, library_scroll) = previous.as_ref()
            .map_or((self.selected, self.scroll.pos), |view| (view.library_selected, view.library_scroll));
        if let Some(view) = previous { self.folders.truncate(view.depth); }
        self.favorite_view = Some(FavoriteView {
            volume, paths, depth: self.folders.len(), selected: 0, scroll: 0.0,
            library_selected, library_scroll,
        });
        self.set_favorite_section(true);
        self.nav_tab = FAVORITES_TAB;
        self.action = None;
        self.restore_view(0, 0.0);
    }

    fn leave_favorites(&mut self) {
        self.loading_favorites = false;
        if let Some(view) = self.favorite_view.take() {
            self.folders.truncate(view.depth);
            self.restore_view(view.library_selected, view.library_scroll);
        }
        self.set_favorite_section(false);
        if self.nav_tab == FAVORITES_TAB { self.nav_tab = 0; }
    }

    fn restore_view(&mut self, selected: usize, scroll: f32) {
        self.scroll = Spring::at(scroll);
        self.select(selected, true);
    }

    // Resolve a season back to its series. Never mark the category folder itself.
    fn series_root(&self) -> Option<PathBuf> {
        let folder = self.folders.last()?;
        if folder.path == folder.volume.path || self.favorite_cards().is_some() { return None; }
        let relative = folder.path.strip_prefix(&folder.volume.path).ok()?;
        let parts: Vec<_> = relative.components().filter_map(|component| match component {
            std::path::Component::Normal(name) => Some(name),
            _ => None,
        }).collect();
        if let Some(index) = parts.iter().position(|name| media_folder_score(&name.to_string_lossy(), 2) > 0) {
            if parts[..index].iter().any(|name| media_folder_score(&name.to_string_lossy(), DOCUMENTALS_TAB) > 0) {
                return self.documentary_series_root(folder);
            }
            if parts.get(index + 1).is_some() {
                let mut root = folder.volume.path.clone();
                for part in &parts[..=index + 1] { root.push(part); }
                return Some(root);
            }
            return None;
        }
        if Self::category(&folder.volume, &folder.path, false) == Category::Documentary {
            return self.documentary_series_root(folder);
        }
        if folder.path.file_name().is_some_and(|name| season_folder(&name.to_string_lossy())) {
            return folder.path.parent().filter(|parent| *parent != folder.volume.path).map(std::path::Path::to_path_buf);
        }
        // Without a recognized category, require episode-like titles or season folders.
        folder.entries.iter().any(|entry| (entry.kind == Kind::Video && continuity::episode(&entry.name).is_some())
            || (entry.kind == Kind::Folder && season_folder(&entry.name.to_string_lossy())))
            .then(|| folder.path.clone())
    }

    fn documentary_series_root(&self, folder: &Folder) -> Option<PathBuf> {
        let category = |path: &std::path::Path| path.file_name()
            .is_some_and(|name| media_folder_score(&name.to_string_lossy(), DOCUMENTALS_TAB) > 0);
        let populated_show = folder.path.ancestors().take_while(|path| !category(path)).find(|path| {
            self.series_cache.get(*path).is_some_and(|entry| entry.summary.is_some())
        });
        if let Some(show) = populated_show { return Some(show.to_path_buf()); }
        if category(&folder.path) { return None; }
        if folder.path.file_name().is_some_and(|name| season_folder(&name.to_string_lossy())) {
            return folder.path.parent().filter(|path| !category(path)).map(std::path::Path::to_path_buf);
        }
        folder.entries.iter().any(|entry| {
            (entry.kind == Kind::Video && continuity::episode(&entry.name).is_some())
                || (entry.kind == Kind::Folder && season_folder(&entry.name.to_string_lossy()))
        }).then(|| folder.path.clone())
    }

    /// A series favorite belongs to the series folder, not to an individual
    /// season or episode, even when the series contains no files yet.
    fn series_favorite_key(&self) -> Option<(PathBuf, String)> {
        let folder = self.folders.last()?;
        let root = self.series_root()?;
        if root != folder.path { return None; }
        let identity = self.identity(&folder.volume)?;
        if !self.history_loaded.contains(&history_prefix(identity)) { return None; }
        let key = favorite_key(identity, &folder.volume, &root)?;
        Some((root, key))
    }

    fn toggle_series_favorite(&mut self) {
        let Some((root, key)) = self.series_favorite_key() else { return; };
        let was_favorite = self.history_value(&key) == Some(FAVORITE);
        self.change_history(key, if was_favorite { 0 } else { FAVORITE });
        if let Some(view) = &mut self.favorite_view {
            if was_favorite { view.paths.retain(|path| *path != root); }
            else if !view.paths.contains(&root) {
                view.paths.push(root.clone());
                view.paths.sort();
            }
        }
        self.favorite_motion = Some((root, !was_favorite, Instant::now()));
        self.status = if was_favorite {
            tr("Serie quitada de favoritos.", "Sèrie treta dels favorits.")
        } else {
            tr("Serie añadida a favoritos.", "Sèrie afegida als favorits.")
        }.into();
        plx_machine::idle::invalidate();
    }

    fn is_library_movie(&self, volume: &Volume, path: &std::path::Path) -> bool {
        self.movie_grid() || path.strip_prefix(&volume.path).is_ok_and(|relative| {
            relative.parent().is_some_and(|parent| parent.iter()
                .any(|name| media_folder_score(&name.to_string_lossy(), MOVIES_TAB) > 0))
        })
    }

    fn video_is_episode(&self, volume: &Volume, path: &std::path::Path) -> bool {
        // Keep episode menus episode-only even when an old standalone favorite
        // is opened from the Favorites tab without a parent folder on the stack.
        !self.is_library_movie(volume, path) && (self.episode_show(volume, path).is_some()
            || home::episode_numbers(path.strip_prefix(&volume.path).unwrap_or(path), false).is_some())
    }

    fn video_favorite_target(&self, volume: &Volume, path: &std::path::Path) -> PathBuf {
        // Opening a series episode must never create an episode-level favorite.
        self.folders.last().filter(|folder| &folder.volume == volume)
            .and_then(|_| self.series_root())
            .filter(|root| path.starts_with(root))
            .unwrap_or_else(|| path.to_path_buf())
    }

    fn video_favorite_key(&self, volume: &Volume, path: &std::path::Path) -> Option<String> {
        let identity = self.identity(volume)?;
        self.history_loaded.contains(&history_prefix(identity)).then_some(())?;
        favorite_key(identity, volume, &self.video_favorite_target(volume, path))
    }

    fn detail_key(folder: &Folder, entry: &Entry) -> DetailKey {
        (folder.path.join(&entry.name), entry.size, entry.modified)
    }

    fn index_inventory_paths(&mut self) {
        self.inventory_paths.clear();
        self.home_recent = home::recent(&self.dashboard.drives);
        for (drive_index, drive) in self.dashboard.drives.iter().enumerate() {
            for (video_index, video) in drive.videos.iter().enumerate() {
                self.inventory_paths.insert(drive.volume.path.join(&video.path), (drive_index, video_index));
            }
        }
    }

    fn inventory_video(&self, path: &std::path::Path) -> Option<(&Volume, &home::Video)> {
        let &(drive, video) = self.inventory_paths.get(path)?;
        let drive = &self.dashboard.drives[drive];
        Some((&drive.volume, &drive.videos[video]))
    }

    fn detail_key_for(&self, path: &std::path::Path) -> DetailKey {
        self.folders.last().filter(|folder| path.parent() == Some(folder.path.as_path()))
            .and_then(|folder| folder.entries.iter()
            .find(|entry| path.file_name() == Some(entry.name.as_os_str()))
            .map(|entry| Self::detail_key(folder, entry)))
            .or_else(|| self.dashboard.items.iter().find(|item| item.path == path).map(|item| item.detail_key.clone()))
            .or_else(|| self.inventory_video(path).map(|(_, video)| {
                (path.to_path_buf(), Some(video.size), Some(video.modified))
            }))
            .unwrap_or_else(|| (path.to_path_buf(), None, None))
    }

    fn card_file(&self, index: usize) -> Option<(&Volume, DetailKey)> {
        if self.drive_picker() {
            if self.home_rect(index).w == 0.0 || index == self.home_more_index() { return None; }
            let position = index.checked_sub(self.scan.volumes.len())?;
            let item = self.dashboard.items.get(position)
                .or_else(|| self.home_recent.get(position.checked_sub(self.dashboard.items.len())?).map(|recent| &recent.item))?;
            return Some((&item.volume, item.detail_key.clone()));
        }
        if let Some(view) = self.favorite_cards() {
            let path = view.paths.get(index)?;
            return is_video_path(path).then(|| (&view.volume, self.detail_key_for(path)));
        }
        let folder = self.folders.last().filter(|_| self.media_cards().is_none())?;
        let entry = self.folder_entry(index)?;
        (entry.kind == Kind::Video).then(|| (&folder.volume, Self::detail_key(folder, entry)))
    }

    fn update_video_details(&mut self) -> bool {
        if self.demo.is_some() { return false; }
        let mut changed = false;
        if let Some(job) = &self.details_job {
            match job.reply.try_recv() {
                Err(TryRecvError::Empty) => return false,
                result => {
                    if self.scan.volumes.contains(&job.volume) {
                        self.accept_video_details(job.key.clone(), result.unwrap_or_default());
                    }
                    self.details_job = None;
                    changed = true;
                }
            }
        }
        if self.mutating { return changed; }
        let target = match &self.dialog {
            Some(Dialog::Video { volume, path }) => {
                let key = self.detail_key_for(path);
                (!self.details.contains_key(&key)).then(|| (volume.clone(), key))
            }
            None if self.pending.is_none() => {
                std::iter::once(self.selected).chain(self.visible_cards())
                    .filter_map(|index| self.card_file(index))
                    .find(|(_, key)| !self.details.contains_key(key))
                    .or_else(|| {
                    let folder = self.folders.last().filter(|_| self.movie_grid())?;
                    // A quality-filtered grid hides unprobed videos. Keep discovering them
                    // from the same directory listing and the same single metadata worker.
                    folder.entries.iter().filter(|entry| entry.kind == Kind::Video)
                        .map(|entry| Self::detail_key(folder, entry))
                        .find(|key| !self.details.contains_key(key))
                        .map(|key| (&folder.volume, key))
                }).map(|(volume, key)| (volume.clone(), key)).or_else(|| self.series_probe())
            }
            _ => None,
        };
        let Some((volume, key)) = target else { return changed };
        let mount = system_mount(&volume);
        let identity = self.identity(&volume).map(str::to_owned);
        let source = volume.clone();
        let detail = key.clone();
        let (send, reply) = mpsc::sync_channel(1);
        if plx_base::task::spawn_off_frame("cineusb-video-details", move |_| {
            let cache = if detail.1.is_some() && detail.2.is_some() {
                identity.or_else(|| volume_facts(&source).1)
                    .and_then(|identity| details_cache::Key::new(&identity, &source, &detail))
            } else {
                None
            };
            let details = details_cache::probe(cache.as_ref(), || open_for_details(&detail.0, mount));
            let _ = send.send(details);
        }) {
            self.details_job = Some(DetailsRequest { volume, key, reply });
        } else {
            self.accept_video_details(key, Default::default());
        }
        changed
    }

    fn series_probe(&mut self) -> Option<(Volume, DetailKey)> {
        for index in self.visible_cards() {
            let Some(card) = self.series_card(index) else { continue; };
            let (volume, path) = (card.volume.clone(), card.path);
            if let Some(summary) = self.series_cache.get_mut(&path).and_then(|entry| entry.summary.as_mut()) {
                if let Some(key) = summary.next_probe(&self.details) {
                    return Some((volume, key.clone()));
                }
            }
        }
        None
    }

    fn series_card(&self, index: usize) -> Option<SeriesCard<'_>> {
        let card = if let Some(view) = self.favorite_cards() {
            let path = view.paths.get(index)?;
            if is_video_path(path) { return None; }
            SeriesCard { volume: &view.volume, path: path.clone(), modified: None, known: true }
        } else {
            if !matches!(self.nav_tab, 0 | 2 | DOCUMENTALS_TAB | FAVORITES_TAB) || self.media_cards().is_some() { return None; }
            let folder = self.folders.last()?;
            let entry = folder.entries.get(index)?;
            if entry.kind != Kind::Folder { return None; }
            let parent_is_category = (self.nav_tab == SERIES_TAB && self.media_view.as_ref().and_then(|view| view.paths.first()) == Some(&folder.path))
                || folder.path.file_name().is_some_and(|name| media_folder_score(&name.to_string_lossy(), 2) == 3);
            SeriesCard { volume: &folder.volume, path: folder.path.join(&entry.name),
                modified: entry.modified, known: parent_is_category }
        };
        let name = card.path.file_name()?.to_string_lossy();
        if art::is_season(&name.to_lowercase()) || [2, DOCUMENTALS_TAB].into_iter()
            .any(|category| !card.known && media_folder_score(&name, category) == 3) {
            return None;
        }
        Some(card)
    }

    fn invalidate_series_counts(&mut self) {
        self.series_cache.clear();
        if let Some(job) = &mut self.series_job {
            job.invalidated = true;
            job.cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn update_series_counts(&mut self, now: Instant) -> bool {
        let mut changed = false;
        if let Some(job) = &self.series_job {
            let listing = match job.reply.try_recv() {
                Ok(listing) => listing,
                Err(TryRecvError::Disconnected) => None,
                Err(TryRecvError::Empty) => return false,
            };
            let job = self.series_job.take().expect("series request just polled");
            if !job.invalidated && self.scan.volumes.contains(&job.volume) {
                if self.series_cache.len() >= 512 && !self.series_cache.contains_key(&job.path) {
                    let oldest = self.series_cache.iter().min_by_key(|(_, entry)| entry.checked)
                        .map(|(path, _)| path.clone());
                    if let Some(path) = oldest { self.series_cache.remove(&path); }
                }
                let mut summary = listing.map(series::Summary::new);
                if let Some(summary) = &mut summary {
                    summary.next_probe(&self.details);
                    summary.watched = self.watched_episodes(&job.volume, &summary.listing);
                }
                self.series_cache.insert(job.path, SeriesCacheEntry {
                    checked: now, modified: job.modified, known: job.known, summary,
                });
                changed = true;
            }
        }
        if self.mutating || self.dialog.is_some() || self.pending.is_some() { return changed; }
        let target = self.visible_cards()
            .filter_map(|index| self.series_card(index))
            .find(|card| !self.series_cache.get(&card.path).is_some_and(|entry| entry.fresh(card, now)));
        let Some(card) = target else { return changed };
        let SeriesCard { volume, path, modified, known } = card;
        let volume = volume.clone();
        let folder = path.clone();
        let mount = system_mount(&volume);
        let (send, reply) = mpsc::sync_channel(1);
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        if plx_base::task::spawn_off_frame("cineusb-series-counts", move |_| {
            let _ = send.send(series::count_cancellable(&folder, mount, known, &worker_cancelled).ok().flatten());
        }) {
            self.series_job = Some(SeriesRequest { volume, path, modified, known, invalidated: false, cancelled, reply });
        }
        changed
    }

    fn home_item(&self) -> Option<&home::Item> {
        if !self.drive_picker() { return None; }
        self.dashboard.items.get(self.selected.checked_sub(self.scan.volumes.len())?)
    }

    fn library_scanning(&self) -> bool {
        self.scanning.is_some() || self.facts_job.is_some() || self.details_job.is_some()
            || self.series_job.as_ref().is_some_and(|job| !job.invalidated)
            || self.home_job.as_ref().is_some_and(|job| !job.cancelled.load(std::sync::atomic::Ordering::Relaxed))
            || self.manual_refresh.is_some()
    }

    fn request_manual_refresh(&mut self) {
        if self.mutating || self.manual_refresh.is_some() { return; }
        let Some(volume) = self.volume().cloned().or_else(|| self.scan.volumes.first().cloned()) else {
            self.refresh_notice = Some((tr("Conecta un disco USB.", "Connecta un disc USB.").into(), Instant::now()));
            return;
        };
        self.manual_refresh = Some(volume);
        self.refresh_notice = None;
        self.refresh_home(true);
    }

    fn refresh_home(&mut self, force: bool) {
        self.home_force_refresh |= force;
        if force || self.dashboard.drives.is_empty() {
            self.home_refresh = Instant::now();
            if let Some(job) = &self.home_job {
                job.cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    fn update_home(&mut self, now: Instant) -> bool {
        let mut changed = false;
        if self.manual_refresh.as_ref().is_some_and(|volume| !self.scan.volumes.contains(volume)) {
            self.manual_refresh = None;
            changed = true;
            self.refresh_notice = Some((tr("Actualización interrumpida: USB desconectado.", "Actualització interrompuda: USB desconnectat.").into(), now));
        }
        let had_missing_drive = self.dashboard.drives.iter().any(|drive| !self.scan.volumes.contains(&drive.volume));
        let previous_focus = had_missing_drive.then(|| self.home_focus()).flatten();
        self.dashboard.drives.retain(|drive| self.scan.volumes.contains(&drive.volume));
        self.dashboard.items.retain(|item| self.scan.volumes.contains(&item.volume));
        if had_missing_drive {
            self.index_inventory_paths();
            self.restore_home_selection(previous_focus);
            self.home_refresh = now;
            changed = true;
        }
        if let Some(job) = &self.home_job {
            if job.volumes != self.scan.volumes {
                job.cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            let incoming: Vec<_> = if job.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                Vec::new()
            } else {
                job.preview.iter().flat_map(|reply| reply.try_iter()).flatten().collect()
            };
            let result = job.reply.try_recv();
            if !incoming.is_empty() {
                let selected_path = self.home_focus();
                for item in incoming {
                    if let Some(existing) = self.dashboard.items.iter_mut().find(|existing|
                        existing.volume == item.volume && existing.path == item.path) {
                        *existing = item;
                    } else {
                        self.dashboard.items.push(item);
                    }
                }
                self.dashboard.items.sort_by_key(|item| !item.favorite);
                self.dashboard.items.truncate(12);
                self.restore_home_selection(selected_path);
                changed = true;
            }
            match result {
                Err(TryRecvError::Empty) => return changed,
                result => {
                    let job = self.home_job.take().expect("home request polled");
                    if job.manual { self.manual_refresh = None; changed = true; }
                    if !job.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                        if let Ok(Ok(mut dashboard)) = result {
                            let selected_path = self.home_focus();
                            for drive in &dashboard.drives {
                                if let Some((identity, records)) = drive.facts.1.as_ref().zip(drive.history.as_ref()) {
                                    let prefix = history_prefix(identity);
                                    let current = self.history.entry(prefix.clone()).or_default();
                                    for (key, value) in records { current.entry(key.clone()).or_insert(*value); }
                                    self.history_loaded.insert(prefix);
                                }
                            }
                            if let Some((volume, path, mut entries)) = dashboard.refreshed_folder.take() {
                                disk::sort_entries_in(&mut entries, self.settings.sort, path.strip_prefix(&volume.path).unwrap_or(&path));
                                let selected = self.folder_entry(self.selected).map(|entry| entry.name.clone());
                                if let Some(folder) = self.folders.last_mut().filter(|folder| folder.volume == volume && folder.path == path) {
                                    folder.entries = entries;
                                    self.rebuild_movie_grid();
                                    let index = self.visible_index_of(selected.as_deref()).unwrap_or(self.selected);
                                    self.selected = index.min(self.len().saturating_sub(1));
                                }
                            }
                            if job.manual {
                                let refreshed: Vec<_> = dashboard.drives.iter().map(|drive| drive.volume.clone()).collect();
                                self.dashboard.drives.retain(|drive| !refreshed.contains(&drive.volume));
                                self.dashboard.items.retain(|item| !refreshed.contains(&item.volume));
                                self.dashboard.drives.append(&mut dashboard.drives);
                                self.dashboard.items.append(&mut dashboard.items);
                                self.dashboard.items.sort_by_key(|item| !item.favorite);
                                self.dashboard.items.truncate(12);
                                self.discovery_cache.clear();
                            } else { self.dashboard = dashboard; }
                            self.index_inventory_paths();
                            self.refresh_series_watched(None);
                            if job.manual {
                                self.invalidate_series_counts();
                                self.details.clear();
                                self.refresh_movie_grid();
                                if self.favorite_cards().is_some() && self.pending.is_none() { self.show_favorites(); }
                                self.refresh_notice = Some((tr("Biblioteca actualizada", "Biblioteca actualitzada").into(), now));
                            }
                            self.restore_home_selection(selected_path);
                            changed = true;
                        }
                        if job.manual && self.refresh_notice.is_none() {
                            self.refresh_notice = Some((tr("No se pudo actualizar la biblioteca.", "No s’ha pogut actualitzar la biblioteca.").into(), now));
                        }
                        self.home_refresh = now + Duration::from_secs(60);
                    } else if job.manual && !self.mutating && self.refresh_notice.is_none() {
                        self.refresh_notice = Some((tr("Actualización interrumpida. Vuelve a intentarlo.", "Actualització interrompuda. Torna-ho a provar.").into(), now));
                    }
                }
            }
        }
        if self.mutating || self.pending.is_some() || self.progress_job.is_some()
            || now < self.home_refresh || self.scan.volumes.is_empty() { return changed; }
        let volumes = self.scan.volumes.clone();
        let worker_volumes = self.manual_refresh.as_ref().map(|volume| vec![volume.clone()]).unwrap_or_else(|| volumes.clone());
        let refresh = self.home_force_refresh;
        let manual = self.manual_refresh.is_some();
        let folder = if manual { self.folders.last().map(|folder| (folder.volume.clone(), folder.path.clone())) } else { None };
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = cancelled.clone();
        let (send, reply) = mpsc::sync_channel(1);
        let (preview_send, preview_reply) = if !manual && (refresh || self.dashboard.items.is_empty()) {
            let (send, reply) = mpsc::channel();
            (Some(send), Some(reply))
        } else { (None, None) };
        let demo_history = self.demo.as_ref().map(|_| self.history.get(&history_prefix(demo::IDENTITY)).cloned().unwrap_or_default());
        if plx_base::task::spawn_off_frame("cineusb-home-library", move |_| {
            let preview = preview_send.map(|send| move |items| { let _ = send.send(items); });
            let result = match demo_history {
                Some(history) => home::load_demo(worker_volumes, history, &stop),
                None => home::load(worker_volumes, refresh, &stop, preview),
            }.and_then(|mut dashboard| {
                if let Some((volume, path)) = folder {
                    let entries = match list_folder(&path, system_mount(&volume)) {
                        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
                        result => result?,
                    };
                    dashboard.refreshed_folder = Some((volume, path, entries));
                }
                Ok(dashboard)
            });
            let _ = send.send(result);
        }) {
            self.home_job = Some(HomeRequest { preview: preview_reply, reply, cancelled, volumes, manual });
            self.home_force_refresh = false;
        } else {
            self.home_refresh = now + Duration::from_secs(10);
        }
        changed
    }

    fn update_settings(&mut self) -> bool {
        if !self.settings_started {
            self.settings_started = true;
            self.settings.language = if i18n::current().language() == Language::Ca { Preference::Ca } else { Preference::Es };
            let (reply, receive) = mpsc::sync_channel(1);
            if plx_base::task::spawn_off_frame("cineusb-settings", move |_| { let _ = reply.send(load_settings()); }) {
                self.settings_job = Some(SettingsJob::Load(receive));
            }
        }
        let (saving, result) = match self.settings_job.as_ref() {
            Some(SettingsJob::Load(reply)) => (false, reply.try_recv()),
            Some(SettingsJob::Save(reply)) => (true, reply.try_recv().map(|result| result.map(Some))),
            None => return false,
        };
        let result = match result {
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => Err("settings worker disconnected".into()),
            Ok(result) => result,
        };
        self.settings_job = None;
        match result {
            Ok(Some(settings)) => {
                let selected = self.folder_entry(self.selected).map(|entry| entry.name.clone());
                let sort_changed = self.settings.sort != settings.sort;
                let size_changed = self.settings.cards != settings.cards;
                self.settings = settings;
                if sort_changed {
                    let last = self.folders.len().saturating_sub(1);
                    for (index, folder) in self.folders.iter_mut().enumerate() {
                        let old = if index == last { self.selected } else { folder.selected };
                        folder.selected = disk::resort(&mut folder.entries, old, settings.sort,
                            folder.path.strip_prefix(&folder.volume.path).unwrap_or(&folder.path));
                        if index == last { self.selected = folder.selected; }
                    }
                }
                if sort_changed && self.movie_grid() {
                    self.rebuild_movie_grid();
                    self.selected = self.visible_index_of(selected.as_deref()).unwrap_or(self.selected);
                }
                if size_changed || sort_changed { self.select(self.selected, true); }
                i18n::apply_usb_language(settings.language);
                if saving {
                    // The unified screen keeps its current focus when a value is saved.
                    // Do not resurrect obsolete per-setting dialogs or reset the selection.
                    if matches!(self.dialog, Some(Dialog::Accent | Dialog::Cards | Dialog::Extensions)) {
                        self.dialog = Some(Dialog::Appearance);
                    }
                    self.status.clear();
                }
            }
            Ok(None) => {}
            Err(error) => {
                plx_base::eventlog::log(&format!("cineusb: USB preferences failed: {error}"));
                if saving {
                    self.status = tr("No se pudieron guardar los ajustes.", "No s’han pogut desar els ajustos.").into();
                }
            }
        }
        true
    }

    fn change_settings(&mut self, settings: Settings) {
        let (reply, receive) = mpsc::sync_channel(1);
        if self.demo.is_some() {
            let _ = reply.send(Ok(settings));
            self.settings_job = Some(SettingsJob::Save(receive));
            return;
        }
        if plx_base::task::spawn_off_frame("cineusb-settings", move |_| {
            let _ = reply.send(save_settings(settings).map(|()| settings));
        }) {
            self.settings_job = Some(SettingsJob::Save(receive));
        } else {
            self.status = tr("No se pudieron guardar los ajustes.", "No s’han pogut desar els ajustos.").into();
        }
    }

    fn volume(&self) -> Option<&Volume> {
        if let Some(item) = self.home_item() { return Some(&item.volume); }
        if self.drive_picker() && !self.home_all {
            if let Some(recent) = self.selected.checked_sub(self.home_recent_offset()).and_then(|index| self.home_recent.get(index)) {
                return Some(&recent.item.volume);
            }
        }
        self.media_cards().map(|view| &view.volume)
            .or_else(|| self.favorite_cards().map(|view| &view.volume))
            .or_else(|| self.folders.last().map(|folder| &folder.volume))
            .or_else(|| self.scan.volumes.get(self.selected))
    }

    fn selectable_delete_kind(&self) -> Option<Kind> {
        match self.nav_tab {
            SERIES_TAB => Some(Kind::Folder),
            MOVIES_TAB if self.movie_grid() => Some(Kind::Video),
            DOCUMENTALS_TAB if self.media_cards().is_none() && !self.folders.is_empty() => Some(Kind::Video),
            _ => None,
        }
    }

    fn action_available(&self, action: Action) -> bool {
        match action {
            Action::ToggleFavorite => self.selection.is_none() && self.series_favorite_key().is_some(),
            Action::Select => self.selectable_delete_kind().is_some_and(|kind| {
                self.selection.is_some() || self.folders.last().is_some_and(|folder| {
                    folder.path != folder.volume.path && (self.demo.is_some() || system_mount(&folder.volume).is_some())
                        && folder.entries.iter().any(|entry| entry.kind == kind)
                })
            }),
            Action::DeleteFolders => self.selectable_delete_kind().is_some()
                && self.selection.as_ref().is_some_and(|marked| !marked.is_empty()),
        }
    }

    fn open_action(&mut self, action: Action) {
        if self.pending.is_some() || !self.action_available(action) { return; }
        self.option = 0;
        self.status.clear();
        self.loading_favorites = false;
        self.dialog = match action {
            Action::ToggleFavorite => { self.toggle_series_favorite(); None },
            Action::Select => {
                self.selection = self.selection.is_none().then(BTreeSet::new);
                self.action = None;
                None
            }
            Action::DeleteFolders => Some(Dialog::DeleteFolders),
        };
    }

    fn toggle_demo(&mut self) {
        if let Some(real) = self.real_browser.take() {
            let mut demo_browser = std::mem::replace(self, *real);
            if let Some(job) = &demo_browser.home_job {
                job.cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(library) = demo_browser.demo.take() {
                plx_base::task::spawn_off_frame("cineusb-demo-cleanup", move |_| drop(library));
            }
            self.retire_scan();
            self.next_scan = Instant::now();
            self.status.clear();
            i18n::apply_usb_language(self.settings.language);
        } else if self.pending.is_none() {
            self.status = tr("Preparando el modo demo…", "Preparant el mode demo…").into();
            self.request(|| DiskResult::Demo(demo::Library::create()));
        }
        plx_machine::idle::invalidate();
    }

    fn enter_demo(&mut self, mut library: demo::Library) {
        let mut browser = Self::new();
        browser.settings = self.settings;
        browser.settings_started = true;
        browser.hour = self.hour;
        browser.tv_clock = self.tv_clock;
        browser.scan.volumes.push(library.volume.clone());
        browser.facts = Some((library.volume.clone(), demo::facts()));
        let prefix = history_prefix(demo::IDENTITY);
        browser.history.insert(prefix.clone(), std::mem::take(&mut library.history));
        browser.history_loaded.insert(prefix);
        browser.demo = Some(library);
        self.dialog = None;
        self.dialog_started = None;
        self.nav_focus = Some(SETTINGS_TAB);
        self.status.clear();
        let real = std::mem::replace(self, browser);
        self.real_browser = Some(Box::new(real));
    }

    fn cycle_unified_setting(&mut self, direction: i32) {
        if self.settings_job.is_some() { return; }
        if self.option == DEMO_SETTING {
            self.toggle_demo();
            return;
        }
        if self.option == 8 {
            self.request_manual_refresh();
            return;
        }
        let mut next = self.settings;
        let step = |value: usize, count: usize| (value as i32 + direction).rem_euclid(count as i32) as usize;
        match self.option {
            0 => next.sort = step(next.sort, 3),
            1 => next.extensions = !next.extensions,
            2 => next.auto_next = !next.auto_next,
            3 => next.forced_subtitles = !next.forced_subtitles,
            4 => next.spanish_audio = !next.spanish_audio,
            5 => next.language = match next.language {
                Preference::Ca => Preference::En, Preference::En => Preference::Es, _ => Preference::Ca,
            },
            6 => next.accent = step(next.accent, 3),
            7 => next.cards = step(next.cards, 3),
            9 => next.show_clock = !next.show_clock,
            10 => next.clock_format = step(next.clock_format, 3),
            11 => next.clock_size = step(next.clock_size, 3),
            12 => next.convert_dts = !next.convert_dts,
            _ => return,
        }
        self.change_settings(next);
    }

    fn choose_option(&mut self) {
        if self.settings_job.is_some() { return; }
        if matches!(self.dialog, Some(Dialog::Settings)) {
            self.cycle_unified_setting(1);
            return;
        }
        self.dialog_started = None;
        match self.dialog.take() {
            Some(Dialog::Appearance) => {
                (self.dialog, self.option) = match self.option {
                    0 => (Some(Dialog::Accent), self.settings.accent),
                    1 => (Some(Dialog::Cards), self.settings.cards),
                    _ => (Some(Dialog::Extensions), usize::from(self.settings.extensions)),
                };
            }
            Some(dialog @ (Dialog::Accent | Dialog::Cards | Dialog::Extensions)) => {
                let mut settings = self.settings;
                match dialog {
                    Dialog::Accent => settings.accent = self.option,
                    Dialog::Cards => settings.cards = self.option,
                    Dialog::Extensions => settings.extensions = self.option == 1,
                    _ => unreachable!(),
                }
                self.dialog = Some(dialog);
                self.change_settings(settings);
            }
            Some(Dialog::Resume(_)) => {
                if let Some(mut media) = self.prepared.take() {
                    if self.option == 1 { media.resume = 0; }
                    self.ready = Some(media);
                }
            }
            Some(Dialog::Next { volume, path, .. }) if self.option == 0 && self.scan.volumes.contains(&volume) => {
                if let Some(index) = self.visible_index_of(path.file_name()) {
                    self.select(index, true);
                }
                self.play_video_at(&volume, path, false);
            }
            Some(Dialog::DeleteFolders) if self.option == 1 => self.delete_marked_folders(),
            Some(Dialog::Video { volume, path }) => {
                if !self.scan.volumes.contains(&volume) { return; }
                if self.option == 0 {
                    self.play_video(&volume, path);
                } else if self.option == 2 {
                    if let Some(key) = self.video_history_key(&volume, &path) {
                        let watched = self.history_value(&key) == Some(WATCHED);
                        self.change_history(key, if watched { 0 } else { WATCHED });
                    }
                } else if self.option == 3 && !self.video_is_episode(&volume, &path) {
                    if let Some(key) = self.video_favorite_key(&volume, &path) {
                        let target = self.video_favorite_target(&volume, &path);
                        let series = target != path;
                        let marked = self.history_value(&key) == Some(FAVORITE);
                        self.change_history(key, if marked { 0 } else { FAVORITE });
                        self.favorite_motion = Some((target.clone(), !marked, Instant::now()));
                        self.status = match (series, marked) {
                            (true, true) => tr("Serie quitada de favoritos.", "Sèrie treta dels favorits."),
                            (true, false) => tr("Serie añadida a favoritos.", "Sèrie afegida als favorits."),
                            (false, true) => tr("Película quitada de favoritos.", "Pel·lícula treta dels favorits."),
                            (false, false) => tr("Película añadida a favoritos.", "Pel·lícula afegida als favorits."),
                        }.into();
                        if marked {
                            if let Some(view) = self.favorite_view.as_mut() { view.paths.retain(|item| item != &target); }
                            self.select(self.selected, true);
                        }
                    }
                } else if self.option == 1 && (self.demo.is_some() || system_mount(&volume).is_some()) {
                    self.dialog = Some(Dialog::Delete { volume, path, finished_episode: false });
                    self.option = 0;
                }
            }
            Some(Dialog::Delete { volume, path, .. }) if self.option == 1 => {
                if !self.scan.volumes.contains(&volume) { return; }
                self.status = tr("Eliminando archivo…", "Esborrant el fitxer…").into();
                let demo = self.demo.as_ref().map(|library| library.volume.path.clone());
                self.request(move || {
                    let result = match demo {
                        Some(root) => demo::delete(&root, &path, false),
                        None => mutate_usb(Mutation::Delete, &volume, &path),
                    };
                    DiskResult::Delete(path, result)
                });
                self.mutating = self.pending.is_some();
            }
            Some(Dialog::Delete { volume, path, finished_episode: false }) => {
                self.dialog = Some(Dialog::Video { volume, path });
                self.option = 0;
            }
            Some(Dialog::Eject(volume)) if self.option == 1 => {
                if !self.scan.volumes.contains(&volume) { return; }
                self.status = tr("Expulsando disco…", "Expulsant el disc…").into();
                self.manual_refresh = None;
                self.ejecting = Some((volume.clone(), Instant::now()));
                self.eject_feedback = None;
                self.eject_uncertain = false;
                // Release any file descriptors retained by the browser before
                // requesting a clean NTFS unmount. An active player may still
                // keep the disk busy, which must be reported as a failure.
                self.prepared = None;
                self.ready = None;
                self.details.clear();
                self.refresh_movie_grid();
                // Drain in-flight workers before ejecting. Dropping their reply
                // channels does not close descriptors held by those workers.
                self.invalidate_series_counts();
                if let Some(job) = &self.home_job {
                    job.cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                self.mutating = true;
            }
            _ => {}
        }
    }

    fn delete_marked_folders(&mut self) {
        let Some(folder) = self.folders.last() else { return };
        if folder.path == folder.volume.path { return; }
        let Some(kind) = self.selectable_delete_kind() else { return };
        let volume = folder.volume.clone();
        let mount = system_mount(&volume);
        let demo = self.demo.as_ref().map(|library| library.volume.path.clone());
        if mount.is_none() && demo.is_none() { return; }
        let Some(mut marked) = self.selection.take() else { return };
        marked.retain(|name| folder.entries.iter().any(|entry| &entry.name == name && entry.kind == kind));
        if marked.is_empty() { return; }
        let parent = folder.path.clone();
        let (progress, updates) = mpsc::channel();
        let total = marked.len();
        self.status = format!("{} 0/{total}", tr("Elementos procesados", "Elements processats"));
        self.request(move || {
            let mut removed = Vec::new();
            let mut error = None;
            for (index, name) in marked.into_iter().enumerate() {
                let path = parent.join(&name);
                let result = if let Some(root) = &demo { demo::delete(root, &path, kind == Kind::Folder) }
                    else if kind == Kind::Folder { delete_folder(&path, mount.expect("live USB mount")) }
                    else { mutate_usb(Mutation::Delete, &volume, &path) };
                match result {
                    Ok(()) => removed.push(name),
                    Err(detail) => { error.get_or_insert(detail); }
                }
                let _ = progress.send((index + 1, total));
            }
            let entries = list_folder(&parent, mount);
            DiskResult::DeleteFolders { removed, total, error, entries }
        });
        self.mutating = self.pending.is_some();
        if self.mutating { self.progress = Some(updates); }
    }

    fn finish_mutation(&mut self, result: Result<(), String>, success: &str, failure: &str) {
        self.invalidate_series_counts();
        self.refresh_home(true);
        self.retire_scan();
        self.next_scan = Instant::now();
        self.status = match result {
            Ok(()) => success.into(),
            Err(error) => {
                plx_base::eventlog::log(&format!("cineusb: USB action failed: {error}"));
                format!("{failure} · {error}")
            }
        };
    }

    fn select(&mut self, index: usize, snap: bool) {
        self.selected = index.min(self.len().saturating_sub(1));
        if self.drive_picker() {
            let drives = self.scan.volumes.len();
            if self.selected < drives {
                self.home_disk_start = self.home_disk_start.clamp(self.selected.saturating_sub(1), self.selected);
            } else if self.selected < self.home_recent_offset() {
                let position = self.selected - drives;
                self.home_resume_start = if self.home_all { position / 9 * 9 }
                    else { self.home_resume_start.clamp(position.saturating_sub(2), position) };
            }
        }
        if snap {
            if self.drive_picker() {
                self.home_resume_scroll = Spring::at(self.home_resume_start.min(self.dashboard.items.len().saturating_sub(3)) as f32 * HOME_CARD_PITCH);
            }
            self.scroll = Spring::at(self.scroll_target());
            self.pop.reset();
        }
        if self.len() > 0 {
            self.pop.arm(Card { cell: self.selected, key: self.selected as u64 });
        }
    }

    fn open(&mut self) {
        if self.pending.is_some() { return; }
        if let Some(item) = self.home_item().cloned() {
            self.play_video_at(&item.volume, item.path, true);
            return;
        }
        if self.drive_picker() && self.selected == self.home_more_index() && self.dashboard.items.len() > 3 {
            self.home_all = !self.home_all;
            self.home_resume_start = 0;
            self.select(self.scan.volumes.len(), true);
            return;
        }
        if self.drive_picker() && !self.home_all {
            if let Some(recent) = self.selected.checked_sub(self.home_recent_offset()).and_then(|index| self.home_recent.get(index)) {
                let volume = recent.item.volume.clone();
                if let Some(folder) = recent.folder.clone() {
                    self.open_folder(volume, folder);
                } else {
                    self.dialog = Some(Dialog::Video { volume, path: recent.item.path.clone() });
                    self.option = 0;
                }
                return;
            }
        }
        if self.tab_waiting_for_volume() {
            self.open_nav_tab(self.nav_tab);
            return;
        }
        if let Some(view) = self.media_cards() {
            if self.documental_folder_picker(view) {
                if let Some(path) = view.paths.get(self.selected) {
                    self.open_folder(view.volume.clone(), path.clone());
                }
            } else {
                self.open_nav_tab(self.nav_tab);
            }
            return;
        }
        if let Some(view) = self.favorite_cards() {
            if let Some(path) = view.paths.get(self.selected) {
                if is_video_path(path) {
                    self.dialog = Some(Dialog::Video { volume: view.volume.clone(), path: path.clone() });
                    self.option = 0;
                } else {
                    self.open_folder(view.volume.clone(), path.clone());
                }
            }
            return;
        }
        let delete_kind = self.selectable_delete_kind();
        if let Some(folder) = self.folders.last() {
            let Some(entry) = self.folder_index(self.selected).and_then(|index| folder.entries.get(index)) else { return };
            if let Some(marked) = &mut self.selection {
                if Some(entry.kind) == delete_kind && !marked.remove(&entry.name) {
                    marked.insert(entry.name.clone());
                }
                return;
            }
            let path = folder.path.join(&entry.name);
            match entry.kind {
                Kind::Folder => self.open_folder(folder.volume.clone(), path),
                Kind::Video => {
                    self.dialog = Some(Dialog::Video { volume: folder.volume.clone(), path });
                    self.option = 0;
                }
            }
        } else if let Some(volume) = self.scan.volumes.get(self.selected).cloned() {
            self.open_folder(volume.clone(), volume.path.clone());
        } else {
            self.next_scan = Instant::now();
            self.status = tr("Buscando discos USB…", "Buscant discos USB…").into();
        }
    }

    fn play_video(&mut self, volume: &Volume, path: PathBuf) {
        self.play_video_at(volume, path, true);
    }

    fn play_video_at(&mut self, volume: &Volume, path: PathBuf, offer_resume: bool) {
        if self.demo.is_some() {
            self.status = tr("Modo demo · El archivo es ficticio.", "Mode demo · El fitxer és fictici.").into();
            return;
        }
        let direct_resume = self.home_item().is_some();
        let mount_id = system_mount(volume);
        let spanish = self.settings.spanish_audio;
        let known_identity = self.identity(volume).map(str::to_owned);
        let cached_record = known_identity.as_deref().and_then(|identity| {
            let entry = self.folders.last()?.entries.iter().find(|entry| path.file_name() == Some(entry.name.as_os_str()))?;
            let key = entry_key(identity, volume, &path, entry.size?, entry.modified?)?;
            Some((key.clone(), self.history_value(&key)))
        });
        let volume = volume.clone();
        self.status = tr("Abriendo vídeo…", "Obrint el vídeo…").into();
        self.request(move || {
            let result = inspect_video(&path, mount_id, spanish).map(|(file, info)| {
                let title = video_title(&path, info.title.as_deref());
                let identity = known_identity.or_else(|| volume_facts(&volume).1);
                let mut key = identity.as_deref().and_then(|identity| progress_key(identity, &volume, &path, &file));
                let mut saved = key.as_ref().and_then(|key| {
                    if let Some((_, Some(value))) = cached_record.as_ref().filter(|(cached, _)| cached == key) { return Some(*value); }
                    resume_record(key, None).ok().flatten()
                }).unwrap_or(0);
                // FAT/exFAT/NTFS drivers may report a different mtime after a
                // reconnect. Keep the existing file record when its path and
                // byte length still match, rather than losing the resume point.
                if saved == 0 {
                    if let (Some(identity), Some(current)) = (identity.as_deref(), key.as_ref()) {
                        if let Some(records) = read_history(&history_prefix(identity)) {
                            if let Some((previous, value)) = matching_size_resume(&records, current) {
                                key = Some(previous);
                                saved = value;
                            }
                        }
                    }
                }
                let (position, _) = unpack_progress(saved);
                let resume = if offer_resume && info.duration_secs.is_none_or(|seconds|
                    position / 1_000_000_000 <= seconds) { position } else { 0 };
                PreparedVideo { file, info, volume, path, title, key, resume, saved, direct_resume }
            });
            DiskResult::Video(result)
        });
    }

    fn open_folder(&mut self, volume: Volume, path: PathBuf) {
        if let Some(view) = self.media_view.as_mut().filter(|view| view.depth == self.folders.len()) {
            view.selected = self.selected;
            view.scroll = self.scroll.pos;
        } else if let Some(view) = self.favorite_view.as_mut().filter(|view| view.depth == self.folders.len()) {
            view.selected = self.selected;
            view.scroll = self.scroll.pos;
        } else if let Some(folder) = self.folders.last_mut() {
            folder.selected = self.selected;
            folder.scroll = self.scroll.pos;
        }
        self.status = tr("Abriendo carpeta…", "Obrint la carpeta…").into();
        self.request(move || {
            let entries = list_folder(&path, system_mount(&volume));
            DiskResult::Folder(volume, path, entries)
        });
    }

    fn back(&mut self) {
        if self.mutating || matches!(self.settings_job, Some(SettingsJob::Save(_))) { return; }
        if let Some(dialog) = self.dialog.take() {
            if matches!(dialog, Dialog::Settings) {
                self.settings_exit = Some(Instant::now());
                self.nav_focus = self.settings_return_focus.take();
            }
            self.prepared = None;
            self.dialog = match dialog {
                Dialog::Appearance => Some(Dialog::Settings),
                Dialog::Accent | Dialog::Cards | Dialog::Extensions => Some(Dialog::Appearance),
                Dialog::Delete { volume, path, finished_episode: false } => Some(Dialog::Video { volume, path }),
                _ => None,
            };
            self.option = 0;
            self.status.clear();
            return;
        }
        if std::mem::take(&mut self.loading_favorites) && self.favorite_view.is_none() { self.set_favorite_section(false); }
        if self.drive_picker() && self.home_all {
            self.home_all = false;
            self.select(self.home_more_index(), true);
            return;
        }
        if self.action.take().is_some() { return; }
        if self.selection.take().is_some() { self.status.clear(); return; }
        if self.retire_read() {
            if self.favorite_view.is_none() { self.set_favorite_section(false); }
            self.status.clear();
            return;
        }
        if self.tab_waiting_for_volume() {
            self.open_nav_tab(0);
            return;
        }
        if self.favorite_cards().is_some() {
            self.leave_favorites();
            self.status.clear();
            return;
        }
        if self.media_cards().is_some() {
            self.leave_media();
            self.status.clear();
            return;
        }
        if self.media_view.as_ref().is_some_and(|view| {
            !self.documental_folder_picker(view) && self.folders.len() == view.depth + 1
                && self.folders.last().is_some_and(|folder| view.paths.first() == Some(&folder.path))
        }) {
            // At the direct category root, Back returns to the drive picker.
            // The category container is deliberately never shown as a card.
            self.leave_media();
            self.status.clear();
            return;
        }
        let Some(folder) = self.folders.pop() else {
            plx_platform::tv::home::go_home();
            return;
        };
        let (index, scroll) = if let Some(view) = self.media_cards().or_else(|| self.favorite_cards()) {
            (view.selected, view.scroll)
        } else if let Some(parent) = self.folders.last() {
            (parent.selected, parent.scroll)
        } else {
            (self.scan.volumes.iter().position(|volume| *volume == folder.volume).unwrap_or_default(), 0.0)
        };
        self.rebuild_movie_grid();
        self.restore_view(index, scroll);
        self.status.clear();
    }

    fn vertical_options(&self) -> usize {
        match self.dialog {
            Some(Dialog::Settings) => UNIFIED_SETTINGS_COUNT,
            Some(Dialog::Appearance) => 3,
            Some(Dialog::Accent | Dialog::Cards) => 3,
            _ => 0,
        }
    }

    fn press_button(&mut self, rect: Rect) {
        self.button_motion = Some((rect, Instant::now()));
    }

    fn nav_available(&self) -> bool {
        self.dialog.is_none() && self.eject_feedback.is_none() && !self.mutating
    }

    fn card_at(&self, x: f32, y: f32) -> Option<usize> {
        let top = self.grid_top();
        let bleed = if self.drive_picker() { 26.0 } else { 14.0 };
        if !self.drive_picker() && (y < top - bleed || y >= GRID_BOTTOM) { return None; }
        // Keep hit targets stable while the existing focus spring scales the artwork.
        self.visible_cards()
            .find(|&index| {
                if self.drive_picker() && index >= self.scan.volumes.len() && index < self.home_recent_offset()
                    && (x < plx_ui::consts::MARGIN_X || x >= HOME_DISK_X - 24.0) { return false; }
                hit(self.tile_rect(index), x, y)
            })
    }

    fn action_at(&self, x: f32, y: f32) -> Option<(Action, Rect)> {
        self.content_actions().into_iter()
            .find(|(action, rect)| self.action_available(*action) && hit(*rect, x, y))
    }

    fn pointer(&mut self, x: f32, y: f32) -> bool {
        let previous_action = self.action_hover.take();
        if matches!(self.dialog, Some(Dialog::Settings | Dialog::Video { .. } | Dialog::Delete { .. } | Dialog::DeleteFolders)) {
            let previous = self.option;
            let option = if matches!(self.dialog, Some(Dialog::Settings)) {
                (0..UNIFIED_SETTINGS_COUNT).find(|&i| hit(unified_setting_rect(i), x, y))
            } else {
                self.horizontal_options().iter().position(|&rect| hit(rect, x, y))
            };
            if let Some(index) = option.filter(|&index| self.option_available(index)) {
                self.option = index;
            }
            return previous != self.option || previous_action.is_some();
        }
        let previous_quality = self.quality_hover;
        self.quality_hover = if self.movie_grid() && self.nav_available() && hit(QUALITY_PILL, x, y) {
            QUALITY_TABS.iter().position(|&rect| hit(rect, x, y)).or(previous_quality)
        } else { None };
        if self.quality_hover.is_some() {
            let left_nav = self.nav_hover.take().is_some() | self.nav_focus.take().is_some();
            let left_action = self.action.take().is_some();
            let left_focus = self.quality_focus.take().is_some();
            return self.quality_hover != previous_quality || left_nav || left_action || left_focus || previous_action.is_some();
        }
        let previous = self.nav_hover;
        self.nav_hover = if self.nav_available() && hit(NAV_PILL, x, y) {
            // Keep the capsule on its last tab while crossing the gap to the next.
            NAV_TABS.iter().position(|rect| hit(*rect, x, y)).or(previous)
        } else {
            None
        };
        if self.nav_hover.is_some() {
            self.nav_focus = None;
            self.quality_focus = None;
        } else if self.nav_available() {
            if let Some((action, _)) = self.action_at(x, y) {
                let moved_focus = self.nav_focus.take().is_some() | self.action.take().is_some() | self.quality_focus.take().is_some();
                self.action_hover = Some(action);
                return previous_action != self.action_hover || previous.is_some() || previous_quality.is_some() || moved_focus;
            }
            if let Some(index) = self.card_at(x, y) {
                let left_nav = self.nav_focus.take().is_some();
                let left_action = self.action.take().is_some();
                let left_quality = self.quality_focus.take().is_some() || previous_quality.is_some();
                if index != self.selected {
                    self.select(index, false);
                    return true;
                }
                return previous.is_some() || left_nav || left_action || left_quality || previous_action.is_some();
            }
        }
        previous != self.nav_hover || previous_quality != self.quality_hover || previous_action.is_some()
    }

    fn click(&mut self, x: f32, y: f32) {
        self.action_hover = None;
        if self.nav_available() && hit(NAV_PILL, x, y) {
            if let Some(tab) = NAV_TABS.iter().position(|rect| hit(*rect, x, y)) {
                self.nav_hover = Some(tab);
                self.open_nav_tab(tab);
            }
            return;
        }
        self.nav_hover = None;
        if self.eject_feedback.is_some() {
            if hit(Rect::new(510.0, 292.0, 900.0, 496.0), x, y) {
                self.eject_feedback = None;
            }
            return;
        }
        if self.mutating || matches!(self.settings_job, Some(SettingsJob::Save(_))) { return; }
        if self.dialog.is_some() {
            let vertical = self.vertical_options();
            let unified = matches!(self.dialog, Some(Dialog::Settings));
            let options: Vec<Rect> = if unified { (0..UNIFIED_SETTINGS_COUNT).map(unified_setting_rect).collect() }
                else if vertical > 0 { SETTINGS_OPTIONS[..vertical].to_vec() }
                else { self.horizontal_options().to_vec() };
            if let Some(index) = options.iter().position(|rect| hit(*rect, x, y)) {
                if !self.option_available(index) { return; }
                self.press_button(options[index]);
                self.option = index;
                self.choose_option();
            }
            return;
        }
        if self.movie_grid() && hit(QUALITY_PILL, x, y) {
            if let Some(index) = QUALITY_TABS.iter().position(|&rect| hit(rect, x, y)) {
                self.nav_focus = None;
                self.action = None;
                self.quality_focus = None;
                self.quality_hover = Some(index);
                self.choose_movie_filter(index);
            }
            return;
        }
        self.quality_focus = None;
        self.quality_hover = None;
        if let Some((action, rect)) = self.action_at(x, y) {
            self.nav_focus = None;
            self.press_button(rect);
            self.action = Some(action);
            self.open_action(action);
            if self.nav_available() { self.action_hover = self.action_at(x, y).map(|(action, _)| action); }
            return;
        }
        if let Some(index) = self.card_at(x, y) {
            self.nav_focus = None;
            if index != self.selected { self.select(index, false); }
            self.action = None;
            self.open();
            return;
        }
        // With no drive cards, clicking the empty Home still retries detection.
        if self.drive_picker() && self.len() == 0 {
            self.open();
        }
    }

    fn key(&mut self, key: Key, repeat: bool) {
        match key {
            Key::PointerHidden => {
                self.nav_hover = None;
                self.quality_hover = None;
                self.action_hover = None;
                return;
            }
            Key::Ok if repeat => {}
            Key::Up | Key::Down | Key::Left { alt: false } | Key::Right { alt: false } | Key::Ok => {
                self.nav_focus = self.nav_hover.take().or(self.nav_focus);
                self.quality_focus = self.quality_hover.take().or(self.quality_focus);
                self.action = self.action_hover.take().or(self.action);
            }
            Key::Back => { self.nav_hover = None; self.quality_hover = None; self.action_hover = None; }
            _ => {}
        }
        if let Some(tab) = self.nav_focus {
            match key {
                Key::Left { alt: false } => self.nav_focus = Some(tab.saturating_sub(1)),
                Key::Right { alt: false } => self.nav_focus = Some((tab + 1).min(NAV_TABS.len() - 1)),
                Key::Down => {
                    self.nav_focus = None;
                    self.action = ACTIONS.iter().find(|(action, _)| self.action_available(*action))
                        .map(|(action, _)| *action);
                    if self.action.is_none() { self.focus_movie_filter(); }
                }
                Key::Back => self.nav_focus = None,
                Key::Ok if !repeat => self.open_nav_tab(tab),
                _ => {}
            }
            return;
        }
        if self.eject_feedback.is_some() {
            if !repeat && matches!(key, Key::Ok | Key::Back) {
                if let Some((volume, success, _)) = self.eject_feedback.take() {
                    if !success && matches!(key, Key::Ok) && self.scan.volumes.contains(&volume) {
                        self.dialog = Some(Dialog::Eject(volume));
                        self.option = 1;
                    }
                }
            }
            return;
        }
        if self.mutating || matches!(self.settings_job, Some(SettingsJob::Save(_))) { return; }
        if self.dialog.is_some() {
            let vertical = self.vertical_options();
            let settings = vertical > 0;
            match key {
                Key::Up | Key::Down if matches!(self.dialog, Some(Dialog::Settings)) => {
                    let row = SETTINGS_ORDER.iter().position(|&index| index == self.option).unwrap_or(0);
                    let next = if key == Key::Up { row.saturating_sub(1) } else { (row + 1).min(SETTINGS_ORDER.len() - 1) };
                    self.option = SETTINGS_ORDER[next];
                }
                Key::Up if settings => self.option = self.option.saturating_sub(1),
                Key::Down if settings => self.option = (self.option + 1).min(vertical - 1),
                Key::Left { alt: false } if matches!(self.dialog, Some(Dialog::Settings)) && self.option != DEMO_SETTING => self.cycle_unified_setting(-1),
                Key::Right { alt: false } if matches!(self.dialog, Some(Dialog::Settings)) && self.option != DEMO_SETTING => self.cycle_unified_setting(1),
                Key::Left { alt: false } if !settings => {
                    self.option = (0..self.option).rev().find(|index| self.option_available(*index)).unwrap_or(self.option);
                }
                Key::Right { alt: false } if !settings => {
                    self.option = (self.option + 1..self.horizontal_options().len())
                        .find(|index| self.option_available(*index)).unwrap_or(self.option);
                }
                Key::Ok if !repeat => {
                    if !self.option_available(self.option) { return; }
                    let rect = if matches!(self.dialog, Some(Dialog::Settings)) { unified_setting_rect(self.option) }
                        else if settings { SETTINGS_OPTIONS[self.option] } else { self.horizontal_options()[self.option] };
                    self.press_button(rect);
                    self.choose_option();
                }
                Key::Back if !repeat => self.back(),
                _ => {}
            }
            return;
        }
        if self.movie_grid() {
            if let Some(index) = self.quality_focus {
                match key {
                    Key::Left { alt: false } => self.quality_focus = Some(index.saturating_sub(1)),
                    Key::Right { alt: false } => self.quality_focus = Some((index + 1).min(QUALITY_TABS.len() - 1)),
                    Key::Up => {
                        self.quality_focus = None;
                        self.action = ACTIONS.iter().rev().find(|(action, _)| self.action_available(*action))
                            .map(|(action, _)| *action);
                        if self.action.is_none() { self.nav_focus = Some(self.nav_tab); }
                    }
                    Key::Down => self.quality_focus = None,
                    Key::Back if !repeat => self.quality_focus = None,
                    Key::Ok if !repeat => self.choose_movie_filter(index),
                    _ => {}
                }
                return;
            }
        }
        if let Some(action) = self.action {
            let actions: Vec<_> = ACTIONS.iter().map(|(action, _)| *action)
                .filter(|action| self.action_available(*action)).collect();
            let index = actions.iter().position(|candidate| *candidate == action).unwrap_or(0);
            match key {
                Key::Left { alt: false } => self.action = actions.get(index.saturating_sub(1)).copied(),
                Key::Right { alt: false } => self.action = actions.get(index + 1).copied().or(Some(action)),
                Key::Down => { self.action = None; self.focus_movie_filter(); }
                Key::Up => { self.action = None; self.nav_focus = Some(self.nav_tab); },
                Key::Back if !repeat => self.back(),
                Key::Ok if !repeat => {
                    if let Some((_, rect)) = self.content_actions().into_iter().find(|(item, _)| *item == action) { self.press_button(rect); }
                    self.open_action(action);
                }
                _ => {}
            }
            return;
        }
        let dir = match key {
            Key::Up if (self.drive_picker() && self.home_neighbor(Dir::Up).is_none())
                || (!self.drive_picker() && self.selected < self.grid().cols) => {
                if !self.focus_movie_filter() { self.nav_focus = Some(self.nav_tab); }
                return;
            }
            Key::Up => Dir::Up,
            Key::Down => Dir::Down,
            Key::Left { alt: false } => Dir::Left,
            Key::Right { alt: false } => Dir::Right,
            Key::Ok if !repeat => return self.open(),
            Key::Back if !repeat => return self.back(),
            _ => return,
        };
        let next = if self.drive_picker() { self.home_neighbor(dir) }
            else { poster_grid::neighbour(self.selected, self.len(), self.grid().cols, dir) };
        if let Some(index) = next {
            self.select(index, false);
        }
    }

    fn horizontal_options(&self) -> &'static [Rect] {
        if matches!(self.dialog, Some(Dialog::Video { ref volume, ref path }) if self.video_is_episode(volume, path)) {
            &EPISODE_VIDEO_OPTIONS
        } else if matches!(self.dialog, Some(Dialog::Video { .. })) { &VIDEO_OPTIONS } else { &OPTIONS }
    }

    fn option_available(&self, index: usize) -> bool {
        match self.dialog.as_ref() {
            Some(Dialog::Settings) if index == 8 => self.manual_refresh.is_none() && !self.mutating,
            Some(Dialog::Video { volume, .. }) if index == 1 => self.demo.is_some() || system_mount(volume).is_some(),
            Some(Dialog::Video { volume, path }) if index == 2 => {
                self.identity(volume).is_some_and(|identity| self.history_loaded.contains(&history_prefix(identity)))
                    && self.video_history_key(volume, path).is_some()
            }
            Some(Dialog::Video { volume, path }) if index == 3 =>
                !self.video_is_episode(volume, path) && self.video_favorite_key(volume, path).is_some(),
            _ => true,
        }
    }

    fn video_history_key(&self, volume: &Volume, path: &std::path::Path) -> Option<String> {
        let identity = self.identity(volume)?;
        let entry = self.folders.last()?.entries.iter().find(|entry| path.file_name() == Some(entry.name.as_os_str()))?;
        entry_key(identity, volume, path, entry.size?, entry.modified?)
    }

    fn item(&self, index: usize) -> Option<(String, &'static str, Cover)> {
        if self.drive_picker() && index >= self.scan.volumes.len() {
            let item = self.dashboard.items.get(index - self.scan.volumes.len())?;
            return Some((item.title.clone(), "", if item.episode.is_some() { Cover::Series } else { Cover::Movie }));
        }
        if let Some(view) = self.media_cards() {
            let path = view.paths.get(index)?;
            let name = clean_media_title(&path.file_name()?.to_string_lossy(), None);
            let cover = path.strip_prefix(&view.volume.path).ok().and_then(art::collection).unwrap_or(Cover::Folder);
            return Some((name, tr("Carpeta", "Carpeta"), cover));
        }
        if let Some(view) = self.favorite_cards() {
            let path = view.paths.get(index)?;
            if is_video_path(path) {
                return Some((video_title(path, None), tr("Vídeo", "Vídeo"), Cover::Movie));
            }
            let name = clean_media_title(&path.file_name()?.to_string_lossy(), None);
            let cover = path.strip_prefix(&view.volume.path).ok().and_then(art::collection).unwrap_or(Cover::Folder);
            return Some((name, tr("Sèrie", "Sèrie"), cover));
        }
        if let Some(folder) = self.folders.last() {
            let entry = self.folder_entry(index)?;
            let (cover, kind) = art::entry_cover(entry, folder.collection);
            Some((display_name(entry, false), kind, cover))
        } else {
        self.scan.volumes.get(index).map(|volume| (volume_name(volume), tr("Disco USB", "Disc USB"), Cover::Drive))
        }
    }

    pub(super) fn draw(&self) {
        let home = self.drive_picker();
        let [r, g, b, _] = theme::USB_HOME_TOP;
        let [rb, gb, bb, _] = theme::USB_HOME_BOTTOM;
        gfx::frame_clear(r, g, b);
        let root = Painter::root();
        root.ambient(Rect::new(0.0, 0.0, 1920.0, 1080.0), 1.0,
            [[r, g, b], [r, g, b], [rb, gb, bb], [rb, gb, bb]], false);
        let page = root.alpha(plx_ui::nav::page_alpha());
        let settings_enter = matches!(self.dialog, Some(Dialog::Settings))
            .then(|| self.dialog_started.map_or(1.0, settings_ease));
        if settings_enter == Some(1.0) {
            self.draw_unified_settings(page, 1.0);
            return;
        }
        let settings_exit = self.settings_exit.map_or(0.0, |started| 1.0 - settings_ease(started));
        let settings_alpha = settings_enter.unwrap_or(settings_exit);
        let p = page.alpha(1.0 - settings_alpha);
        let x = plx_ui::consts::MARGIN_X;
        let width = 1920.0 - 2.0 * x;
        self.draw_nav_pill(p);
        art::draw_brand(p, Rect::new(x, 29.0, 76.0, 76.0));
        for (action, rect) in self.content_actions() {
            if self.action_available(action) {
                if action == Action::ToggleFavorite {
                    self.draw_series_favorite(p, rect);
                    continue;
                }
                let label = match action {
                    Action::ToggleFavorite => continue,
                    Action::Select if self.selection.is_some() => tr("Cancelar", "Cancel·la"),
                    Action::Select if self.nav_tab == SERIES_TAB => tr("Seleccionar series", "Selecciona sèries"),
                    Action::Select if self.nav_tab == DOCUMENTALS_TAB => tr("Seleccionar documentales", "Selecciona documentals"),
                    Action::Select => tr("Seleccionar películas", "Selecciona pel·lícules"),
                    Action::DeleteFolders if self.nav_tab == SERIES_TAB => tr("Eliminar serie", "Esborra la sèrie"),
                    Action::DeleteFolders if self.nav_tab == DOCUMENTALS_TAB => tr("Eliminar documentales", "Esborra documentals"),
                    Action::DeleteFolders => tr("Eliminar películas", "Esborra pel·lícules"),
                };
                let glyph = match action {
                    Action::ToggleFavorite => None,
                    Action::Select if self.selection.is_some() => None,
                    Action::Select | Action::DeleteFolders => Some(Glyph::Trash),
                };
                let tone = if action == Action::DeleteFolders { ButtonTone::Delete }
                    else { ButtonTone::Default };
                self.draw_button(p, rect, label, self.action_hover.or(self.action) == Some(action), tone, glyph);
            }
        }
        if self.movie_grid() { self.draw_movie_filter(p); }
        if self.media_library() {
            if let Some(name) = self.media_series_name() {
                draw_line(p, &clean_series_name(&name), Rect::new(x, 122.0, 880.0, 42.0), theme::size::HEADLINE, true);
            }
            if let Some(marked) = &self.selection {
                let count = if self.nav_tab == MOVIES_TAB {
                    if marked.is_empty() { tr("Selecciona las películas que quieres eliminar.", "Selecciona les pel·lícules que vols esborrar.").into() }
                    else { format!("{} {}", marked.len(), tr("películas seleccionadas", "pel·lícules seleccionades")) }
                } else if self.nav_tab == DOCUMENTALS_TAB {
                    if marked.is_empty() { tr("Selecciona los documentales que quieres eliminar.", "Selecciona els documentals que vols esborrar.").into() }
                    else { format!("{} {}", marked.len(), tr("documentales seleccionados", "documentals seleccionats")) }
                } else if marked.is_empty() {
                    tr("Selecciona la serie que quieres eliminar.", "Selecciona la sèrie que vols esborrar.").into()
                } else { format!("{} {}", marked.len(), tr("series seleccionadas", "sèries seleccionades")) };
                draw_line(p, &count, Rect::new(x, 161.0, 880.0, 28.0), theme::size::CAPTION, false);
            }
        } else {
            self.draw_browser_header(p, home, x, width);
        }

        let no_drive = self.scan.volumes.is_empty() && !self.scan.hidden_mounts;
        let media_status_in_body = self.media_cards().is_some_and(|view| {
            self.len() == 0 && (self.pending.is_some() || !view.paths.is_empty())
        });
        if self.len() == 0 {
            let message = if no_drive {
                tr("Conecta un disco USB.", "Connecta un disc USB.")
            } else if self.tab_waiting_for_volume() {
                tr("Pulsa OK para abrir el disco USB.", "Prem OK per obrir el disc USB.")
            } else if self.media_cards().is_some() && self.pending.is_some() {
                tr("Cargando…", "Carregant…")
            } else if let Some(view) = self.media_cards() {
                if !view.paths.is_empty() && !self.status.is_empty() {
                    self.status.as_str()
                } else {
                    tr("No hay contenido en esta sección.", "No hi ha contingut en aquest apartat.")
                }
            } else if self.favorite_cards().is_some() {
                if self.loading_favorites { tr("Cargando favoritos…", "Carregant els favorits…") }
                else if !self.status.is_empty() { self.status.as_str() }
                else { tr("No hay favoritos en este USB.", "No hi ha cap favorit en aquest USB.") }
            } else if self.movie_grid() {
                if self.movie_pending > 0 { tr("Identificando resolución…", "Identificant la resolució…") }
                else { tr("No hay películas.", "No hi ha pel·lícules.") }
            } else if self.folders.last().is_some() {
                tr("No hay contenido disponible.", "No hi ha contingut disponible.")
            } else {
                tr("CineUSB aún no puede leer el disco. Pulsa OK para volver a intentarlo.",
                    "CineUSB encara no pot llegir el disc. Prem OK per tornar-ho a provar.")
            };
            if no_drive {
                draw_glyph(p, Glyph::Drive, Rect::new(900.0, 438.0, 120.0, 120.0), theme::TEXT_SECONDARY);
                let run = TtfMeasure.fit_line(message, width, theme::size::BODY, false);
                Label::new(run.as_ptr(), theme::size::BODY, theme::TEXT_SECONDARY).h(HAlign::Center)
                    .draw(p, Rect::new(x, 565.0, width, 54.0));
            } else {
                draw_line(p, message, Rect::new(x, 380.0, width, 70.0), theme::size::BODY, false);
            }
        } else {
            if home {
                self.draw_home_tiles(p, x);
            } else {
                let top = self.grid_top();
                p.clip(Rect::new(0.0, top - 14.0, 1920.0, GRID_BOTTOM - top + 14.0));
                let cells = self.visible_cards();
                for i in cells.filter(|index| *index != self.selected) { self.draw_tile(p, i, false); }
                self.draw_tile(p, self.selected, self.grid_has_focus());
                p.clip_clear();
            }
        }
        let storage = self.facts.as_ref().filter(|(disk, _)| !home && self.volume() == Some(disk))
            .and_then(|(disk, (capacity, _))| capacity.map(|facts| (disk, facts)));
        if self.dialog.is_none() {
            if let Some((disk, facts)) = storage {
                self.draw_storage(p, disk, facts);
            }
        }
        if settings_enter.is_some() || self.settings_exit.is_some() {
            self.draw_unified_settings(page, settings_alpha);
        } else if let Some(dialog) = &self.dialog {
            self.draw_dialog(p, dialog);
        } else if !self.status.is_empty() && !no_drive && !media_status_in_body {
            draw_line(p, &self.status, Rect::new(x, 990.0, width - if storage.is_some() { 696.0 } else { 0.0 }, 48.0), theme::size::CAPTION, false);
        } else {
            self.draw_browser_hints(p, x);
        }
        self.draw_eject_feedback(p);
    }

    fn draw_browser_header(&self, p: Painter, home: bool, x: f32, width: f32) {
        let breadcrumb = self.folders.last().filter(|_| self.favorite_cards().is_none()).map(|folder| {
            let relative = folder.path.strip_prefix(&folder.volume.path).unwrap_or(&folder.path);
            let mut parts = vec![volume_name(&folder.volume)];
            parts.extend(relative.components().map(|part| part.as_os_str().to_string_lossy().into_owned()));
            parts.join("   /   ")
        });
        let focused = self.item(self.selected);
        let (title, detail, greeting_icon) = if self.favorite_cards().is_some() || (self.tab_waiting_for_volume() && self.nav_tab == FAVORITES_TAB) {
            (tr("Favoritos", "Favorits"), "", None)
        } else if breadcrumb.is_none() {
            let greeting = match self.hour {
                Some(6..=11) => (tr("Buenos días", "Bon dia"), Some(Glyph::Sun)),
                Some(12..=19) => (tr("Buenas tardes", "Bona tarda"), Some(Glyph::Sunset)),
                Some(_) => (tr("Buenas noches", "Bona nit"), Some(Glyph::Moon)),
                None => (tr("Te damos la bienvenida", "Benvingut a CineUSB"), None),
            };
            if self.demo.is_some() {
                (tr("Modo demo", "Mode demo"), tr("Biblioteca USB ficticia", "Biblioteca USB fictícia"), None)
            } else {
                (greeting.0, tr("Tus películas, series y documentales en el USB",
                    "Les teves pel·lícules, sèries i documentals a l’USB"), greeting.1)
            }
        } else {
            focused.as_ref().map_or(("Biblioteca USB", "Carpeta", None), |(title, kind, _)| (title.as_str(), *kind, None))
        };
        let title_size = if home {
            theme::size::HERO
        } else if TtfMeasure.width_str(title, theme::size::DISPLAY, true) <= width {
            theme::size::DISPLAY
        } else { theme::size::TITLE };
        let title_y = if home { 133.0 } else { 116.0 };
        plx_ui::text_view::TextView::new(title, title_size, theme::TEXT_PRIMARY)
            .with_measure(&TtfMeasure).bold().break_long_words().leading(if home { 76.0 } else { 42.0 })
            .max_lines(if home { 1 } else { 2 }).draw(p, Rect::new(x, title_y, width, 84.0));
        if let Some(icon) = greeting_icon {
            let after_title = x + TtfMeasure.width_str(title, title_size, true) + 16.0;
            draw_glyph(p, icon, Rect::new(after_title, title_y + 7.0, 48.0, 48.0), theme::USB_FAVORITES);
        }
        let count = self.selection.as_ref().map(|marked| {
            if self.nav_tab == SERIES_TAB {
                if marked.is_empty() { tr("Selecciona la serie que quieres eliminar.", "Selecciona la sèrie que vols esborrar.").into() }
                else { format!("{} {}", marked.len(), tr("series seleccionadas", "sèries seleccionades")) }
            } else {
                format!("{} {}", marked.len(), tr("carpetas seleccionadas", "carpetes seleccionades"))
            }
        });
        if home {
            draw_line(p, detail, Rect::new(x, HOME_INTRO_BOTTOM - 34.0, 920.0, 34.0), theme::size::CAPTION, false);
            if let Some((notice, _)) = &self.refresh_notice {
                draw_line(p, notice, Rect::new(96.0, 956.0, 920.0, 32.0), theme::size::MICRO, false);
            }
            return;
        }
        let section = breadcrumb.as_deref().unwrap_or("");
        let section_y = 191.0;
        draw_line(p, section, Rect::new(x, section_y, width, 34.0), theme::size::BODY, false);
        draw_line(p, count.as_deref().unwrap_or(detail), Rect::new(x, section_y + 35.0, 920.0, 37.0), theme::size::CAPTION, false);
    }

    fn draw_browser_hints(&self, p: Painter, x: f32) {
        let quality_focused = self.movie_grid() && self.quality_focus.or(self.quality_hover).is_some();
        let primary = if quality_focused {
            tr("Elegir", "Tria")
        } else if self.selection.is_some() && self.action.is_none() {
            tr("Marcar", "Marca")
        } else if self.action.is_some() {
            tr("Elegir", "Tria")
        } else {
            tr("Abrir", "Obre")
        };
        let back = if self.selection.is_some() && self.action.is_none() {
            tr("Cancelar selección", "Cancel·la la selecció")
        } else {
            tr("Volver", "Torna")
        };
        let mut cursor = x;
        let (direction, target) = if quality_focused {
            (Icon::ChevronDown, tr("Tarjetas", "Targetes"))
        } else if self.movie_grid() && self.nav_focus.is_none() {
            let direction = if self.action.is_some() { Icon::ChevronDown } else { Icon::ChevronUp };
            (direction, tr("Calidad", "Qualitat"))
        } else if self.action.is_none() {
            (Icon::ChevronUp, tr("Barra superior", "Barra superior"))
        } else {
            (Icon::ChevronDown, tr("Tarjetas", "Targetes"))
        };
        cursor = draw_hint(p, cursor, HintKey::Icon(direction), target);
        cursor = draw_hint(p, cursor, HintKey::Ok, primary);
        draw_hint(p, cursor, HintKey::Icon(Icon::ChevronLeft), back);
    }

    fn draw_eject_feedback(&self, p: Painter) {
        let state = self.eject_feedback.as_ref().map(|(volume, success, at)| (volume, Some(*success), *at))
            .or_else(|| self.ejecting.as_ref().map(|(volume, started)| (volume, None, *started)));
        let Some((volume, result, started)) = state else { return };
        let progress = (started.elapsed().as_secs_f32() / 0.75).clamp(0.0, 1.0);
        let smooth = progress * progress * (3.0 - 2.0 * progress);
        let panel = Rect::new(550.0, 300.0, 820.0, 480.0);
        p.rect(Rect::new(0.0, 0.0, 1920.0, 1080.0), 0.0, theme::scrim_black(0.72), theme::scrim_black(0.72), 0.0);
        p.shadow_outside(panel, 28.0, theme::MENU_SHADOW_BLUR, theme::MENU_SHADOW_DY, theme::MENU_SHADOW);
        p.rect_sheened(panel, 28.0, theme::MENU_GLASS_TOP, theme::MENU_GLASS_BOT);
        let center = Rect::new(906.0, 344.0 - smooth * 16.0, 106.0, 106.0).scaled(0.9 + 0.1 * smooth);
        match result {
            Some(true) if smooth > 0.65 => icons::draw(p, Icon::Check, center, theme::USB_FAVORITES),
            Some(false) => icons::draw(p, Icon::Alert, center, theme::DANGER),
            _ => draw_glyph(p, Glyph::Eject, center, theme::TEXT_PRIMARY),
        }
        let headline = match result {
            Some(true) => tr("USB expulsado correctamente", "USB expulsat correctament"),
            Some(false) if self.eject_uncertain => tr("Expulsión USB sin confirmar", "Expulsió USB sense confirmar"),
            Some(false) => tr("No se pudo expulsar el USB", "No s'ha pogut expulsar l'USB"),
            None => tr("Expulsando USB…", "Expulsant l'USB…"),
        };
        draw_line(p, headline, Rect::new(630.0, 477.0, 660.0, 58.0), theme::size::HEADLINE, true);
        draw_line(p, &volume_name(volume), Rect::new(655.0, 539.0, 610.0, 48.0), theme::size::BODY, false);
        let failure_detail = format!("{} {}",
            tr("No desconectes el disco.", "No desconnectis el disc."), self.status);
        let description = match result {
            Some(true) => tr("Ya puedes desconectar el dispositivo con seguridad.", "Ja pots desconnectar el dispositiu amb seguretat."),
            Some(false) => failure_detail.as_str(),
            None => tr("Espera a que termine la operación.", "Espera que acabi l'operació."),
        };
        plx_ui::text_view::TextView::new(description, theme::size::CAPTION, theme::TEXT_SECONDARY)
            .with_measure(&TtfMeasure).break_long_words().max_lines(4)
            .draw(p, Rect::new(620.0, 595.0, 690.0, 132.0));
    }

    fn step_movie_filter(&mut self, dt: f32) -> bool {
        if !self.movie_grid() { return false; }
        self.quality_strip.update(self.movie_filter as i32,
            self.quality_hover.or(self.quality_focus).map_or(-1, |index| index as i32),
            |index| QUALITY_TABS.get(index).map(|rect| (rect.x, rect.w)), widgets::SelMark::Travels, dt);
        self.quality_strip.moving()
    }

    fn draw_movie_filter(&self, p: Painter) {
        let count = match self.movie_count {
            1 => tr("1 película", "1 pel·lícula").to_owned(),
            n => format!("{n} {}", tr("películas", "pel·lícules")),
        };
        let size = theme::size::CAPTION;
        let count_rect = Rect::new(plx_ui::consts::MARGIN_X, QUALITY_PILL.y, 400.0, QUALITY_PILL.h);
        let run = TtfMeasure.fit_line(&count, count_rect.w, size, false);
        Label::new(run.as_ptr(), size, theme::TEXT_SECONDARY).draw(p, count_rect);
        p.rect(QUALITY_PILL, QUALITY_PILL.h * 0.5, theme::USB_CHROME_TOP, theme::USB_CHROME_BOTTOM, 0.0);
        let ground = widgets::TabGround::Tracked { glass: false };
        self.quality_strip.draw(p, QUALITY_TABS[0].y, QUALITY_TABS[0].h, ground);
        for (index, (filter, rect)) in MovieFilter::TABS.into_iter().zip(QUALITY_TABS).enumerate() {
            let run = TtfMeasure.fit_line(filter.label(), rect.w - 16.0, size, true);
            let ink = self.quality_strip.pill_ink(rect, ground);
            widgets::paint_inked(p, &ink, |fm, sm| {
                Label::new(run.as_ptr(), size, widgets::TabPill::mixed_ink(fm, sm))
                    .h(HAlign::Center).bold().draw(p, rect);
            });
            if self.quality_hover.is_none() && self.quality_focus == Some(index) {
                p.rring(rect, rect.h * 0.5, 1.5, self.accent());
            }
        }
    }

    fn draw_nav_pill(&self, p: Painter) {
        if let Some(clock) = clock_label(self.clock_time, &self.settings, self.tv_clock) {
            let size = clock_font_size(self.settings.clock_size);
            let run = TtfMeasure.fit_line(&clock, CLOCK_RECT.w, size, false);
            Label::new(run.as_ptr(), size, theme::TEXT_SECONDARY)
                .h(HAlign::Right).draw(p, CLOCK_RECT);
        }
        p.rect(NAV_PILL, 35.0, theme::USB_CHROME_TOP, theme::USB_CHROME_BOTTOM, 0.0);
        // Same spring capsules as PlxNative; labels and track stay in place.
        self.nav_strip.draw(p, NAV_TABS[0].y, NAV_TABS[0].h, widgets::TabGround::Tracked { glass: false });
        for (index, rect) in NAV_TABS.into_iter().enumerate() {
            let (title, glyph) = match index {
                0 => (tr("Inicio", "Inici"), None),
                1 | 2 | DOCUMENTALS_TAB => (media_tab_title(index), None),
                FAVORITES_TAB => (tr("Favoritos", "Favorits"), Some(Glyph::Star(false))),
                SETTINGS_TAB => (tr("Ajustes", "Ajustos"), Some(Glyph::Settings)),
                _ => (tr("Expulsar disco", "Expulsar disc"), Some(Glyph::Eject)),
            };
            let (focus, selected) = self.nav_strip.mixes((rect.x, rect.w));
            let icon_space = if glyph.is_some() { 39.0 } else { 0.0 };
            let (run, width) = TtfMeasure.fit_line_measured(title, rect.w - 28.0 - icon_space, theme::size::CAPTION, true);
            let x = rect.x + (rect.w - width - icon_space) * 0.5;
            if let Some(glyph) = glyph {
                let icon = Rect::new(x, rect.y + (rect.h - 26.0) * 0.5, 26.0, 26.0);
                if index == FAVORITES_TAB && selected > 0.1 {
                    // Tiny dark under-stroke keeps the yellow outline legible
                    // against the travelling light selection capsule.
                    let shadow = Rect::new(icon.x + 1.0, icon.y + 1.0, icon.w, icon.h);
                    draw_glyph(p, glyph, shadow, theme::USB_FAVORITES_GROUND);
                }
                draw_glyph(p, glyph, icon, nav_glyph_ink(index, focus, selected));
            }
            // PlxNative's ink regions are clipped to the travelling capsule.
            // This avoids illegible intermediate-grey text when it crosses labels.
            let ink = self.nav_strip.pill_ink(rect, widgets::TabGround::Tracked { glass: false });
            widgets::paint_inked(p, &ink, |fm, sm| {
                Label::new(run.as_ptr(), theme::size::CAPTION, widgets::TabPill::mixed_ink(fm, sm))
                    .h(HAlign::Center).bold().draw(p, Rect::new(x + icon_space, rect.y, width, rect.h));
            });
            if self.nav_hover.is_none() && self.nav_focus == Some(index) {
                p.rring(rect, 24.0, 1.5, self.accent());
            }
        }
    }

    fn draw_storage(&self, p: Painter, volume: &Volume, (total, used, free, readonly): (u64, u64, u64, bool)) {
        let rect = Rect::new(1148.0, if readonly { 964.0 } else { 984.0 }, 676.0,
            if readonly { 80.0 } else { 60.0 });
        p.rect(rect, 21.0, theme::USB_CHROME_TOP, theme::USB_CHROME_BOTTOM, 0.0);
        p.rring(rect, 21.0, 1.0, theme::HAIRLINE);
        draw_glyph(p, Glyph::Drive, Rect::new(rect.x + 14.0, rect.y + 7.0, 27.0, 27.0), theme::TEXT_PRIMARY);
        let name = volume_name(volume);
        draw_line(p, &name, Rect::new(rect.x + 47.0, rect.y + 5.0, 128.0, 32.0), theme::size::MICRO, true);
        let fraction = if total == 0 { 0.0 } else { (used as f64 / total as f64).clamp(0.0, 1.0) };
        let percent = format!("{:.0} %", fraction * 100.0);
        let capacity = format!("{} / {} {}", video_size(free), video_size(total), tr("libres", "lliures"));
        for offset in [184.0, 284.0] {
            p.rrect(Rect::new(rect.x + offset, rect.y + 12.0, 1.0, 18.0), 0.0, 0.0, theme::GLASS_RIM);
        }
        draw_line(p, &percent, Rect::new(rect.x + 200.0, rect.y + 5.0, 76.0, 32.0), theme::size::MICRO, true);
        draw_line(p, &capacity, Rect::new(rect.x + 300.0, rect.y + 5.0, 360.0, 32.0), theme::size::MICRO, false);
        if readonly {
            draw_line(p, tr("Solo lectura", "Només lectura"),
                Rect::new(rect.x + 300.0, rect.y + 35.0, 360.0, 25.0), theme::size::MICRO, false);
        }
        let bar = Rect::new(rect.x + 18.0, rect.y + rect.h - 17.0, rect.w - 36.0, 5.0);
        p.rect(bar, 2.5, theme::RAIL_TRACK, theme::RAIL_TRACK, 0.0);
        if fraction > 0.0 {
            let width = bar.w * fraction as f32;
            let radius = 2.5_f32.min(width * 0.5);
            p.rrect(Rect::new(bar.x, bar.y, width, bar.h), radius, radius, self.accent());
        }
    }

    fn draw_unified_settings(&self, p: Painter, progress: f32) {
        let p = p.alpha(progress);
        let x = plx_ui::consts::MARGIN_X;
        art::draw_brand(p, Rect::new(x, 58.0, 66.0, 66.0));
        draw_line(p, tr("Configuración", "Configuració"),
            Rect::new(x, 172.0, 530.0, 76.0), theme::size::DISPLAY, true);
        let (name, description, _) = unified_setting_text(self.option, &self.settings, self.demo.is_some());
        draw_line(p, name, Rect::new(x, 297.0, 530.0, 40.0), theme::size::LABEL, true);
        plx_ui::text_view::TextView::new(description, theme::size::BODY, theme::TEXT_SECONDARY)
            .with_measure(&TtfMeasure).break_long_words().leading(38.0).max_lines(4)
            .draw(p, Rect::new(x, 353.0, 505.0, 156.0));
        let back = draw_hint(p, x, HintKey::Ok, tr("Cambiar", "Canvia"));
        draw_hint(p, back, HintKey::Icon(Icon::ChevronLeft), tr("Volver", "Torna"));

        let headings = [
            tr("Biblioteca / USB", "Biblioteca / USB"), tr("Reproducción", "Reproducció"),
            tr("Sistema", "Sistema"), tr("Apariencia", "Aparença"),
        ];
        for ((indices, heading_y), heading) in SETTINGS_GROUPS.into_iter().zip(headings) {
            let top = unified_setting_rect(indices[0]);
            let bottom = unified_setting_rect(*indices.last().expect("nonempty settings group"));
            let group = Rect::new(top.x - 8.0, top.y - 6.0, top.w + 16.0, bottom.y + bottom.h - top.y + 12.0);
            p.rect(group, 19.0, theme::USB_CHROME_TOP, theme::USB_CHROME_BOTTOM, 0.0);
            draw_line(p, heading, Rect::new(top.x + 14.0, heading_y, top.w - 28.0, 27.0),
                theme::size::CAPTION, true);
            for &index in &indices[..indices.len() - 1] {
                let divider = unified_setting_rect(index);
                p.rect(Rect::new(divider.x + 22.0, divider.y + divider.h + 1.0,
                    divider.w - 44.0, 1.0), 0.0,
                    theme::HAIRLINE, theme::HAIRLINE, 0.0);
            }
        }
        let focus = unified_setting_rect(self.option);
        let highlight = Rect::new(focus.x, self.settings_focus_y.pos, focus.w, focus.h);
        let fill = theme::with_a(self.accent(), 0.20);
        p.rect(highlight, 15.0, fill, fill, 0.0);
        for i in 0..UNIFIED_SETTINGS_COUNT {
            let rect = unified_setting_rect(i);
            let (name, _, value) = unified_setting_text(i, &self.settings, self.demo.is_some());
            draw_line(p, name, Rect::new(rect.x + 22.0, rect.y, 694.0, rect.h),
                theme::size::LABEL, self.option == i);
            let (value_run, value_width) = TtfMeasure.fit_line_measured(&value, 315.0, theme::size::CAPTION, false);
            let value_x = rect.x + rect.w - 24.0 - value_width;
            Label::new(value_run.as_ptr(), theme::size::CAPTION, theme::TEXT_SECONDARY)
                .h(HAlign::Right).draw(p, Rect::new(value_x, rect.y, value_width, rect.h));
            if i == 8 {
                if self.manual_refresh.is_some() {
                    widgets::Spinner::new(rect.x + rect.w - 42.0, rect.y + rect.h * 0.5, 10.0)
                        .phase(self.scan_phase).tint(self.accent()).draw(&Env::inert(), p);
                } else {
                    draw_glyph(p, Glyph::Refresh, Rect::new(rect.x + rect.w - 55.0, rect.y + 14.0, 26.0, 26.0), theme::TEXT_SECONDARY);
                }
            }
            if i == 6 {
                p.rrect(Rect::new(value_x - 30.0, rect.y + 24.0, 16.0, 16.0), 8.0, 8.0, self.accent());
            }
        }
        if let Some((notice, _)) = &self.refresh_notice {
            draw_line(p, notice, Rect::new(x, 870.0, 530.0, 48.0), theme::size::CAPTION, false);
        }
        if !self.status.is_empty() {
            draw_line(p, &self.status, Rect::new(x, 926.0, 530.0, 48.0), theme::size::CAPTION, false);
        }
    }

    fn draw_dialog(&self, p: Painter, dialog: &Dialog) {
        let cancel = tr("Cancelar", "Cancel·la");
        let save_status = |description: &'static str| {
            if matches!(self.settings_job, Some(SettingsJob::Save(_))) { tr("Guardando…", "Desant…") }
            else if self.status.is_empty() { description }
            else { self.status.as_str() }
        };
        let (title, subject, detail, options) = match dialog {
            Dialog::Settings => (tr("Ajustes", "Ajustos"), String::new(), "", ["", ""]),
            Dialog::Appearance => (tr("Apariencia", "Aparença"), String::new(), "", ["", ""]),
            Dialog::Accent => (tr("Color de acento", "Color de ressaltat"), String::new(), "", ["", ""]),
            Dialog::Extensions => (tr("Mostrar extensiones", "Mostra les extensions"), String::new(),
                save_status(""), [tr("Ocultar", "Amaga-les"), tr("Mostrar", "Mostra-les")]),
            Dialog::Cards => (tr("Tamaño de tarjetas", "Mida de les targetes"), String::new(), "", ["", ""]),
            Dialog::Resume(_) => (tr("Reanudar reproducción", "Continua mirant"),
                self.prepared.as_ref().map_or_else(String::new, |media| media.title.clone()), "",
                ["", tr("Empezar desde el principio", "Torna a començar")]),
            Dialog::Next { path, .. } => (tr("Siguiente episodio", "Episodi següent"),
                video_title(path, None), "",
                [tr("Reproducir ahora", "Reprodueix ara"), cancel]),
            Dialog::Video { path, .. } => (tr("Opciones del vídeo", "Opcions del vídeo"),
                self.dialog_video_title(path),
                "",
                [tr("Reproducir", "Reprodueix"), tr("Eliminar archivo", "Esborra el fitxer")]),
            Dialog::Delete { path, finished_episode, .. } => (
                if *finished_episode { tr("¿Borrar el episodio terminado?", "Vols esborrar l’episodi acabat?") }
                else { tr("¿Eliminar este archivo?", "Vols esborrar aquest fitxer?") },
                path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                tr("Se borrará del disco y no se podrá deshacer.", "S’esborrarà del disc i no el podràs recuperar."),
                [cancel, tr("Eliminar archivo", "Esborra el fitxer")]),
            Dialog::DeleteFolders if self.nav_tab == SERIES_TAB && self.selection.as_ref().is_some_and(|marked| marked.len() == 1) =>
                (tr("¿Eliminar esta serie?", "Vols esborrar aquesta sèrie?"),
                    self.selection.as_ref().and_then(|marked| marked.first())
                        .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
                    tr("Se borrará la carpeta con todos sus episodios.", "S’esborrarà la carpeta amb tots els episodis."),
                    [cancel, tr("Eliminar serie", "Esborra la sèrie")]),
            Dialog::DeleteFolders if self.nav_tab == DOCUMENTALS_TAB =>
                (tr("¿Eliminar los documentales seleccionados?", "Vols esborrar els documentals seleccionats?"),
                    format!("{} {}", self.selection.as_ref().map_or(0, BTreeSet::len), tr("documentales", "documentals")),
                    tr("Se borrarán del disco y no se podrá deshacer.", "S’esborraran del disc i no els podràs recuperar."),
                    [cancel, tr("Eliminar documentales", "Esborra documentals")]),
            Dialog::DeleteFolders if self.nav_tab == MOVIES_TAB =>
                (tr("¿Eliminar las películas seleccionadas?", "Vols esborrar les pel·lícules seleccionades?"),
                    format!("{} {}", self.selection.as_ref().map_or(0, BTreeSet::len), tr("películas", "pel·lícules")),
                    tr("Se borrarán del disco y no se podrá deshacer.", "S’esborraran del disc i no les podràs recuperar."),
                    [cancel, tr("Eliminar películas", "Esborra pel·lícules")]),
            Dialog::DeleteFolders => (tr("¿Eliminar las carpetas seleccionadas?", "Vols esborrar les carpetes seleccionades?"),
                format!("{} {}", self.selection.as_ref().map_or(0, BTreeSet::len), tr("carpetas", "carpetes")),
                tr("Se borrará todo su contenido del disco de forma irreversible.",
                    "Se n’esborrarà tot el contingut del disc i no el podràs recuperar."),
                [cancel, tr("Eliminar carpetas", "Esborra les carpetes")]),
            Dialog::Eject(volume) => (tr("Desconectar USB de forma segura", "Expulsa l’USB amb seguretat"),
                volume_name(volume), tr("Espera el aviso antes de retirar el disco.", "Espera l’avís abans de treure el disc."),
                [cancel, tr("Expulsar disco", "Expulsa el disc")]),
        };
        let alpha = self.dialog_started.map_or(1.0, motion_alpha);
        let p = p.alpha(0.82 + alpha * 0.18);
        let (x, w) = (PANEL.x + 45.0, PANEL.w - 90.0);
        let video = matches!(dialog, Dialog::Video { .. });
        if let Dialog::Video { volume, path } = dialog {
            self.draw_video_dialog_body(p, title, &subject, volume, path);
        } else {
            draw_dialog_panel(p, PANEL, theme::USB_DIALOG_TOP, theme::USB_DIALOG_BOTTOM);
        }
        let destructive = matches!(dialog, Dialog::Delete { .. } | Dialog::DeleteFolders);
        let eject = matches!(dialog, Dialog::Eject(_));
        let glyph_rect = Rect::new(x, 326.0, 80.0, 80.0);
        if destructive { icons::draw(p, Icon::Alert, glyph_rect, theme::DANGER); }
        if eject { draw_glyph(p, Glyph::Eject, glyph_rect, theme::TEXT_PRIMARY); }
        let title_y = if destructive || eject { 414.0 } else { 343.0 };
        if !video {
            draw_line(p, title, Rect::new(x, title_y, w, 60.0), theme::size::HEADLINE, true);
        }
        let labels: &[&str] = match dialog {
            Dialog::Settings => &[tr("Idioma", "Idioma"), tr("Subtítulos forzados", "Subtítols forçats"),
                tr("Idioma de audio predeterminado", "Idioma d’àudio per defecte"),
                tr("Siguiente episodio automático", "Episodi següent automàtic"),
                tr("Apariencia", "Aparença"), tr("Biblioteca USB", "Biblioteca USB")],
            Dialog::Appearance => &[tr("Color de acento", "Color de ressaltat"), tr("Tamaño de tarjetas", "Mida de les targetes"), tr("Mostrar extensiones", "Mostra les extensions")],
            Dialog::Accent => &[tr("Morado", "Lila"), tr("Azul", "Blau"), tr("Gris neutro", "Gris neutre")],
            Dialog::Cards => &[tr("Normal", "Normal"), tr("Compacto", "Compacte"), tr("Grande", "Gran")],
            _ => &[],
        };
        if !labels.is_empty() {
            for (index, (rect, &label)) in SETTINGS_OPTIONS.into_iter().zip(labels).enumerate() {
                self.draw_button(p, rect, label, self.option == index, ButtonTone::Default, None);
            }
            if !self.status.is_empty() {
                draw_line(p, &self.status, Rect::new(x, 737.0, w, 40.0), theme::size::MICRO, false);
            }
            return;
        }
        if !video {
            draw_line(p, &subject, Rect::new(x, title_y + 67.0, w, 52.0), theme::size::BODY, true);
            draw_line(p, detail, Rect::new(x, title_y + 133.0, w, 48.0), theme::size::CAPTION, false);
        }
        let resume_label = match dialog {
            Dialog::Resume(seconds) => format!("{} {}", tr("Continuar desde", "Continua a partir de"), playback_time(*seconds / 1_000_000_000)),
            _ => String::new(),
        };
        if matches!(dialog, Dialog::Next { .. }) {
            let countdown = format!("{} {} s", tr("Empieza en", "Comença d’aquí a"), self.countdown_display);
            draw_line(p, &countdown, Rect::new(x, 488.0, w, 48.0), theme::size::BODY, false);
        }
        let watched_label = if let Dialog::Video { volume, path } = dialog {
            let watched = self.video_history_key(volume, path).and_then(|key| self.history_value(&key)) == Some(WATCHED);
            Some(if watched { (tr("Marcar como no visto", "Marcar com a no vist"), Glyph::Shared(Icon::CircleSlash)) }
                else { (tr("Marcar como visto", "Marcar com a vist"), Glyph::Shared(Icon::Check)) })
        } else { None };
        let favorite_label = if let Dialog::Video { volume, path } = dialog {
            let marked = self.video_favorite_key(volume, path).and_then(|key| self.history_value(&key)) == Some(FAVORITE);
            Some(if marked { (tr("Quitar", "Treu"), Glyph::Star(true)) }
                else { (tr("Añadir", "Afegeix"), Glyph::Star(false)) })
        } else { None };
        for (index, (&rect, label)) in self.horizontal_options().iter().zip(options.into_iter()
            .chain(watched_label.map(|(label, _)| label))
            .chain(favorite_label.map(|(label, _)| label))).enumerate() {
            let label = if index == 0 && !resume_label.is_empty() { &resume_label } else { label };
            if !self.option_available(index) { continue; }
            let glyph = match (dialog, index) {
                (Dialog::Video { .. }, 0) => Some(Glyph::Play),
                (Dialog::Video { .. } | Dialog::Delete { .. } | Dialog::DeleteFolders, 1) => Some(Glyph::Trash),
                (Dialog::Eject(_), 1) => Some(Glyph::Eject),
                (Dialog::Video { .. }, 2) => watched_label.map(|(_, glyph)| glyph),
                (Dialog::Video { .. }, 3) => favorite_label.map(|(_, glyph)| glyph),
                _ => None,
            };
            if matches!(dialog, Dialog::Video { .. }) && index == 3 {
                let marked = matches!(glyph, Some(Glyph::Star(true)));
                self.draw_favorite_chip(p, rect, label, marked, self.option == index);
            } else {
                let tone = match (dialog, index, glyph) {
                    (_, 1, Some(Glyph::Trash)) => ButtonTone::Delete,
                    (Dialog::Video { .. }, 2, Some(Glyph::Shared(Icon::Check))) => ButtonTone::Watched,
                    (Dialog::Video { .. }, 2, _) => ButtonTone::Unwatched,
                    _ => ButtonTone::Default,
                };
                self.draw_button(p, rect, label, self.option == index, tone, glyph);
            }
        }
    }

    fn draw_video_dialog_body(&self, p: Painter, heading: &str, title: &str, volume: &Volume, path: &std::path::Path) {
        use plx_ui::text_view::TextView;
        let options = self.horizontal_options();
        let (x, width) = video_dialog_column(options);
        let context = self.video_episode_context(volume, path);
        let details = self.details.get(&self.detail_key_for(path));
        let chips = video_detail_chips(details, self.is_library_movie(volume, path), width, &TtfMeasure);
        let raw_name = path.file_name().unwrap_or_default().to_string_lossy();
        let filename = video_filename_lines(&raw_name, width, &TtfMeasure);
        let context_view = TextView::new(&context, theme::size::CAPTION, theme::USB_FAVORITES)
            .with_measure(&TtfMeasure).bold().break_long_words().leading(29.0);
        let context_h = if context.is_empty() { 0.0 } else { context_view.measure_h(width) + 4.0 };
        let chips_h = chips.last().map_or(0.0, |chip| chip.rect.y + chip.rect.h + 9.0);
        let filename_h = filename.len() as f32 * VIDEO_FILENAME_LINE_H;
        let other_h = 30.0 + 4.0 + context_h + 8.0 + chips_h + 28.0 + filename_h;
        let title_view = |size| TextView::new(title, size, theme::TEXT_PRIMARY)
            .with_measure(&TtfMeasure).bold().break_long_words().leading(size as f32 + 4.0);
        let budget = (VIDEO_ACTION_Y - 18.0 - other_h - 24.0 - plx_ui::consts::MARGIN_Y).max(26.0);
        let title = [theme::size::TITLE, theme::size::HEADLINE, theme::size::CAPTION, theme::size::MICRO, theme::size::USB_CARD_META]
            .into_iter().map(title_view).find(|view| view.measure_h(width) <= budget)
            .unwrap_or_else(|| {
                let size = theme::size::USB_CARD_META;
                title_view(size).max_lines((budget / (size as f32 + 4.0)).floor().max(1.0) as usize)
            });
        let title_h = title.measure_h(width);
        let content_h = other_h + title_h;
        let mut y = VIDEO_ACTION_Y - 18.0 - content_h;
        let panel = Rect::new(x - VIDEO_DIALOG_INSET, y - 24.0, width + 2.0 * VIDEO_DIALOG_INSET,
            VIDEO_ACTION_Y + VIDEO_ACTION_H + 28.0 - (y - 24.0));
        draw_dialog_panel(p, panel, theme::USB_DRIVE_TOP, theme::USB_DRIVE_BOTTOM);
        draw_line(p, heading, Rect::new(x, y, width, 30.0), theme::size::CAPTION, false);
        y += 34.0;
        title.draw(p, Rect::new(x, y, width, title_h));
        y += title_h;
        if context_h > 0.0 {
            context_view.draw(p, Rect::new(x, y + 4.0, width, context_h - 4.0));
            y += context_h;
        }
        y += 8.0;
        for chip in chips { chip.draw(p, x, y); }
        y += chips_h;
        draw_line(p, tr("Archivo original", "Fitxer original"), Rect::new(x, y, width, 28.0), theme::size::CAPTION, false);
        y += 28.0;
        for line in filename {
            draw_line(p, line, Rect::new(x, y, width, VIDEO_FILENAME_LINE_H), theme::size::MICRO, false);
            y += VIDEO_FILENAME_LINE_H;
        }
    }

    fn video_episode_context(&self, volume: &Volume, path: &std::path::Path) -> String {
        if !self.video_is_episode(volume, path) { return String::new(); }
        let show = self.episode_show(volume, path);
        let relative = show.as_deref().and_then(|show| path.strip_prefix(show).ok())
            .or_else(|| path.strip_prefix(&volume.path).ok()).unwrap_or(path);
        let name = show.as_deref().and_then(std::path::Path::file_name).map(|name| clean_series_name(&name.to_string_lossy()))
            .or_else(|| {
                let (prefix, _, _) = continuity::episode(path.file_name()?)?;
                (!prefix.is_empty()).then(|| clean_series_name(&String::from_utf8_lossy(prefix)))
            });
        let Some((season, episode)) = home::episode_numbers(relative, show.is_some()) else { return name.unwrap_or_default(); };
        let number = format!("Temporada {season} · {} {episode}", tr("Episodio", "Episodi"));
        name.filter(|name| !name.is_empty()).map_or_else(|| number.clone(), |name| format!("{name} · {number}"))
    }

    fn dialog_video_title(&self, path: &std::path::Path) -> String {
        let key = self.detail_key_for(path);
        let tagged = self.details.get(&key).and_then(|details| details.title.as_deref());
        let movie = matches!(&self.dialog, Some(Dialog::Video { volume, .. }) if self.is_library_movie(volume, path));
        let mut title = if movie { movie_title(path, tagged) } else { video_title(path, tagged) };
        if let Some(extension) = path.extension().filter(|_| self.settings.extensions) {
            title.push('.');
            title.push_str(&extension.to_string_lossy());
        }
        title
    }

    /// A compact, clearly stateful favorite action in the video's own menu.
    /// Series favorites are still resolved from an episode to the series root.
    fn draw_series_favorite(&self, p: Painter, rect: Rect) {
        let Some((root, key)) = self.series_favorite_key() else { return };
        let marked = self.history_value(&key) == Some(FAVORITE);
        let focused = self.action_hover.or(self.action) == Some(Action::ToggleFavorite);
        // A real 340 ms transition, reversing the yellow fill when removed.
        let alpha = self.favorite_motion.as_ref()
            .filter(|(changed, _, _)| changed == &root)
            .map_or(if marked { 1.0 } else { 0.0 }, |(_, target, at)| {
                let t = favorite_ease(at.elapsed().as_secs_f32() / FAVORITE_MOTION.as_secs_f32());
                if *target { t } else { 1.0 - t }
            });
        let pop = self.favorite_motion.as_ref()
            .filter(|(changed, _, at)| changed == &root && at.elapsed() < FAVORITE_MOTION)
            .map_or(0.0, |(_, _, at)| {
                let t = (at.elapsed().as_secs_f32() / FAVORITE_MOTION.as_secs_f32()).clamp(0.0, 1.0);
                (t * std::f32::consts::PI).sin()
            });
        let rect = rect.scaled(1.0 + pop * 0.018);
        let neutral = if focused { self.accent() } else { theme::CONTROL_IDLE_FILL };
        let background = favorite_lerp(neutral, FAVORITE_YELLOW, alpha);
        let ink = favorite_lerp(if focused { theme::ACCENT_INK } else { theme::TEXT_PRIMARY },
            [0.16, 0.13, 0.08, 1.0], alpha);
        widgets::draw_control_face(p, rect, background, focused, widgets::ControlGround::Keyed);
        if focused { p.rring(rect, rect.h * 0.5, 2.0, theme::CONTROL_FOCUS_RING); }
        if pop > 0.0 {
            p.alpha(pop * 0.72).rring(rect.scaled(1.02), rect.h * 0.5, 1.7, FAVORITE_YELLOW);
        }
        let label = if marked { tr("Quitar de favoritos", "Treure dels favorits") }
            else { tr("Añadir a favoritos", "Afegir als favorits") };
        let star = Rect::new(rect.x + 18.0, rect.y + (rect.h - 27.0) * 0.5, 27.0, 27.0)
            .scaled(1.0 + 0.17 * pop);
        draw_glyph(p.alpha(1.0 - alpha), Glyph::Star(false), star, ink);
        draw_glyph(p.alpha(alpha), Glyph::Star(true), star, ink);
        let fitted = TtfMeasure.fit_line(label, rect.w - 75.0, theme::size::CAPTION, true);
        Label::new(fitted.as_ptr(), theme::size::CAPTION, ink).h(HAlign::Center).bold()
            .draw(p, Rect::new(rect.x + 57.0, rect.y, rect.w - 69.0, rect.h));
    }

    fn draw_favorite_chip(&self, p: Painter, rect: Rect, label: &str, marked: bool, focused: bool) {
        let scale = self.button_motion.filter(|(pressed, at)| *pressed == rect && at.elapsed() < MOTION)
            .map_or(1.0, |(_, at)| 0.975 + motion_alpha(at) * 0.025);
        let rect = rect.scaled(scale);
        let radius = 24.0;
        let fill = if focused { self.accent() } else { theme::CONTROL_IDLE_FILL };
        p.rect_sheened(rect, radius, fill, fill);
        if focused {
            p.rring(rect, radius, 2.0, theme::CONTROL_FOCUS_RING);
        } else if marked {
            p.rring(rect, radius, 1.5, self.accent());
        }
        let badge = Rect::new(rect.x + 15.0, rect.y + 15.0, 36.0, 36.0);
        let badge_fill = if focused { theme::scrim_black(0.14) }
            else if marked { self.accent() } else { theme::CONTROL_IDLE_FILL };
        p.rect_sheened(badge, 18.0, badge_fill, badge_fill);
        if !marked && !focused { p.rring(badge, 18.0, 1.0, theme::TEXT_SECONDARY); }
        let star_ink = if focused || marked { theme::ACCENT_INK } else { self.accent() };
        draw_glyph(p, Glyph::Star(marked), Rect::new(badge.x + 7.0, badge.y + 7.0, 22.0, 22.0), star_ink);
        let ink = if focused { theme::ACCENT_INK } else { theme::TEXT_PRIMARY };
        let text = TtfMeasure.fit_line(label, rect.w - 76.0, theme::size::CAPTION, true);
        Label::new(text.as_ptr(), theme::size::CAPTION, ink).h(HAlign::Center).bold()
            .draw(p, Rect::new(rect.x + 63.0, rect.y, rect.w - 73.0, rect.h));
    }

    fn accent(&self) -> [f32; 4] {
        theme::usb_accent(self.settings.accent)
    }

    fn draw_button(&self, p: Painter, rect: Rect, label: &str, focused: bool, tone: ButtonTone, glyph: Option<Glyph>) {
        let scale = self.button_motion.filter(|(pressed, at)| *pressed == rect && at.elapsed() < MOTION)
            .map_or(1.0, |(_, at)| 0.975 + motion_alpha(at) * 0.025);
        draw_button(p, rect.scaled(scale), label, focused, tone, glyph, self.accent());
    }

    fn draw_tile(&self, p: Painter, index: usize, focused: bool) {
        let frame = self.tile_rect(index);
        if frame.w == 0.0 { return; }
        let scale = self.pop.scale(index, focused && self.action.is_none(), &USB_STYLE);
        let card = frame.scaled(scale);
        let radius = USB_STYLE.tile_radius(card, scale);
        let lift = ((scale - 1.0) / (USB_STYLE.focus_scale - 1.0)).clamp(0.0, 1.0);
        if self.drive_picker() {
            if let Some(item) = index.checked_sub(self.scan.volumes.len()).and_then(|index| self.dashboard.items.get(index)) {
                self.draw_resume_card(p, item, media_card::Frame { rect: card, radius, lift, accent: self.accent() });
            } else if index == self.home_more_index() && self.dashboard.items.len() > 3 {
                let label = if self.home_all { tr("Volver", "Torna").into() }
                    else { format!("{} ({})", tr("Ver todo", "Veure tot"), self.dashboard.items.len()) };
                self.draw_button(p, frame, &label, focused, ButtonTone::Default, None);
            } else if let Some(recent) = index.checked_sub(self.home_recent_offset()).and_then(|position| self.home_recent.get(position)) {
                let item = &recent.item;
                let details = self.details.get(&item.detail_key);
                let title = self.home_media_title(item, recent.folder.is_some());
                self.media_art.draw(p, media_card::Frame { rect: card, radius, lift, accent: self.accent() }, media_card::Card {
                    title: &title, category: Self::category(&item.volume, &item.path, recent.folder.is_some()),
                    formats: details.map_or_else(Default::default, |details| media_card::Formats::file(details, self.settings.spanish_audio)),
                    show_category: false, readout: Readout::Recent(details.and_then(|details| details.duration_secs)),
                    fraction: 0.0, watched: false, favorite: false,
                });
            } else {
                self.draw_home_card(p, index, card, radius, lift);
            }
            return;
        }
        let media = self.draw_library_card(p, index, media_card::Frame { rect: card, radius, lift, accent: self.accent() });
        if !media {
            let Some((title, kind, cover)) = self.item(index) else { return };
            let kind = if self.media_library() && (kind == tr("Películas", "Pel·lícules")
                || kind == tr("Series", "Sèries") || kind == "Documental") { "" } else { kind };
            self.draw_folder_card(p, card, radius, lift, &title, kind, cover);
        }
        if let Some(checked) = self.delete_selection_mark(index) {
            let mark = Rect::new(card.x + card.w - 57.0, card.y + 17.0, 40.0, 40.0);
            let fill = if checked { self.accent() } else { theme::scrim_black(0.6) };
            p.rect(mark, 20.0, fill, fill, 0.0);
            p.rring(mark, 20.0, 2.0, theme::TEXT_PRIMARY);
            if checked {
                p.rring(card, radius, 3.0, self.accent());
                icons::draw(p, Icon::Check, Rect::new(mark.x + 6.0, mark.y + 6.0, 28.0, 28.0), theme::ACCENT_INK);
            }
        }
    }

    fn delete_selection_mark(&self, index: usize) -> Option<bool> {
        let marked = self.selection.as_ref()?;
        let entry = self.folder_entry(index)?;
        (Some(entry.kind) == self.selectable_delete_kind()).then(|| marked.contains(&entry.name))
    }

    fn category(volume: &Volume, path: &std::path::Path, episodic: bool) -> Category {
        Category::for_path(path.strip_prefix(&volume.path).unwrap_or(path), episodic)
    }

    fn draw_resume_card(&self, p: Painter, item: &home::Item, frame: media_card::Frame) {
        let details = self.details.get(&item.detail_key);
        let duration = details.and_then(|details| details.duration_secs);
        let title = self.home_media_title(item, item.episode.is_some() || item.show);
        let (fraction, remaining) = media_card::file_progress(item.progress, duration);
        self.media_art.draw(p, frame, media_card::Card {
            title: &title, category: Self::category(&item.volume, &item.path, item.show || item.episode.is_some()),
            formats: details.map_or_else(Default::default, |details| media_card::Formats::file(details, self.settings.spanish_audio)),
            show_category: false,
            readout: Readout::Resume { episode: item.episode, remaining, season_remaining: item.season_remaining },
            fraction, watched: false, favorite: item.favorite,
        });
    }

    fn home_media_title(&self, item: &home::Item, series: bool) -> String {
        if series { return clean_series_name(&item.title); }
        let embedded = self.details.get(&item.detail_key).and_then(|details| details.title.as_deref());
        if Self::category(&item.volume, &item.path, false) == Category::Movie {
            movie_title(&item.path, embedded)
        } else {
            home_file_title(&item.path, embedded)
        }
    }

    fn draw_library_card(&self, p: Painter, index: usize, frame: media_card::Frame) -> bool {
        if let Some((volume, key)) = self.card_file(index) {
            let path = &key.0;
            let details = self.details.get(&key);
            let show = self.episode_show(volume, path);
            let movie = self.is_library_movie(volume, path);
            let source_title = details.and_then(|value| value.title.as_deref());
            let mut labels = self.card_labels.borrow_mut();
            if labels.len() >= 512 && !labels.contains_key(&key) { labels.clear(); }
            let valid = labels.get(&key).is_some_and(|label|
                label.show == show && label.movie == movie && label.source_title.as_deref() == source_title);
            if !valid {
                let episode_path = show.as_ref().and_then(|show| path.strip_prefix(show).ok())
                    .or_else(|| path.strip_prefix(&volume.path).ok()).unwrap_or(path);
                let numbers = (!movie).then(|| home::episode_numbers(episode_path, show.is_some())).flatten();
                let episodic = !movie && (numbers.is_some() || show.is_some());
                let category = Self::category(volume, path, episodic);
                let title = if episodic {
                    show.as_deref().and_then(std::path::Path::file_name).and_then(|name| name.to_str())
                        .map(clean_series_name)
                        .or_else(|| {
                            let (prefix, _, _) = continuity::episode(path.file_name()?)?;
                            let prefix = std::str::from_utf8(prefix).ok()?;
                            (!prefix.is_empty()).then(|| clean_series_name(prefix))
                        }).unwrap_or_else(|| video_title(path, source_title))
                } else if movie || category == Category::Movie {
                    movie_title(path, source_title)
                } else {
                    video_title(path, source_title)
                };
                labels.insert(key.clone(), CardLabel {
                    show, movie, source_title: source_title.map(str::to_owned), title, category, numbers, episodic,
                });
            }
            let label = labels.get(&key).expect("prepared card label");
            let duration = details.and_then(|details| details.duration_secs);
            let record = self.file_record(volume, &key);
            let (fraction, _) = media_card::file_progress(record, duration);
            let favorite = !label.episodic && self.favorite_marked(volume, path);
            self.media_art.draw(p, frame, media_card::Card {
                title: &label.title, category: label.category, fraction, favorite, watched: record == WATCHED,
                formats: details.map_or_else(Default::default, |details| media_card::Formats::file(details, self.settings.spanish_audio)),
                show_category: !self.media_library(),
                readout: if label.episodic { Readout::Episode { number: label.numbers, duration } } else { Readout::File(duration) },
            });
            return true;
        }
        let Some(series) = self.series_card(index) else { return false; };
        let summary = self.series_cache.get(&series.path).and_then(|entry| entry.summary.as_ref());
        if summary.is_none() && !series.known { return false; }
        let category = Self::category(series.volume, &series.path, true);
        let title = clean_series_name(&series.path.file_name().unwrap_or_default().to_string_lossy());
        let counts = summary.map(|summary| summary.listing.counts);
        let watched = summary.map_or(0, |summary| summary.watched);
        let total = counts.map_or(0, |counts| counts.episodes);
        self.media_art.draw(p, frame, media_card::Card {
            title: &title, category,
            formats: summary.map_or_else(Default::default, |summary| summary.formats(self.settings.spanish_audio)),
            show_category: !self.media_library(),
            readout: counts.map_or(Readout::File(None), Readout::Series),
            fraction: if total > 0 { watched as f32 / total as f32 } else { 0.0 },
            watched: total > 0 && watched == total,
            favorite: self.favorite_marked(series.volume, &series.path),
        });
        true
    }

    fn episode_show(&self, volume: &Volume, path: &std::path::Path) -> Option<PathBuf> {
        if self.is_library_movie(volume, path) { return None; }
        path.strip_prefix(&volume.path).ok()?;
        if let Some((_, video)) = self.inventory_video(path).filter(|(disk, _)| *disk == volume) {
            return video.show.as_ref().map(|show| volume.path.join(show));
        }
        let folder = self.folders.last().filter(|folder| &folder.volume == volume)?;
        let show = self.series_root().filter(|show| path.starts_with(show))?;
        let categorized = show.strip_prefix(&volume.path).ok()?.parent()?.iter()
            .any(|part| media_folder_score(&part.to_string_lossy(), 2) > 0);
        let season = path.strip_prefix(&show).ok()?.parent()?.iter()
            .any(|part| continuity::season_number(&part.to_string_lossy()).is_some());
        let prefix_matches = path.file_name().and_then(continuity::episode).is_some_and(|(prefix, _, _)| {
            show.file_name().and_then(|name| home::normalized_prefix(clean_series_name(&name.to_string_lossy()).as_bytes()))
                .is_some_and(|name| home::normalized_prefix(prefix).as_ref() == Some(&name))
        });
        (categorized || season || prefix_matches || self.series_cache.get(&show).is_some_and(|entry| entry.summary.is_some())
            || folder.entries.iter().any(|entry| entry.kind == Kind::Folder && season_folder(&entry.name.to_string_lossy())))
            .then_some(show)
    }

    fn favorite_marked(&self, volume: &Volume, path: &std::path::Path) -> bool {
        self.identity(volume).and_then(|id| favorite_key(id, volume, path))
            .and_then(|key| self.history_value(&key)) == Some(FAVORITE)
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_folder_card(&self, p: Painter, card: Rect, radius: f32, lift: f32, title: &str, kind: &str, cover: Cover) {
        if cover == Cover::Drive {
            p.shadow_outside(card, radius, 21.0, 9.0, theme::CARD_SHADOW);
            if lift > 0.0 {
                p.shadow_outside(card, radius, 21.0, 5.0, theme::with_a(self.accent(), lift * 0.42));
            }
            cover.draw(p, card, radius, lift);
            let accent = theme::mix(self.accent(), theme::TEXT_PRIMARY, 0.38);
            if lift > 0.0 {
                p.rring(card, radius, 2.5, theme::with_a(accent, lift));
            }
            let icon_size = card.w * 0.54;
            draw_glyph(p, Glyph::Drive, Rect::new(card.x + (card.w - icon_size) * 0.5,
                card.y + card.h * 0.36 - icon_size * 0.5, icon_size, icon_size), theme::TEXT_PRIMARY);
            let text_x = card.x + 19.0;
            let text_width = card.w - 38.0;
            draw_line(p, kind, Rect::new(text_x, card.y + card.h - 82.0, text_width, 33.0), theme::size::LABEL, true);
            draw_line(p, title, Rect::new(text_x, card.y + card.h - 49.0, text_width, 30.0), theme::size::CAPTION, false);
            return;
        }
        cover.draw(p, card, radius, lift);
        if lift > 0.0 {
            p.rring(card, radius, theme::CONTROL_FOCUS_RING_W, theme::with_a(self.accent(), lift));
        }
        let caption = widgets::poster_title(p, card, radius, title, false, &TtfMeasure);
        if !kind.is_empty() {
            draw_line(p, kind, Rect::new(card.x + 16.0, caption.y - 30.0, card.w - 32.0, 28.0),
                theme::size::CAPTION, false);
        }
    }
}

fn hit(rect: Rect, x: f32, y: f32) -> bool {
    x >= rect.x && x < rect.x + rect.w && y >= rect.y && y < rect.y + rect.h
}

enum HintKey { Ok, Icon(Icon) }

fn draw_hint(p: Painter, x: f32, key: HintKey, text: &str) -> f32 {
    let width = if matches!(key, HintKey::Ok) { 42.0 } else { 30.0 };
    let rect = Rect::new(x, 1001.0, width, 26.0);
    p.rring(rect, 7.0, 1.0, theme::PILL_KEYLINE);
    match key {
        HintKey::Ok => {
            let run = TtfMeasure.fit_line("OK", width, theme::size::MICRO, true);
            Label::new(run.as_ptr(), theme::size::MICRO, theme::TEXT_READING).h(HAlign::Center).bold().draw(p, rect);
        }
        HintKey::Icon(icon) => icons::draw(p, icon, Rect::new(x + 5.0, 1004.0, 20.0, 20.0), theme::TEXT_READING),
    }
    let (run, text_width) = TtfMeasure.fit_line_measured(text, 300.0, theme::size::MICRO, false);
    Label::new(run.as_ptr(), theme::size::MICRO, theme::TEXT_SECONDARY).h(HAlign::Left)
        .draw(p, Rect::new(x + width + 10.0, 990.0, text_width, 48.0));
    x + width + 10.0 + text_width + 24.0
}

fn video_size(bytes: u64) -> String {
    let (unit, divisor) = match bytes {
        1_000_000_000_000.. => ("TB", 1_000_000_000_000),
        1_000_000_000.. => ("GB", 1_000_000_000),
        1_000_000.. => ("MB", 1_000_000),
        1_000.. => ("KB", 1_000),
        _ => return format!("{bytes} B"),
    };
    let tenths = bytes / (divisor / 10);
    let number = if tenths < 100 {
        i18n::current().decimal(tenths as i64, 1)
    } else {
        i18n::current().decimal((bytes / divisor) as i64, 0)
    };
    format!("{number} {unit}")
}

fn playback_time(seconds: u64) -> String {
    format!("{:02}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60)
}

fn video_duration(seconds: u64) -> String {
    let minutes = seconds / 60;
    if minutes == 0 {
        "< 1 min".into()
    } else if minutes < 60 {
        format!("{minutes} min")
    } else {
        format!("{} h {:02} min", minutes / 60, minutes % 60)
    }
}

fn resolution_label(width: u32, height: u32, movie: bool) -> Option<String> {
    let quality = MovieQuality::of(width, height)?;
    let (wide, short) = (width.max(height), width.min(height));
    if movie || quality == MovieQuality::Uhd || (wide >= 1800 && wide < 2100) {
        return Some(quality.label().into());
    }
    Some(match (wide, short) {
        (1280, 500..=720) => "720p".into(),
        (1..=1024, 1..=576) => "SD".into(),
        _ => format!("{width}×{height}"),
    })
}

fn draw_dialog_panel(p: Painter, panel: Rect, top: [f32; 4], bottom: [f32; 4]) {
    let scrim = theme::scrim_black(0.48);
    p.rect(Rect::new(0.0, 0.0, 1920.0, 1080.0), 0.0, scrim, scrim, 0.0);
    p.shadow_outside(panel, theme::ALERT_PANEL_RAD, theme::MENU_SHADOW_BLUR, theme::MENU_SHADOW_DY, theme::MENU_SHADOW);
    p.rect(panel, theme::ALERT_PANEL_RAD, top, bottom, 0.0);
    p.rring(panel, theme::ALERT_PANEL_RAD, 1.0, theme::HAIRLINE);
}

fn video_dialog_column(options: &[Rect]) -> (f32, f32) {
    let x = options[0].x;
    let last = options[options.len() - 1];
    (x, last.x + last.w - x)
}

const VIDEO_FILENAME_LINE_H: f32 = 26.0;

fn video_filename_lines<'a>(mut name: &'a str, width: f32, measure: &dyn Measure) -> Vec<&'a str> {
    let mut lines = Vec::new();
    while !name.is_empty() {
        if measure.width_str(name, theme::size::MICRO, false) <= width {
            lines.push(name);
            break;
        }
        let boundaries: Vec<_> = name.char_indices().skip(1).map(|(index, _)| index).collect();
        let fit = boundaries.partition_point(|&end| measure.width_str(&name[..end], theme::size::MICRO, false) <= width);
        let end = if fit > 0 { boundaries[fit - 1] } else { name.chars().next().unwrap().len_utf8() };
        let (line, rest) = name.split_at(end);
        lines.push(line);
        name = rest;
    }
    lines
}

struct VideoDetailChip {
    text: String,
    rect: Rect,
    format: bool,
}

impl VideoDetailChip {
    fn draw(self, p: Painter, start: f32, top: f32) {
        let rect = Rect::new(start + self.rect.x, top + self.rect.y, self.rect.w, self.rect.h);
        p.rect_sheened(rect, 9.0, theme::CONTROL_IDLE_FILL, theme::CONTROL_IDLE_FILL);
        let icon = if self.format {
            p.rring(rect, 9.0, 1.0, theme::TEXT_SECONDARY);
            let svg = icons::Svg::new(include_str!("../../../assets/icons/cineusb-dynamic-range.svg"));
            icons::draw_svg(p, svg, Rect::new(rect.x + 9.0, rect.y + 9.0, 18.0, 18.0), theme::TEXT_READING);
            24.0
        } else { 0.0 };
        let run = TtfMeasure.fit_line(&self.text, rect.w - icon, theme::size::MICRO, true);
        Label::new(run.as_ptr(), theme::size::MICRO, theme::TEXT_READING).h(HAlign::Center).bold()
            .draw(p, Rect::new(rect.x + icon, rect.y, rect.w - icon, rect.h));
    }
}

fn video_detail_chips(details: Option<&plx_play_feed::LocalVideoDetails>, movie: bool, width: f32,
    measure: &dyn Measure) -> Vec<VideoDetailChip> {
    use plx_play_feed::VideoRange;
    let Some(details) = details else { return Vec::new(); };
    let dolby = matches!(details.range, Some(VideoRange::DolbyVision | VideoRange::DolbyVisionHdr10));
    let formats = [
        dolby.then_some("Dolby Vision"),
        details.hdr10_plus.then_some("HDR10+"),
        details.hdr10_base.then_some("HDR10"),
        (details.range == Some(VideoRange::Sdr)).then_some("SDR"),
    ];
    let size = details.bytes.map(video_size);
    let duration = details.duration_secs.map(video_duration);
    let resolution = details.resolution.and_then(|(width, height)| resolution_label(width, height, movie));
    let labels = resolution.as_deref().into_iter().map(|text| (text, false)).chain(
        formats.into_iter().flatten().map(|text| (text, true))).chain(
        [size.as_deref(), duration.as_deref()].into_iter().flatten().map(|text| (text, false)));
    let mut chips = Vec::new();
    let (mut x, mut y) = (0.0, 0.0);
    for (text, format) in labels {
        let icon = if format { 24.0 } else { 0.0 };
        let chip_width = measure.width_str(text, theme::size::MICRO, true).ceil() + icon + 22.0;
        if x + chip_width > width && x > 0.0 {
            x = 0.0;
            y += 43.0;
        }
        chips.push(VideoDetailChip { text: text.into(), rect: Rect::new(x, y, chip_width, 35.0), format });
        x += chip_width + 8.0;
    }
    chips
}

#[derive(Clone, Copy)]
enum Glyph { Play, Refresh, Trash, Eject, Drive, Settings, Sun, Sunset, Moon, Star(bool), Shared(Icon) }

fn draw_glyph(p: Painter, glyph: Glyph, rect: Rect, ink: [f32; 4]) {
    let asset = match glyph {
        Glyph::Play => return icons::draw(p, Icon::Play, rect, ink),
        Glyph::Refresh => include_str!("../../../assets/icons/cineusb-refresh.svg"),
        Glyph::Shared(icon) => return icons::draw(p, icon, rect, ink),
        Glyph::Star(false) => include_str!("../../../assets/icons/cineusb-star.svg"),
        Glyph::Star(true) => include_str!("../../../assets/icons/cineusb-star-fill.svg"),
        Glyph::Sun => include_str!("../../../assets/icons/cineusb-sun.svg"),
        Glyph::Sunset => include_str!("../../../assets/icons/cineusb-sunset.svg"),
        Glyph::Moon => include_str!("../../../assets/icons/cineusb-moon.svg"),
        Glyph::Drive => include_str!("../../../assets/icons/cineusb-drive-outline.svg"),
        Glyph::Trash => include_str!("../../../assets/icons/cineusb-trash.svg"),
        Glyph::Eject => include_str!("../../../assets/icons/cineusb-eject.svg"),
        Glyph::Settings => include_str!("../../../assets/icons/cineusb-settings.svg"),
    };
    icons::draw_svg(p, icons::Svg::new(asset), rect, ink);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ButtonTone { Default, Delete, Watched, Unwatched }

fn draw_button(p: Painter, rect: Rect, label: &str, focused: bool, tone: ButtonTone, glyph: Option<Glyph>, accent: [f32; 4]) {
    let (fill, ink) = match tone {
        ButtonTone::Delete => (theme::CONTROL_DELETE_FILL, theme::CONTROL_IDLE_INK),
        ButtonTone::Watched => (theme::CONTROL_WATCHED_FILL, theme::ACCENT_INK),
        ButtonTone::Unwatched => (theme::CONTROL_UNWATCHED_FILL, theme::ACCENT_INK),
        ButtonTone::Default if focused => (accent, theme::ACCENT_INK),
        ButtonTone::Default => (theme::CONTROL_IDLE_FILL, theme::TEXT_PRIMARY),
    };
    widgets::draw_control_face(p, rect, fill, focused, widgets::ControlGround::Keyed);
    if focused && tone != ButtonTone::Default { p.rring(rect, rect.h * 0.5, 3.0, theme::CONTROL_FOCUS_RING); }
    draw_button_content(p, rect, label, glyph, ink);
}

fn draw_button_content(p: Painter, rect: Rect, label: &str, glyph: Option<Glyph>, ink: [f32; 4]) {
    let icon_space = if glyph.is_some() { 39.0 } else { 0.0 };
    let (run, width) = TtfMeasure.fit_line_measured(label, rect.w - 28.0 - icon_space, theme::size::CAPTION, true);
    let x = rect.x + (rect.w - width - icon_space) * 0.5;
    if let Some(glyph) = glyph {
        draw_glyph(p, glyph, Rect::new(x, rect.y + (rect.h - 26.0) * 0.5, 26.0, 26.0), ink);
    }
    Label::new(run.as_ptr(), theme::size::CAPTION, ink).h(HAlign::Center).bold()
        .draw(p, Rect::new(x + icon_space, rect.y, width, rect.h));
}

fn load_settings() -> Result<Option<Settings>, String> {
    #[cfg(not(feature = "hostsim"))]
    let record = plx_platform::tv::usb::preferences().map_err(|error| format!("{error:?}"))?;
    #[cfg(feature = "hostsim")]
    let record = match std::fs::read_to_string(plx_base::paths::in_runtime_dir("cineusb-settings")) {
        Ok(record) => record,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    parse_settings(&record)
}

fn parse_settings(record: &str) -> Result<Option<Settings>, String> {
    if record.is_empty() { return Ok(None); }
    let mut fields = record.split(' ');
    let language = match fields.next() {
        Some("es") => Preference::Es,
        Some("ca") => Preference::Ca,
        Some("en") => Preference::En,
        _ => return Err("invalid settings language".into()),
    };
    let flag = |field| match field {
        Some("0") => Ok(false),
        Some("1") => Ok(true),
        _ => Err("invalid settings flag".to_owned()),
    };
    let forced_subtitles = flag(fields.next())?;
    let spanish_audio = flag(fields.next().or(Some("0")))?;
    let auto_next = flag(fields.next().or(Some("1")))?;
    let choice = |field: Option<&str>| field.unwrap_or("0").parse::<usize>()
        .ok().filter(|value| *value < 3).ok_or_else(|| "invalid settings choice".to_owned());
    let accent = choice(fields.next())?;
    let cards = choice(fields.next())?;
    let sort = choice(fields.next())?;
    let extensions = flag(fields.next().or(Some("0")))?;
    let show_clock = flag(fields.next().or(Some("1")))?;
    let clock_format = choice(fields.next())?;
    let clock_size = choice(fields.next().or(Some("1")))?;
    let convert_dts = flag(fields.next().or(Some("0")))?;
    if fields.next().is_some() { return Err("invalid settings record".into()); }
    Ok(Some(Settings { language, forced_subtitles, spanish_audio, auto_next, accent, cards, sort, extensions, show_clock, clock_format, clock_size, convert_dts }))
}

fn save_settings(settings: Settings) -> Result<(), String> {
    let language = match settings.language { Preference::Ca => "ca", Preference::En => "en", _ => "es" };
    let record = format!("{} {} {} {} {} {} {} {} {} {} {} {}", language,
        u8::from(settings.forced_subtitles), u8::from(settings.spanish_audio), u8::from(settings.auto_next),
        settings.accent, settings.cards, settings.sort, u8::from(settings.extensions), u8::from(settings.show_clock), settings.clock_format, settings.clock_size, u8::from(settings.convert_dts));
    #[cfg(not(feature = "hostsim"))]
    return plx_platform::tv::usb::save_preferences(&record).map_err(|error| format!("{error:?}"));
    #[cfg(feature = "hostsim")]
    plx_plex::plex::session::write_atomic(&plx_base::paths::in_runtime_dir("cineusb-settings"), record.as_bytes())
        .map_err(|error| format!("{error:?}"))
}

fn mutate_usb(action: Mutation, volume: &Volume, path: &std::path::Path) -> Result<(), String> {
    #[cfg(not(feature = "hostsim"))]
    {
        let id = system_mount(volume).ok_or("missing live USB mount")?;
        match action {
            Mutation::Delete => plx_platform::tv::usb::delete_video(path, id).map_err(|error| format!("{error:?}")),
            Mutation::Eject => plx_platform::tv::usb::eject_disk(path, id),
        }
    }
    #[cfg(feature = "hostsim")]
    {
        let _ = (action, volume, path);
        Err("USB actions require webOS".into())
    }
}

fn delete_folder(path: &std::path::Path, mount: u64) -> Result<(), String> {
    #[cfg(not(feature = "hostsim"))]
    return plx_platform::tv::usb::delete_folder(path, mount);
    #[cfg(feature = "hostsim")]
    {
        let _ = (path, mount);
        Err("USB actions require webOS".into())
    }
}

fn system_mount(volume: &Volume) -> Option<u64> {
    volume.mount_id.filter(|_| volume.system)
}

fn scan_volumes() -> Scan {
    #[cfg(not(feature = "hostsim"))]
    match plx_platform::tv::usb::volumes() {
        Ok(mounts) => return Scan {
            volumes: mounts.into_iter().map(|(id, path, label)| Volume {
                path, label, mount_id: Some(id), system: true,
            }).collect(),
            hidden_mounts: false,
            system: true,
        },
        Err(error) => plx_base::eventlog::log(&format!("cineusb: system USB inventory failed: {error:?}")),
    }
    disk::volumes()
}

fn directory_stamps(volume: &Volume, paths: &[PathBuf]) -> io::Result<Vec<(u64, i64, i64)>> {
    #[cfg(not(feature = "hostsim"))]
    if let Some(mount) = system_mount(volume) {
        return plx_platform::tv::usb::folder_stamps(paths, mount).map_err(|error| io::Error::other(format!("{error:?}")));
    }
    use std::os::unix::fs::MetadataExt;
    let _ = volume;
    paths.iter().map(|path| {
        let meta = std::fs::symlink_metadata(path)?;
        if !meta.is_dir() || meta.file_type().is_symlink() { return Err(io::ErrorKind::InvalidData.into()); }
        Ok((meta.ino(), meta.mtime(), meta.mtime_nsec()))
    }).collect()
}

fn volume_facts(volume: &Volume) -> VolumeFacts {
    #[cfg(not(feature = "hostsim"))]
    if let Some(mount) = system_mount(volume) {
        return plx_platform::tv::usb::volume_details(&volume.path, mount).unwrap_or_default();
    }
    #[cfg(feature = "hostsim")]
    return (None, Some(volume.path.to_string_lossy().into_owned()));
    #[cfg(not(feature = "hostsim"))]
    Default::default()
}

fn hex(bytes: impl IntoIterator<Item = u8>) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::new();
    for byte in bytes { let _ = write!(encoded, "{byte:02x}"); }
    encoded
}

fn history_prefix(identity: &str) -> String { hex(identity.bytes().chain([0])) }

fn path_key(identity: &str, volume: &Volume, path: &std::path::Path) -> Option<String> {
    let relative = path.strip_prefix(&volume.path).ok()?;
    Some(hex(identity.bytes().chain([0]).chain(relative.as_os_str().as_encoded_bytes().iter().copied())))
}

fn favorite_key(identity: &str, volume: &Volume, path: &std::path::Path) -> Option<String> {
    let relative = path.strip_prefix(&volume.path).ok()?;
    if relative.components().count() < 1 { return None; }
    Some(format!("{}00", path_key(identity, volume, path)?))
}

fn entry_key(identity: &str, volume: &Volume, path: &std::path::Path, size: u64, mtime: i64) -> Option<String> {
    let base = path_key(identity, volume, path)?;
    Some(format!("{base}{}", hex([0].into_iter().chain(size.to_le_bytes()).chain(mtime.to_le_bytes()))))
}

// The encoded legacy key ends with a NUL, 8-byte length and 8-byte mtime.
// Only ignore mtime when the full volume identity, relative path and length
// match. This keeps existing resume entries readable across driver differences.
fn matching_size_resume(records: &BTreeMap<String, u64>, current: &str) -> Option<(String, u64)> {
    let prefix = current.get(..current.len().checked_sub(16)?)?;
    records.range(prefix.to_owned()..).take_while(|(key, _)| key.starts_with(prefix)).find_map(|(key, value)| {
        (key.len() == current.len() && *value != 0 && *value != WATCHED
            && *value != FAVORITE).then(|| (key.clone(), *value))
    })
}

fn file_record(records: &BTreeMap<String, u64>, identity: &str, volume: &Volume, key: &DetailKey) -> u64 {
    let (path, size, modified) = key;
    if let Some((size, modified)) = size.zip(*modified) {
        if let Some(exact) = entry_key(identity, volume, path, size, modified) {
            if let Some(record) = records.get(&exact) { return *record; }
            if let Some((_, record)) = matching_size_resume(records, &exact) { return record; }
        }
    }
    let Some(base) = path_key(identity, volume, path).map(|base| format!("{base}00")) else { return 0; };
    let mut partial = 0;
    for (_, record) in records.range(base.clone()..).take_while(|(key, _)| key.starts_with(&base)) {
        if *record == WATCHED { return WATCHED; }
        if size.is_none() && *record != FAVORITE { partial = partial.max(*record); }
    }
    partial
}

fn progress_key(identity: &str, volume: &Volume, path: &std::path::Path, file: &File) -> Option<String> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = file.metadata().ok()?;
    entry_key(identity, volume, path, metadata.len(), metadata.mtime())
}

fn decode_relative(encoded: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;
    if encoded.len() % 2 != 0 || !encoded.is_ascii() { return None; }
    let bytes = (0..encoded.len()).step_by(2).map(|i| u8::from_str_radix(&encoded[i..i + 2], 16).ok()).collect::<Option<Vec<_>>>()?;
    if bytes.contains(&0) { return None; }
    let path = PathBuf::from(OsString::from_vec(bytes));
    path.components().all(|component| matches!(component, std::path::Component::Normal(_))).then_some(path)
}

#[cfg(any(test, feature = "hostsim"))]
fn history_matches(key: &str, base: &str) -> bool {
    key.strip_prefix(base).is_some_and(|suffix| suffix.starts_with("00") || suffix.starts_with("2f"))
}

fn clear_history(base: &str) -> Result<(), String> {
    #[cfg(not(feature = "hostsim"))]
    return plx_platform::tv::usb::clear_history(base).map_err(|error| format!("{error:?}"));
    #[cfg(feature = "hostsim")]
    {
        let Some(end) = base.as_bytes().chunks_exact(2).position(|pair| pair == b"00") else { return Ok(()) };
        if let Some(records) = read_history(&base[..2 * (end + 1)]) {
            for key in records.keys().filter(|key| history_matches(key, base)) { resume_record(key, Some(0))?; }
        }
        Ok(())
    }
}

fn read_history(prefix: &str) -> Option<BTreeMap<String, u64>> {
    #[cfg(not(feature = "hostsim"))]
    return plx_platform::tv::usb::history(prefix).ok();
    #[cfg(feature = "hostsim")]
    {
        let mut records = BTreeMap::new();
        let dir = plx_base::paths::in_runtime_dir("cineusb-settings");
        for file in std::fs::read_dir(dir.parent()?).into_iter().flatten().filter_map(Result::ok) {
            if !file.file_name().to_string_lossy().starts_with("cineusb-resume-") { continue; }
            let Ok(text) = std::fs::read_to_string(file.path()) else { continue };
            if let Some((key, value)) = text.split_once(' ') {
                if key.starts_with(prefix) {
                    if let Ok(value) = value.parse() { records.insert(key.to_owned(), value); }
                }
            }
        }
        Some(records)
    }
}

fn resume_record(key: &str, position: Option<u64>) -> Result<Option<u64>, String> {
    #[cfg(not(feature = "hostsim"))]
    return plx_platform::tv::usb::resume(key, position).map_err(|error| format!("{error:?}"));
    #[cfg(feature = "hostsim")]
    {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        let path = plx_base::paths::in_runtime_dir(&format!("cineusb-resume-{:016x}", hasher.finish()));
        if let Some(position) = position {
            if position == 0 {
                match std::fs::remove_file(path) {
                    Ok(()) => return Ok(None),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                    Err(error) => return Err(error.to_string()),
                }
            }
            let record = format!("{key} {position}");
            plx_plex::plex::session::write_atomic(&path, record.as_bytes()).map_err(|error| format!("{error:?}"))?;
            return Ok(None);
        }
        let record = std::fs::read_to_string(path).unwrap_or_default();
        Ok(record.split_once(' ').filter(|(saved, _)| *saved == key).and_then(|(_, seconds)| seconds.parse().ok()))
    }
}

fn list_folder(path: &std::path::Path, _mount_id: Option<u64>) -> io::Result<Vec<Entry>> {
    #[cfg(not(feature = "hostsim"))]
    if let Some(id) = _mount_id {
        let entries = plx_platform::tv::usb::list_folder(path, id)
            .map_err(|error| io::Error::other(format!("Homebrew USB ({error:?})")))?;
        let mut entries: Vec<_> = entries.into_iter().filter_map(|(name, dir, modified, size)| {
            let mut entry = disk::media_entry(name, dir)?;
            entry.modified = modified;
            entry.size = size;
            Some(entry)
        }).collect();
        disk::sort_entries(&mut entries);
        return Ok(entries);
    }
    disk::listing(path)
}

fn inspect_video(path: &std::path::Path, _mount_id: Option<u64>, spanish: bool) -> Result<(File, plx_play_feed::LocalMedia), String> {
    let inspect = |file: File| plx_play_feed::probe_local_with_audio(&file, spanish).map(|info| (file, info));
    #[cfg(not(feature = "hostsim"))]
    if _mount_id.is_some() {
        let file = plx_platform::tv::usb::open_file(path, _mount_id)
            .map_err(|error| format!("No se pudo abrir el vídeo desde Homebrew USB ({error:?})."))?;
        return inspect(file);
    }
    let direct = disk::open_video(path).map_err(|error| format!("No se pudo abrir el vídeo · {error}"))
        .and_then(inspect);
    #[cfg(not(feature = "hostsim"))]
    if direct.is_err() {
        match plx_platform::tv::usb::open_file(path, None) {
            Ok(file) => return inspect(file),
            Err(error) => plx_base::eventlog::log(&format!("cineusb: Homebrew USB descriptor failed: {error:?}")),
        }
    }
    direct
}

fn open_for_details(path: &std::path::Path, _mount_id: Option<u64>) -> Result<File, String> {
    #[cfg(not(feature = "hostsim"))]
    if _mount_id.is_some() {
        return plx_platform::tv::usb::open_file(path, _mount_id).map_err(|error| format!("{error:?}"));
    }
    let direct = disk::open_video(path).map_err(|error| error.to_string());
    #[cfg(not(feature = "hostsim"))]
    if direct.is_err() {
        return plx_platform::tv::usb::open_file(path, None).map_err(|error| format!("{error:?}"));
    }
    direct
}

/// Remove trailing release tags without inventing technical metadata.
/// Use a deliberate container title when supplied, or remove a marked release suffix
/// from the filename. Plain trailing numbers stay part of the title.
fn clean_media_title(stem: &str, metadata: Option<&str>) -> String {
    fn clean(value: &str) -> String {
        const TECH: &[&str] = &[
            "2160p", "1440p", "1080p", "900p", "720p", "576p", "480p", "4k", "uhd", "hdr10+",
            "hdr10plus", "hdr10", "hdr", "hlg", "sdr",
            "dovi", "dolbyvision", "dolby vision", "dv", "bdremux", "remux", "bluray", "blu-ray",
            "web-dl", "webdl", "webrip", "bdrip", "hevc", "h.265", "h.264", "h265", "h264",
            "x265", "x264", "av1", "vp9", "truehd", "dts-hd ma", "dts-hd", "dts:x", "dts",
            "dolby atmos", "atmos", "dolby digital plus", "dolby digital", "eac3", "ac3", "aac", "pcm",
        ];
        let value = value.trim();
        let value = if is_video_path(std::path::Path::new(value)) {
            value.rsplit_once('.').map_or(value, |(stem, _)| stem)
        } else { value };
        let trim = |ch: char| matches!(ch, ' ' | '.' | '_' | '-');
        let lower = value.to_ascii_lowercase();
        fn technical_tail(text: &str) -> Option<&str> {
            TECH.iter().find_map(|tag| {
                let prefix = text.as_bytes().get(..tag.len())?;
                if !prefix.iter().zip(tag.bytes()).all(|(&actual, expected)| {
                    actual == expected || (expected == b' ' && matches!(actual, b'.' | b'_'))
                }) { return None; }
                text.get(tag.len()..).filter(|tail| tail.chars().next().is_none_or(|c| !c.is_alphanumeric()))
            })
        }
        let mut cut = value.len();
        for (index, ch) in lower.char_indices().skip(1) {
            if !matches!(ch, ' ' | '.' | '_' | '-' | '(' | '[') { continue; }
            let rest = &lower[index + ch.len_utf8()..];
            if technical_tail(rest).is_some_and(|suffix| {
                let suffix = suffix.trim();
                let next = suffix.trim_start_matches(|c: char| matches!(c, ' ' | '.' | '_'));
                suffix.is_empty() || suffix.starts_with(['-', ']', ')', '['])
                    || technical_tail(next).is_some() || next.starts_with("5.1") || next.starts_with("7.1")
            }) {
                let prefix = value[..index].trim_end_matches(trim);
                if !prefix.is_empty() {
                    cut = index;
                    break;
                }
            }
        }
        let before = value[..cut].trim_end_matches(trim);
        let year = |part: &str| part.len() == 4 && part.parse::<u16>().is_ok_and(|n| (1888..=2035).contains(&n));
        let explicit = [" - ", " (", " ["].into_iter().find_map(|separator| {
            let (title, suffix) = before.rsplit_once(separator)?;
            let suffix = match separator {
                " (" => suffix.strip_suffix(')')?,
                " [" => suffix.strip_suffix(']')?,
                _ => suffix,
            };
            (year(suffix) && !title.is_empty()).then_some(title)
        });
        let dotted = if cut < value.len() {
            before.rsplit_once('.').and_then(|(title, suffix)| year(suffix).then_some(title))
        } else { None };
        let title = explicit.or(dotted).unwrap_or(before);
        let title = if !title.contains(' ') {
            title.replace(['.', '_'], " ")
        } else { title.to_owned() };
        title.split_whitespace().collect::<Vec<_>>().join(" ")
    }
    let tagged = metadata.filter(|title| title.len() <= 512 && !title.chars().any(char::is_control))
        .map(clean).filter(|title| !title.is_empty());
    tagged.unwrap_or_else(|| clean(stem))
}

fn file_title(stem: &str, metadata: Option<&str>) -> String {
    let clean = clean_media_title(stem, metadata);
    let Some((prefix, _, _, suffix)) = continuity::episode_parts(std::ffi::OsStr::new(&clean)) else { return clean; };
    let trim = |ch: char| matches!(ch, ' ' | '.' | '_' | '-' | '–' | ':' | '[' | ']' | '(' | ')');
    let title = [suffix, prefix].into_iter().find_map(|part| {
        let text = std::str::from_utf8(part).ok()?.trim_matches(trim);
        (!text.is_empty()).then_some(text)
    });
    title.map_or_else(|| tr("Episodio", "Episodi").into(), str::to_owned)
}

fn video_title(path: &std::path::Path, metadata: Option<&str>) -> String {
    let name = path.file_stem().or_else(|| path.file_name()).unwrap_or_default().to_string_lossy();
    let tagged = metadata.filter(|title| !placeholder_title(title) && title.len() <= 512 && !title.chars().any(char::is_control));
    if placeholder_title(&name) && tagged.is_none_or(|title| title.trim().is_empty()) {
        if let Some(parent) = path.parent().and_then(std::path::Path::file_name).and_then(|name| name.to_str())
            .filter(|name| !placeholder_title(name) && !name.bytes().all(|b| b.is_ascii_digit())
                && [1, 2, DOCUMENTALS_TAB].into_iter().all(|kind| media_folder_score(name, kind) == 0)
                && continuity::season_number(name).is_none()) {
            return clean_series_name(parent);
        }
    }
    let title = file_title(&name, tagged);
    if tagged.is_none() && continuity::episode_parts(std::ffi::OsStr::new(name.as_ref())).is_none() {
        clean_series_name(&title)
    } else {
        title
    }
}

fn placeholder_title(title: &str) -> bool {
    let title = title.trim().to_ascii_lowercase();
    ["title_t", "title t", "track_", "track "].into_iter().any(|prefix| {
        let Some(rest) = title.strip_prefix(prefix) else { return false; };
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        digits > 0 && (rest[digits..].trim().is_empty() || rest[digits..].trim_start().starts_with('('))
    })
}

fn home_file_title(path: &std::path::Path, embedded: Option<&str>) -> String {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    if stem.is_empty() || placeholder_title(&stem) {
        return video_title(path, embedded);
    }
    clean_media_title(&stem, None)
}

fn movie_title(path: &std::path::Path, embedded: Option<&str>) -> String {
    let stem = path.file_stem().or_else(|| path.file_name()).unwrap_or_default().to_string_lossy();
    if let Some(title) = movie_title_candidate(&stem) { return title; }
    if let Some(title) = embedded.filter(|title| title.len() <= 512 && !title.chars().any(char::is_control))
        .and_then(movie_title_candidate) {
        return title;
    }
    if let Some(title) = path.parent().and_then(std::path::Path::file_name).and_then(|name| name.to_str())
        .filter(|name| [MOVIES_TAB, SERIES_TAB, DOCUMENTALS_TAB].into_iter().all(|tab| media_folder_score(name, tab) == 0))
        .and_then(movie_title_candidate) {
        return title;
    }
    tr("Película", "Pel·lícula").into()
}

fn movie_title_candidate(raw: &str) -> Option<String> {
    // Release prefixes with several digits and no lowercase letters, such as T3P4K1NG.
    let coded_group = |tag: &str| {
        let tag = tag.trim();
        tag.bytes().filter(u8::is_ascii_digit).count() >= 2
            && tag.bytes().any(|b| b.is_ascii_uppercase())
            && tag.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    };
    let raw = raw.trim();
    let raw = raw.strip_prefix('[').and_then(|value| value.split_once(']'))
        .or_else(|| raw.split_once('-'))
        .filter(|(tag, _)| coded_group(tag))
        .map_or(raw, |(_, title)| title.trim());
    let lower = raw.to_ascii_lowercase();
    let cut = lower.char_indices().find_map(|(index, separator)| {
        if !matches!(separator, ' ' | '.' | '_' | '-' | '[' | '(') { return None; }
        let tail = &lower[index + 1..];
        let resolution = ["2160p", "1440p", "1080p", "720p", "576p", "480p"].into_iter()
            .find_map(|token| tail.strip_prefix(token));
        // A release tag often follows the resolution after a dot, even when it has no known codec name.
        if resolution.is_some_and(|rest| rest.starts_with(['.', '_'])) && index > 0 {
            return Some(index);
        }
        if separator == '-' && raw[..index].ends_with(" m")
            && resolution.is_some_and(|rest| rest.is_empty() || !rest.starts_with(char::is_alphanumeric)) {
            return Some(index - 2);
        }
        None
    });
    let value = cut.map_or(raw, |index| {
        let prefix = raw[..index].trim_end_matches(' ');
        let prefix = if raw[index..].starts_with('-') { prefix.strip_suffix(" m").unwrap_or(prefix) } else { prefix };
        prefix.rsplit_once('.').filter(|(name, year)| !name.is_empty() && year.len() == 4
            && year.parse::<u16>().is_ok_and(|year| (1888..=2035).contains(&year)))
            .map_or(prefix, |(name, _)| name)
    });
    let title = clean_media_title(value.trim_end_matches([' ', '.', '_', '-', '[', '(']), None);
    let lower = title.to_ascii_lowercase();
    let generic = placeholder_title(&title) || matches!(lower.as_str(), "" | "video" | "movie" | "película" | "pel·lícula"
        | "2160p" | "1440p" | "1080p" | "720p" | "576p" | "480p");
    let credit = ["montado por ", "montaje de ", "encode by ", "encoded by ", "ripped by ", "muxed by "]
        .into_iter().any(|prefix| lower.starts_with(prefix));
    (!generic && !credit).then_some(title)
}

fn clean_series_name(raw: &str) -> String {
    let raw = raw.trim();
    let bytes = raw.as_bytes();
    let numbered = bytes.len() > 3 && bytes[0] == b'0' && bytes[1].is_ascii_digit()
        && matches!(bytes[2], b' ' | b'.' | b'_' | b'-');
    let title = if numbered { raw[2..].trim_start_matches([' ', '.', '_', '-']) } else { raw };
    clean_media_title(title, None)
}

/// The player handoff is scoped to the current physical Feed snapshot.
pub(super) fn hud_formats(ps: &plx_media::route::PlaybackSession)
    -> Option<plx_appkit::player_hud::model::LocalFormats> {
    if !plx_media::route::is_local_file(ps) { return None; }
    let declared_dv = plx_media::route::stream_dv_decision(ps).presentation.declared().is_some();
    Some(formats_from_feed(ps.readout().raw().diag, declared_dv))
}

fn formats_from_feed(feed: plx_playback_contract::feed::FeedDiag, declared_dv: bool)
    -> plx_appkit::player_hud::model::LocalFormats {
    use plx_appkit::player_hud::model::LocalFormats;
    let video = if feed.load_completed && feed.fed_v > 0 {
        plx_playback_contract::feed::VideoFormat::label(feed.video_format, declared_dv)
    } else { None };
    let audio = if feed.load_completed && feed.fed_a > 0 {
        match feed.load_a {
            1 => Some("Dolby Digital"),
            2 => Some("Dolby Digital Plus"),
            3 => Some("AAC"),
            4 => Some("DTS"),
            5 => Some("PCM"),
            6 => Some("Opus"),
            _ => None,
        }
    } else { None };
    let channels = audio.and_then(|_| {
        if feed.dts_ac3 { return Some("5.1"); }
        let count = feed.dts_pcm.map_or(feed.decoded_audio_channels, |pcm| pcm.output.channels as u8);
        match count {
            1 => Some("1.0"),
            2 => Some("2.0"),
            3 => Some("3 ch"),
            4 => Some("4 ch"),
            5 => Some("5 ch"),
            6 => Some("6 ch"),
            7 => Some("7 ch"),
            8 => Some("8 ch"),
            _ => None,
        }
    });
    LocalFormats { video, audio, channels }
}

fn display_name(entry: &Entry, extensions: bool) -> String {
    let name = std::path::Path::new(&entry.name);
    match entry.kind {
        Kind::Folder => entry.name.to_string_lossy().into_owned(),
        Kind::Video if extensions => entry.name.to_string_lossy().into_owned(),
        Kind::Video => video_title(name, None),
    }
}

fn volume_name(volume: &Volume) -> String {
    if let Some(label) = &volume.label { return label.to_string_lossy().into_owned(); }
    let name = volume.path.file_name().unwrap_or_default().to_string_lossy();
    name.strip_prefix("usb-").and_then(|name| name.split_once('-'))
        .map_or(name.as_ref(), |(_, label)| label).to_owned()
}

fn draw_line(p: Painter, line: &str, frame: Rect, size: i32, strong: bool) {
    let (line, _) = TtfMeasure.fit_line_measured(line, frame.w, size, strong);
    let ink = if strong { theme::TEXT_PRIMARY } else { theme::TEXT_SECONDARY };
    let label = Label::new(line.as_ptr(), size, ink).h(HAlign::Left);
    if strong { label.bold().draw(p, frame); } else { label.draw(p, frame); }
}

pub(super) fn click(app: &mut App, x: f32, y: f32, multi_click: bool) {
    // The first press may have opened a video dialog beneath the pointer.
    if multi_click && matches!(app.usb.dialog, Some(Dialog::Video { .. })) {
        return;
    }
    app.usb.click(x, y);
    plx_machine::idle::invalidate();
}

pub(super) fn pointer(app: &mut App, x: f32, y: f32) {
    if app.usb.pointer(x, y) {
        plx_machine::idle::invalidate();
    }
}

pub(super) fn key(app: &mut App, key: Key, repeat: bool) {
    if key == Key::Exit { app.running = false; }
    else if !super::bridge::player_arriving(&app.pages) { app.usb.key(key, repeat); }
    plx_machine::idle::invalidate();
}

/// Build the same track inventory that the Plex path installs for its in-player menu,
/// directly from the already-probed USB file. In particular, subtitle order here MUST
/// equal FFmpeg's SubTracks::inspect file-order enumeration (not an alphabetic sort).
fn local_playing_item(info: &plx_play_feed::LocalMedia, dovi: plx_data::metadata::Dovi)
    -> plx_data::metadata::PlayingItem {
    use plx_data::metadata::{PlayingItem, Stream};

    let language = |raw: Option<&str>| -> (String, String) {
        let code = raw.unwrap_or("").trim().to_ascii_lowercase();
        if code.is_empty() || code == "und" { return (String::new(), String::new()); }
        let label = plx_data::metadata::lang_key(&code)
            .map(|key| plx_plex::plex::languages::label(&key).to_owned())
            .unwrap_or_default();
        (code, label)
    };
    // Include all audio stream ordinals so the player's track menu addresses the same stream as FFmpeg.
    let audio = info.audio_tracks.iter().map(|track| {
        let (lang_code, lang) = language(track.language.as_deref());
        Stream {
            id: i64::from(track.index) + 1,
            index: i64::from(track.index),
            lang, lang_code, codec: track.codec.clone(), title: track.title.clone(),
            channels: track.channels, bit_depth: track.depth, sampling_rate: track.rate,
            default: track.default, selected: track.index == info.audio_stream_index,
            ..Stream::default()
        }
    }).collect();
    let subs = info.subtitles.iter().map(|track| {
        let (lang_code, lang) = language(track.language.as_deref());
        Stream {
            // Stream.id==0 denotes Off in the existing menu. Preserve an unambiguous
            // nonzero id for every real track, even when its FFmpeg stream index is zero.
            id: i64::from(track.stream_index) + 1,
            index: i64::from(track.stream_index),
            lang, lang_code, codec: track.codec.clone(), title: track.title.clone(),
            forced: track.forced, default: track.default, sdh: track.sdh,
            ..Stream::default()
        }
    }).collect();
    PlayingItem {
        // A local file has no Plex server or rating key. Empty rk and part-id 0 prevent
        // the ordinary subtitle-selection path from issuing Plex PUT requests.
        sid: plx_plex::plex::ServerId::from_raw(0),
        rk: String::new(), show_rk: String::new(), kind: String::new(),
        duration_ms: info.duration_secs.and_then(|seconds| seconds.checked_mul(1000)), audio, subs, video_fps: info.fps,
        width: i64::from(info.width), height: i64::from(info.height), bitrate: 0,
        dovi, markers: Vec::new(), chapters: Vec::new(), hdr: dovi.present,
    }
}

fn initial_forced_subtitle(item: &plx_data::metadata::PlayingItem, enabled: bool) -> Option<(i64, i32)> {
    if !enabled { return None; }
    let audio_lang = item.audio.iter().find(|audio| audio.selected).map_or("", |audio| audio.lang_code.as_str());
    let names = plx_data::metadata::track_names::TrackNames::new();
    let picked = plx_media::route::forced_only_pick(&item.subs, &names, audio_lang, "");
    Some(picked.map_or((0, -1), |index| {
        (item.subs[index].id, plx_data::metadata::sub_render_ordinal(&item.subs, index))
    }))
}

pub(super) fn tick(app: &mut App) {
    if !app.window_activity.allow_present(true) && matches!(app.usb.dialog, Some(Dialog::Next { .. })) {
        app.usb.dialog = None;
    }
    let Some(PreparedVideo { file, info, title, volume, path, key, resume, saved, .. }) = app.usb.update() else { return };
    if !app.usb.scan.volumes.contains(&volume) { return; }
    let ps = &mut app.player.session;
    let dovi = info.dovi.map_or(plx_data::metadata::Dovi::NONE, |record| plx_data::metadata::Dovi {
        present: record.dv_profile > 0, profile: i64::from(record.dv_profile),
        bl_compat: i64::from(record.dv_bl_signal_compatibility_id), el_present: record.el_present_flag != 0,
        ..plx_data::metadata::Dovi::NONE
    });
    let audio = (!info.audio.is_empty()).then(|| plx_media::route::CarriedAudio {
        // Same local-only id as PlayingItem.audio: the existing subtitle menu
        // matches this sid to determine the playing audio's language for Forced only.
        sid: i64::from(info.audio_stream_index) + 1,
        ordinal: info.audio_tracks.iter().position(|track| track.index == info.audio_stream_index).map_or(-1, |i| i as i32),
        codec: info.audio.clone(), channels: info.audio_channels,
        can_normalize_loudness: false, immersive: false, sample_rate: info.audio_rate,
        bit_depth: info.audio_depth,
    });
    let item = local_playing_item(&info, dovi);
    let forced = initial_forced_subtitle(&item, app.usb.settings.forced_subtitles);
    let ok = plx_media::route::set_stream_declaration(ps, &info.video, &info.audio, info.fps, dovi, false)
        && plx_media::route::set_local_source(ps, file, &title, audio, forced, &item, playback::start_caps(), app.usb.settings.convert_dts);
    if ok {
        plx_media::route::set_stream_source_raster(ps, info.width, info.height);
        if playback::start_playback(ps, &mut app.adapters.player, resume.min(i64::MAX as u64) as i64, playback::Origin::Here,
            4500, None, &mut app.pages, &mut app.bridge) {
            app.usb.playing = Some(PlayingVideo { volume, path, key, position: resume, duration: info.duration_secs.map(|seconds| seconds.saturating_mul(1_000_000_000)),
                saved, next_save: Instant::now() + Duration::from_secs(30), seen_player: false, observed_playhead: false, paused: false });
            app.bridge.metadata_run(plx_data::stores::metadata::MetadataCmd::InstallPlaying(
                Some(item)));
            return;
        }
        plx_media::route::clear_url(ps);
    }
    app.usb.status = tr("No se pudo iniciar el reproductor con este vídeo.", "No s’ha pogut reproduir aquest vídeo.").into();
}

fn remaining_wall_seconds(position: i64, duration: i64, rate: i32) -> Option<u64> {
    if position < 0 || duration <= position || rate <= 0 { return None; }
    Some((duration - position).cast_unsigned().div_ceil(u64::from(rate as u32) * 1_000_000))
}

pub(super) fn finish_clock(app: &App) -> Option<plx_appkit::player_hud::model::FinishClock> {
    app.usb.estimate.map(|(_, clock)| clock)
}

pub(super) fn observe_playback(app: &mut App) {
    let Some(playing) = &mut app.usb.playing else { return };
    if super::bridge::player(&app.pages).is_some() {
        playing.seen_player = true;
        if let Some(position) = plx_media::player::observed_position_ns(&app.player.session)
            .filter(|position| *position > 0 || playing.observed_playhead) {
            playing.observed_playhead = true;
            playing.position = position.max(0) as u64;
        }
        if let Some(duration) = plx_media::player::duration_ns(&app.player.session).filter(|duration| *duration > 0) {
            playing.duration = Some(duration as u64);
        }
        let paused = plx_media::player::transport::viewer_paused(&app.player.session);
        let just_paused = paused && !playing.paused;
        let resumed = !paused && playing.paused;
        playing.paused = paused;
        let now = Instant::now();
        let position = plx_media::player::intended_pos_ns(&app.player.session);
        let rate = plx_media::player::applied_rate(&app.player.session).unwrap_or(1000);
        let seeking = plx_media::player::seek_pending(&app.player.session).is_some();
        let anchor = (paused || seeking).then_some((position, rate));
        let anchor_changed = anchor.is_some() && anchor != app.usb.estimate_anchor;
        app.usb.estimate_anchor = anchor;
        if resumed || just_paused || anchor_changed || app.usb.next_estimate.is_none_or(|next| now >= next) {
            app.usb.next_estimate = Some(now + Duration::from_secs(1));
            if !paused || just_paused || anchor_changed || app.usb.estimate.is_none() {
                let estimate = plx_media::player::duration_ns(&app.player.session).and_then(|duration|
                    remaining_wall_seconds(position, duration, rate))
                    .and_then(i18n::local_finish_time);
                if app.usb.estimate.as_ref().map(|(minute, _)| *minute) != estimate.map(|(minute, _, _)| minute) {
                    app.usb.estimate = estimate.map(|(minute, hour, min)| {
                        (minute, plx_appkit::player_hud::model::FinishClock {
                            hour, minute: min,
                            twelve_hour: i18n::current().clock() == i18n::Clock::H12,
                            prefix: tr("Termina a las", "Acaba a les"),
                            remaining: tr("Quedan", "Queden"),
                        })
                    });
                    plx_machine::idle::invalidate();
                }
            }
        }
        app.usb.store_playhead(false, just_paused);
    } else if playing.seen_player {
        app.usb.finish_video(false);
    }
}

pub(super) fn playing_local(app: &App) -> bool {
    app.usb.playing.is_some()
}

pub(super) fn playback_completed(app: &mut App) {
    app.usb.finish_video(true);
}

pub(super) fn flush_playback(app: &mut App) {
    observe_playback(app);
    app.usb.finish_video(false);
    app.usb.flush_progress(true);
}

#[cfg(test)]
mod usb_browser_tests {
    use super::*;

    #[test]
    fn demo_restores_the_real_browser_and_does_not_queue_history_writes() {
        let mut browser = Browser::new();
        let real = Volume { path: "/tmp/usb/test-real".into(), label: None, mount_id: None, system: false };
        browser.scan.volumes.push(real.clone());
        browser.settings.cards = 2;
        browser.enter_demo(demo::Library::create().unwrap());
        let simulated = browser.scan.volumes[0].clone();
        assert_ne!(simulated.path, real.path);
        let key = favorite_key(demo::IDENTITY, &simulated, &simulated.path.join("Series/Prueba")).unwrap();
        browser.change_history(key.clone(), FAVORITE);
        assert_eq!(browser.history_value(&key), Some(FAVORITE));
        assert!(browser.queued_progress.is_empty());
        browser.settings.cards = 0;
        browser.toggle_demo();
        assert_eq!(browser.scan.volumes, vec![real]);
        assert_eq!(browser.settings.cards, 2);
        assert_eq!(browser.history_value(&key), None);
    }

    #[test]
    fn clock_modes_follow_tv_preference_and_keep_midnight_and_noon_correct() {
        let mut settings = Browser::new().settings;
        assert!(settings.show_clock);
        assert_eq!(clock_label(Some((0, 5)), &settings, i18n::Clock::H12).as_deref(), Some("12:05 AM"));
        assert_eq!(clock_label(Some((12, 5)), &settings, i18n::Clock::H12).as_deref(), Some("12:05 PM"));
        assert_eq!(clock_label(Some((23, 59)), &settings, i18n::Clock::H24).as_deref(), Some("23:59"));
        settings.clock_format = 1;
        assert_eq!(clock_label(Some((23, 59)), &settings, i18n::Clock::H24).as_deref(), Some("11:59 PM"));
        settings.clock_format = 2;
        assert_eq!(clock_label(Some((0, 5)), &settings, i18n::Clock::H12).as_deref(), Some("00:05"));
        settings.show_clock = false;
        assert_eq!(clock_label(Some((12, 0)), &settings, i18n::Clock::H12), None);
        assert!(NAV_PILL.x + NAV_PILL.w < 1690.0);
    }

    #[test]
    fn clock_preferences_restore_old_defaults_and_saved_choices() {
        for record in ["ca 0", "ca 0 1", "ca 0 1 1 2 1 0", "ca 0 1 1 2 1 0 1"] {
            let settings = parse_settings(record).unwrap().unwrap();
            assert!(settings.show_clock);
            assert_eq!(settings.clock_format, 0);
        }
        let settings = parse_settings("ca 0 1 1 2 1 0 1 0 2").unwrap().unwrap();
        assert!(!settings.show_clock);
        assert_eq!(settings.clock_format, 2);
        assert!(parse_settings("ca 0 1 1 2 1 0 1 1 3").is_err());
    }

    #[test]
    fn experimental_dts_setting_defaults_to_off_and_restores_saved_value() {
        assert!(!Browser::new().settings.convert_dts);
        assert!(!parse_settings("es 0 0 1 0 0 0 0 1 0 1").unwrap().unwrap().convert_dts);
        assert!(parse_settings("es 0 0 1 0 0 0 0 1 0 1 1").unwrap().unwrap().convert_dts);
        assert!(parse_settings("es 0 0 1 0 0 0 0 1 0 1 2").is_err());
    }

    #[test]
    fn closing_settings_does_not_cancel_favorites_waiting_for_history() {
        let mut browser = Browser::new();
        let volume = Volume { path: "/tmp/usb/favorites-settings".into(), label: None, mount_id: None, system: false };
        browser.scan.volumes.push(volume.clone());
        browser.open_nav_tab(FAVORITES_TAB);
        assert!(browser.loading_favorites);
        browser.open_nav_tab(SETTINGS_TAB);
        browser.back();
        assert!(browser.loading_favorites);
        assert_eq!(browser.volume(), Some(&volume));
        assert!(browser.favorite_cards().is_some());
        assert_eq!(browser.len(), 0);
    }

    #[test]
    fn three_clock_sizes_preserve_original_small_and_fit_outside_navigation() {
        assert_eq!(clock_font_size(0), theme::size::MICRO);
        assert!(clock_font_size(0) < clock_font_size(1));
        assert!(clock_font_size(1) < clock_font_size(2));
        assert!(clock_font_size(2) as f32 <= CLOCK_RECT.h);
        assert!(CLOCK_RECT.x > NAV_PILL.x + NAV_PILL.w);
        assert!(CLOCK_RECT.x + CLOCK_RECT.w <= 1920.0 - plx_ui::consts::MARGIN_X);
        for size in 0..=2 {
            let record = format!("ca 0 1 1 2 1 0 1 1 2 {size}");
            assert_eq!(parse_settings(&record).unwrap().unwrap().clock_size, size);
        }
        assert_eq!(parse_settings("ca 0").unwrap().unwrap().clock_size, 1);
        assert_eq!(parse_settings("ca 0 1 1 2 1 0 1 0 2").unwrap().unwrap().clock_size, 1);
        assert!(parse_settings("ca 0 1 1 2 1 0 1 1 2 3").is_err());
        assert!(unified_setting_rect(11).y + unified_setting_rect(11).h <= 980.0);
    }

    #[test]
    fn favorites_listing_is_not_restarted_while_its_reply_is_pending() {
        let mut browser = Browser::new();
        let volume = Volume { path: "/tmp/usb/favorites-pending".into(), label: None, mount_id: None, system: false };
        browser.scan.volumes.push(volume.clone());
        browser.enter_favorites(volume.clone(), vec![]);
        browser.loading_favorites = true;
        browser.facts = Some((volume, (None, None)));
        let (send, reply) = mpsc::sync_channel(1);
        browser.pending = Some(reply);
        browser.settings_started = true;
        browser.next_scan = Instant::now() + Duration::from_secs(60);
        browser.home_refresh = browser.next_scan;
        browser.update();
        assert!(browser.loading_favorites);
        assert_eq!(browser.len(), 0);
        assert!(browser.retired_reads.is_empty());
        assert!(send.send(DiskResult::Favorites(browser.volume().unwrap().clone(), vec![])).is_ok(),
            "the original listing must retain its receiver");
        browser.update();
        assert!(!browser.loading_favorites);
        assert!(browser.pending.is_none());
        assert!(browser.favorite_cards().is_some());
    }

    #[test]
    fn home_shows_cached_episode_before_full_inventory_and_discards_disconnected_preview() {
        let mut browser = Browser::new();
        let volume = Volume { path: "/tmp/usb/preview".into(), label: None, mount_id: None, system: false };
        browser.scan.volumes.push(volume.clone());
        let path = volume.path.join("Season 1/Episode 2.mkv");
        let item = home::Item { volume: volume.clone(), path: path.clone(), title: "Serie".into(),
            episode: Some((1, 2)), season_remaining: None, show: true, favorite: true, progress: 0,
            detail_key: (path.clone(), None, None) };
        let (preview_send, preview) = mpsc::channel();
        let (_send, reply) = mpsc::sync_channel(1);
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        browser.home_job = Some(HomeRequest { preview: Some(preview), reply, cancelled: cancelled.clone(),
            volumes: vec![volume], manual: false });

        preview_send.send(vec![item.clone()]).unwrap();
        assert!(browser.update_home(Instant::now()));
        assert!(browser.home_job.is_some(), "the full inventory is still pending");
        assert_eq!(browser.dashboard.items[0].path, path);
        assert_eq!(browser.dashboard.items[0].episode, Some((1, 2)));

        preview_send.send(vec![item]).unwrap();
        browser.scan.volumes.clear();
        browser.update_home(Instant::now());
        assert!(cancelled.load(std::sync::atomic::Ordering::Relaxed));
        assert!(browser.dashboard.items.is_empty());
    }

    #[test]
    fn manual_refresh_is_single_and_cancellation_preserves_library_and_history() {
        let mut browser = Browser::new();
        let volume = Volume { path: "/tmp/usb/sda1".into(), label: None, mount_id: None, system: false };
        browser.scan.volumes.push(volume.clone());
        browser.history.insert("keep".into(), BTreeMap::from([("deleted-episode".into(), WATCHED)]));
        let path = volume.path.join("old.mkv");
        browser.dashboard.items.push(home::Item { volume: volume.clone(), path: path.clone(), title: "Keep".into(),
            episode: None, season_remaining: None, show: false, favorite: true, progress: 0, detail_key: (path, None, None) });
        let (send, reply) = mpsc::sync_channel(1);
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        browser.home_job = Some(HomeRequest { preview: None, reply, cancelled: cancelled.clone(), volumes: vec![volume.clone()], manual: false });
        browser.request_manual_refresh();
        browser.request_manual_refresh();
        assert_eq!(browser.manual_refresh, Some(volume.clone()));
        assert!(cancelled.load(std::sync::atomic::Ordering::Relaxed));
        assert!(browser.home_force_refresh);
        // A manual worker failure must retain the previous valid dashboard.
        browser.home_job.as_mut().unwrap().manual = true;
        cancelled.store(false, std::sync::atomic::Ordering::Relaxed);
        send.send(Err(io::ErrorKind::PermissionDenied.into())).unwrap();
        assert!(browser.update_home(Instant::now()));
        assert!(browser.manual_refresh.is_none());
        assert_eq!(browser.dashboard.items[0].title, "Keep");
        assert_eq!(browser.history["keep"]["deleted-episode"], WATCHED);
        assert!(browser.refresh_notice.is_some());
        browser.manual_refresh = Some(volume);
        browser.scan.volumes.clear();
        browser.update_home(Instant::now());
        assert!(browser.manual_refresh.is_none());
        assert!(browser.refresh_notice.is_some());
        assert_eq!(browser.history["keep"]["deleted-episode"], WATCHED);
    }

    #[test]
    fn successful_manual_refresh_publishes_entries_and_preserves_focused_filename() {
        let mut browser = Browser::new();
        let volume = Volume { path: "/tmp/usb/manual".into(), label: None, mount_id: None, system: false };
        browser.scan.volumes.push(volume.clone());
        let entry = |name: &str| disk::media_entry(name.into(), false).unwrap();
        browser.folders.push(Folder { volume: volume.clone(), path: volume.path.clone(), entries: vec![entry("Keep.mkv"), entry("Deleted.mkv")], collection: None, selected: 0, scroll: 0.0 });
        browser.manual_refresh = Some(volume.clone());
        let (send, reply) = mpsc::sync_channel(1);
        browser.home_job = Some(HomeRequest { preview: None, reply, cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)), volumes: vec![volume.clone()], manual: true });
        send.send(Ok(home::Dashboard {
            drives: vec![home::Drive { volume: volume.clone(), facts: (None, None), stats: Some(Default::default()), videos: vec![], history: None }],
            items: vec![], refreshed_folder: Some((volume.clone(), volume.path.clone(), vec![entry("New.mkv"), entry("Keep.mkv")])),
        })).unwrap();
        assert!(browser.update_home(Instant::now()));
        let entries = &browser.folders.last().unwrap().entries;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[browser.selected].name, OsString::from("Keep.mkv"));
        assert!(!entries.iter().any(|entry| entry.name == "Deleted.mkv"));
        assert_eq!(browser.dashboard.drives.len(), 1);
        assert!(browser.manual_refresh.is_none());
        assert!(browser.refresh_notice.is_some());
    }

    #[test]
    fn settings_new_rows_follow_visual_order_without_overlap() {
        let mut browser = Browser::new();
        browser.open_nav_tab(SETTINGS_TAB);
        browser.option = 1;
        browser.key(Key::Down, false);
        assert_eq!(browser.option, 8);
        browser.key(Key::Down, false);
        assert_eq!(browser.option, 2);
        for pair in SETTINGS_ORDER.windows(2) {
            let first = unified_setting_rect(pair[0]);
            let next = unified_setting_rect(pair[1]);
            assert!(first.y + first.h <= next.y);
        }
        assert!(unified_setting_rect(10).y + unified_setting_rect(10).h < 980.0);
    }

    #[test]
    fn home_keeps_fixed_card_sizes_for_zero_through_four_recommendations() {
        let mut browser = Browser::new();
        let volume = Volume { path: "/tmp/usb/sda1".into(), label: None, mount_id: None, system: false };
        browser.scan.volumes.push(volume.clone());
        for count in 0..=4 {
            browser.dashboard.items = (0..count).map(|episode| {
                let path = volume.path.join(format!("Show S01E{episode:02}.mkv"));
                home::Item { volume: volume.clone(), path: path.clone(), title: "Show".into(),
                    episode: Some((1, episode)), season_remaining: None, show: true, favorite: true, progress: 0,
                    detail_key: (path, None, None) }
            }).collect();
            assert_eq!(browser.len(), 1 + count as usize + usize::from(count > 3));
            for index in 1..=count.min(3) as usize {
                let tile = browser.home_rect_measured(index, &plx_base::fontcov::advances::ShippedMeasure);
                assert_eq!((tile.w, tile.h), (HOME_CARD_W, HOME_CARD_H));
                assert!(tile.x + tile.w < browser.home_rect(0).x);
            }
            assert_eq!(browser.home_rect(0).y + browser.home_rect(0).h, HOME_DISK_BOTTOM);
        }
    }

    #[test]
    fn home_recommendations_share_pointer_and_dpad_geometry() {
        let mut browser = Browser::new();
        let volume = Volume { path: "/tmp/usb/sda1".into(), label: None, mount_id: None, system: false };
        browser.scan.volumes.push(volume.clone());
        browser.dashboard.items = (1..=5).map(|episode| {
            let path = volume.path.join(format!("Show S02E{episode:02}.mkv"));
            home::Item { volume: volume.clone(), path: path.clone(), title: "Una sèrie".into(),
                episode: Some((2, episode)), season_remaining: None, show: true, favorite: true, progress: 0, detail_key: (path, None, None) }
        }).collect();
        assert_eq!(browser.len(), 7);
        browser.key(Key::Left { alt: false }, false);
        assert_eq!(browser.selected, 1);
        assert_eq!(browser.home_item().unwrap().episode, Some((2, 1)));
        browser.key(Key::Right { alt: false }, false);
        assert_eq!(browser.selected, 2);
        let target = browser.tile_rect(3);
        assert!(browser.pointer(target.x + 30.0, target.y + 30.0));
        assert_eq!(browser.selected, 3);
        browser.key(Key::Up, false);
        assert_eq!(browser.selected, browser.home_more_index());
        assert!(browser.pending.is_none());
        assert!(browser.dialog.is_none());
        assert_eq!(browser.home_rect(1).x, plx_ui::consts::MARGIN_X);
        assert_eq!(browser.home_rect(4).w, 0.0);
        assert_eq!(browser.home_rect(0).y, HOME_HEADER_TOP);
        browser.open();
        assert!(browser.home_all);
        assert_eq!(browser.home_rect(4).w, HOME_CARD_W);
        browser.back();
        assert!(!browser.home_all);
    }

    #[test]
    fn safe_eject_waits_for_the_home_inventory_worker() {
        let mut browser = Browser::new();
        let volume = Volume { path: "/tmp/usb/sda1".into(), label: None, mount_id: None, system: false };
        browser.scan.volumes.push(volume.clone());
        let (send, reply) = mpsc::sync_channel(1);
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        browser.home_job = Some(HomeRequest { preview: None, reply, cancelled: cancelled.clone(), volumes: vec![volume.clone()], manual: true });
        browser.manual_refresh = Some(volume.clone());
        browser.dialog = Some(Dialog::Eject(volume));
        browser.option = 1;
        browser.choose_option();
        assert!(cancelled.load(std::sync::atomic::Ordering::Relaxed));
        assert!(!browser.usb_reads_drained());
        send.send(Err(io::ErrorKind::Interrupted.into())).unwrap();
        browser.update_home(Instant::now());
        assert!(browser.home_job.is_none());
        assert!(browser.usb_reads_drained());
        assert!(browser.pending.is_none());
    }

    #[test]
    fn eject_keeps_inflight_probe_tracked_until_its_file_is_released() {
        let mut browser = Browser::new();
        let volume = Volume { path: "/tmp/usb/sda1".into(), label: None, mount_id: None, system: false };
        browser.scan.volumes.push(volume.clone());
        let (send, reply) = mpsc::sync_channel(1);
        browser.details_job = Some(DetailsRequest {
            volume: volume.clone(), key: (volume.path.join("video.mkv"), None, None), reply,
        });
        browser.dialog = Some(Dialog::Eject(volume));
        browser.option = 1;
        browser.choose_option();
        assert!(browser.mutating);
        assert!(browser.pending.is_none());
        assert!(!browser.usb_reads_drained());
        assert!(!browser.update_video_details());
        send.send(Default::default()).unwrap();
        assert!(browser.update_video_details());
        assert!(browser.details_job.is_none());
        assert!(browser.usb_reads_drained());
    }

    #[test]
    fn home_pointer_opens_the_exact_drive_and_ignores_space_between_drives() {
        let root = plx_base::testscratch::process_dir("cineusb-pointer-home");
        let first = root.join("unit-a");
        let second = root.join("unit-b");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let mut browser = Browser::new();
        for path in [first, second.clone()] {
            browser.scan.volumes.push(Volume { path, label: None, mount_id: None, system: false });
        }
        let a = browser.tile_rect(0);
        let b = browser.tile_rect(1);
        assert!(browser.pointer(b.x + 20.0, b.y + 20.0));
        assert_eq!(browser.selected, 1);
        browser.click(a.x + a.w + 1.0, a.y + 20.0);
        assert!(browser.pending.is_none());
        browser.click(b.x + 20.0, b.y + 20.0);
        let result = browser.pending.take().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(result, DiskResult::Folder(_, path, Ok(_)) if path == second));
        plx_base::testscratch::remove_tree(&root);
    }

    #[test]
    fn home_pointer_opens_nested_folders_and_the_episode_under_it() {
        let root = plx_base::testscratch::process_dir("cineusb-home-nested-pointer");
        let show = root.join("Biblioteca/Sèries/Una sèrie");
        let season = show.join("Temporada 1");
        let episode = season.join("Una sèrie S01E01.mkv");
        std::fs::create_dir_all(&season).unwrap();
        std::fs::write(&episode, []).unwrap();
        let volume = Volume { path: root.clone(), label: None, mount_id: None, system: false };
        let mut browser = Browser::new();
        browser.folders.push(Folder {
            volume, path: show.clone(), entries: disk::listing(&show).unwrap(),
            collection: Some(Cover::Series), selected: 0, scroll: 0.0,
        });
        let card = browser.tile_rect(0);
        browser.click(card.x + 20.0, card.y + 20.0);
        let result = browser.pending.take().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
        let DiskResult::Folder(volume, path, Ok(entries)) = result else { panic!("expected season folder") };
        assert_eq!(path, season);
        browser.folders.push(Folder { volume, path, entries, collection: Some(Cover::Series), selected: 0, scroll: 0.0 });
        let card = browser.tile_rect(0);
        browser.click(card.x + 20.0, card.y + 20.0);
        assert!(matches!(browser.dialog, Some(Dialog::Video { path, .. }) if path == episode));
        plx_base::testscratch::remove_tree(&root);
    }

    #[test]
    fn series_counts_expire_and_an_invalidated_request_cannot_restore_stale_counts() {
        let mut browser = Browser::new();
        let volume = Volume { path: "/tmp/usb/sda1".into(), label: None, mount_id: None, system: false };
        let path = volume.path.join("Sèries/Una sèrie");
        let now = Instant::now();
        let listing = series::Listing { counts: series::Counts { seasons: 2, episodes: 13 }, episodes: vec![], identities: BTreeMap::new() };
        let entry = SeriesCacheEntry { checked: now, modified: Some(1), known: true, summary: None };
        let mut card = SeriesCard { volume: &volume, path: path.clone(), modified: Some(1), known: true };
        assert!(entry.fresh(&card, now));
        assert!(!entry.fresh(&card, now + SERIES_REFRESH));
        card.modified = Some(2);
        assert!(!entry.fresh(&card, now));
        browser.scan.volumes.push(volume.clone());
        browser.series_cache.insert(path.clone(), entry);
        let (send, reply) = mpsc::sync_channel(1);
        browser.series_job = Some(SeriesRequest { volume, path, modified: Some(1), known: true, invalidated: false,
            cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)), reply });
        browser.invalidate_series_counts();
        send.send(Some(listing)).unwrap();
        browser.update_series_counts(now);
        assert!(browser.series_cache.is_empty());
        assert!(browser.series_job.is_none());
    }

    #[test]
    fn media_hover_and_click_use_the_same_grid_without_activating_gaps() {
        let volume = Volume { path: "/tmp/usb/sda1".into(), label: None, mount_id: None, system: false };
        let folder = volume.path.join("Temporada 1");
        for tab in [1, 2, DOCUMENTALS_TAB, FAVORITES_TAB] {
            let mut browser = Browser::new();
            let names = ["Primer.S01E01.mkv", "Segon.S01E02.mkv", "Tercer.S01E03.mkv"];
            if tab == FAVORITES_TAB {
                browser.enter_favorites(volume.clone(), names.map(|name| folder.join(name)).to_vec());
            } else {
                browser.nav_tab = tab;
                browser.folders.push(Folder {
                    volume: volume.clone(), path: folder.clone(),
                    entries: names.into_iter().map(|name| disk::media_entry(name.into(), false).unwrap()).collect(),
                    collection: None, selected: 0, scroll: 0.0,
                });
            }
            let first = browser.tile_rect(0);
            let second = browser.tile_rect(1);
            let third = browser.tile_rect(2);
            assert!(browser.pointer(second.x + 20.0, second.y + 20.0));
            assert_eq!(browser.selected, 1);
            assert!(browser.pending.is_none() && browser.dialog.is_none());
            assert!(!browser.pointer(second.x + 21.0, second.y + 20.0));
            browser.key(Key::Right { alt: false }, false);
            assert_eq!(browser.selected, 2);
            browser.click(first.x + first.w + 1.0, first.y + 20.0);
            assert_eq!(browser.selected, 2);
            assert!(browser.pending.is_none() && browser.dialog.is_none());
            assert!(browser.pointer(second.x + 20.0, second.y + 20.0));
            browser.click(third.x + 20.0, third.y + 20.0);
            assert_eq!(browser.selected, 2);
            assert!(matches!(browser.dialog, Some(Dialog::Video { path, .. }) if path == folder.join(names[2])));
        }
    }

    #[test]
    fn media_pointer_opens_the_hit_folder_and_ok_still_opens_the_focused_card() {
        let root = plx_base::testscratch::process_dir("cineusb-pointer-folders");
        let series = root.join("Sèries");
        let second_show = series.join("Segona sèrie");
        std::fs::create_dir_all(&second_show).unwrap();
        let volume = Volume { path: root.clone(), label: None, mount_id: None, system: false };
        let mut browser = Browser::new();
        browser.nav_tab = 2;
        browser.folders.push(Folder {
            volume,
            path: series,
            entries: ["Primera sèrie", "Segona sèrie"].into_iter()
                .map(|name| disk::media_entry(name.into(), true).unwrap()).collect(),
            collection: Some(Cover::Series), selected: 0, scroll: 0.0,
        });
        let first = browser.tile_rect(0);
        let second = browser.tile_rect(1);
        assert!(browser.pointer(second.x + 20.0, second.y + 20.0));
        browser.click(second.x + 20.0, second.y + 20.0);
        let result = browser.pending.take().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(result, DiskResult::Folder(_, path, Ok(_)) if path == second_show));

        assert!(browser.pointer(first.x + 20.0, first.y + 20.0));
        browser.key(Key::Right { alt: false }, false);
        browser.key(Key::Ok, false);
        let result = browser.pending.take().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(result, DiskResult::Folder(_, path, Ok(_)) if path == second_show));
        plx_base::testscratch::remove_tree(&root);
    }

    #[test]
    fn media_rows_share_a_left_edge_and_column_width_at_every_card_size() {
        let mut browser = Browser::new();
        for tab in [1, 2, DOCUMENTALS_TAB] {
            browser.nav_tab = tab;
            for (size, cols) in [8, 10, 6].into_iter().enumerate() {
                browser.settings.cards = size;
                let first = browser.tile_rect(0);
                let next_row = browser.tile_rect(cols);
                assert_eq!(browser.grid().cols, cols);
                assert_eq!(first.x, plx_ui::consts::MARGIN_X);
                assert_eq!(next_row.x, first.x);
                assert_eq!(next_row.w, first.w);
                assert!(browser.tile_rect(cols - 1).x + first.w <= 1920.0 - first.x + 0.01);
                assert_eq!(grid_geometry(1, size, true).w, grid_geometry(cols + 1, size, true).w);
            }
        }
    }

    #[test]
    fn series_heading_is_the_show_name_at_any_depth_below_the_category() {
        let mut browser = Browser::new();
        let volume = Volume { path: "/tmp/usb/sda1".into(), label: None, mount_id: None, system: false };
        let category = volume.path.join("Vídeos/SÈRIES");
        browser.enter_media(volume.clone(), 2, Vec::new());
        browser.media_view.as_mut().unwrap().paths.push(category.clone());
        browser.folders.push(Folder {
            volume, path: category.clone(), entries: vec![], collection: Some(Cover::Series),
            selected: 0, scroll: 0.0,
        });
        assert_eq!(browser.media_series_name(), None);
        browser.folders.last_mut().unwrap().path = category.join("La sèrie");
        assert_eq!(browser.media_series_name().as_deref(), Some("La sèrie"));
        browser.folders.last_mut().unwrap().path.push("Temporada 2");
        assert_eq!(browser.media_series_name().as_deref(), Some("La sèrie"));
        browser.nav_tab = DOCUMENTALS_TAB;
        assert_eq!(browser.media_series_name(), None);
    }


    #[test]
    fn hour_long_movies_fit_card_duration_badges_without_ellipsis() {
        for width in [58.0, 70.0, 90.0, 120.0] {
            assert_eq!(card_duration(115 * 60, width), "1h55");
            assert_eq!(card_duration(135 * 60, width), "2h15");
            assert_eq!(card_duration(125 * 60, width), "2h05");
        }
        assert_eq!(card_duration(35 * 60, 65.0), "35m");
        assert_eq!(card_duration(35 * 60, 110.0), "35 min");
        assert_eq!(card_duration(115 * 60, 140.0), "1 h 55 min");
    }

    #[test]
    fn settings_motion_uses_a_clamped_eased_transition() {
        assert_eq!(SETTINGS_MOTION, Duration::from_millis(220));
        assert_eq!(theme::SURFACE_APP[3], 1.0);
        let browser = Browser::new();
        assert!(browser.settings_return_focus.is_none());
        assert!(browser.settings_exit.is_none());
    }

    #[test]
    fn episode_titles_hide_file_extensions_independently_of_settings() {
        let entry = disk::media_entry("Serie.S01E02.2160p.mkv".into(), false).unwrap();
        assert_eq!(display_name(&entry, false), "Serie");
        let movie = disk::media_entry("Película.mp4".into(), false).unwrap();
        assert_eq!(display_name(&movie, false), "Película");
        for name in ["03 La sangre helada", "03. La sangre helada", "03 - La sangre helada", "03_La sangre helada"] {
            assert_eq!(clean_series_name(name), "La sangre helada");
        }
        for title in ["1917", "2012", "101 Dálmatas", "24", "3 Body Problem"] {
            assert_eq!(clean_series_name(title), title);
        }
        let path = std::path::Path::new("Películas/La vida es bella/title_t00 (1).mkv");
        assert_eq!(video_title(path, Some("track_01")), "La vida es bella");
        assert_eq!(video_title(path, Some("La vita è bella")), "La vita è bella");
        assert_eq!(video_title(std::path::Path::new("Películas/1917.mkv"), Some("title_t01")), "1917");
        assert_eq!(video_title(std::path::Path::new("Películas/03 - La sangre helada.mkv"), None), "La sangre helada");
        assert_eq!(file_title("Breaking.Bad.S01E01.Pilot.1080p.WEB-DL", None), "Pilot");
        assert_eq!(file_title("Película.2025", None), "Película 2025");
    }

    #[test]
    fn media_folders_match_catalan_spanish_and_english_without_false_positives() {
        for name in ["PELIS", "Películas", "Peliculas", "Pel·lícules", "Movies", "movie_collection", "Films", "Cine"] {
            assert!(media_folder_score(name, 1) > 0, "{name}");
        }
        for name in ["SERIES", "Sèries", "Series TV", "TV Shows", "tv_series", "Temporadas", "Episodios"] {
            assert!(media_folder_score(name, 2) > 0, "{name}");
        }
        assert_eq!(media_folder_score("Mis documentos", 1), 0);
        assert_eq!(media_folder_score("The Series Movie.mkv", 2), 0);
        assert_eq!(media_folder_score("Séries", 1), 0);
        assert_eq!(folder_fold("TÉLÉ-ÉPISODES"), "teleepisodes");
    }

    #[test]
    fn favorite_nav_star_uses_yellow_in_every_state() {
        for selected in [0.0, 0.25, 0.75, 1.0] {
            for focus in [0.0, 0.5, 1.0] {
                assert_eq!(nav_glyph_ink(FAVORITES_TAB, focus, selected), theme::USB_FAVORITES);
            }
        }
    }

    #[test]
    fn leaving_settings_restores_previous_navigation_focus() {
        let mut browser = Browser::new();
        browser.nav_tab = 2;
        browser.open_nav_tab(SETTINGS_TAB);
        assert!(matches!(browser.dialog, Some(Dialog::Settings)));
        browser.back();
        assert!(browser.dialog.is_none());
        assert_eq!(browser.nav_focus, Some(SETTINGS_TAB));
        assert!(browser.settings_exit.is_some());
    }

    #[test]
    fn magic_remote_hover_previews_tabs_until_ok_and_yields_to_dpad() {
        let mut browser = Browser::new();
        let documentals = NAV_TABS[DOCUMENTALS_TAB];
        let favorites = NAV_TABS[FAVORITES_TAB];
        let settings = NAV_TABS[SETTINGS_TAB];

        assert!(browser.pointer(documentals.x + 1.0, documentals.y + 1.0));
        assert_eq!(browser.nav_hover, Some(DOCUMENTALS_TAB));
        assert_eq!(browser.nav_tab, 0);
        assert!(browser.pending.is_none());
        assert!(!browser.pointer(favorites.x - 1.0, favorites.y + 1.0));
        assert!(browser.pointer(favorites.x + 1.0, favorites.y + 1.0));

        browser.key(Key::Right { alt: false }, false);
        assert_eq!(browser.nav_hover, None);
        assert_eq!(browser.nav_focus, Some(SETTINGS_TAB));
        assert!(browser.dialog.is_none());
        browser.key(Key::Ok, false);
        assert!(matches!(browser.dialog, Some(Dialog::Settings)));
        assert!(!browser.pointer(settings.x + 1.0, settings.y + 1.0));
        browser.back();
        assert_eq!(browser.nav_focus, Some(SETTINGS_TAB));
        browser.key(Key::Down, false);
        assert!(browser.grid_has_focus());
    }

    #[test]
    fn hidden_pointer_clears_hover_and_click_opens_the_tab_at_its_coordinates() {
        let mut browser = Browser::new();
        let settings = NAV_TABS[SETTINGS_TAB];
        let (x, y) = (settings.x + 1.0, settings.y + 1.0);
        assert!(browser.pointer(x, y));
        assert!(browser.dialog.is_none());
        browser.key(Key::PointerHidden, false);
        assert_eq!(browser.nav_hover, None);
        assert!(browser.grid_has_focus());
        browser.click(x, y);
        assert!(matches!(browser.dialog, Some(Dialog::Settings)));
        assert_eq!(browser.nav_hover, None);
    }

    #[test]
    fn clicking_between_tabs_does_not_activate_a_usb_drive() {
        let mut browser = Browser::new();
        browser.scan.volumes.push(Volume {
            path: "/tmp/usb/sda/sda1".into(), label: None, mount_id: None, system: false,
        });
        let first = NAV_TABS[0];
        assert!(browser.pointer(first.x + 1.0, first.y + 1.0));
        browser.click(first.x + first.w + 1.0, first.y + 1.0);
        assert!(browser.pending.is_none());
        assert_eq!(browser.nav_hover, Some(0));
    }

    #[test]
    fn clicking_a_tab_without_a_disk_keeps_it_active_when_the_pointer_leaves() {
        let mut browser = Browser::new();
        let rect = NAV_TABS[DOCUMENTALS_TAB];
        let (x, y) = (rect.x + 5.0, rect.y + 5.0);
        assert!(browser.pointer(x, y));
        assert_eq!(browser.nav_tab, 0);
        browser.click(x, y);
        assert_eq!(browser.nav_tab, DOCUMENTALS_TAB);
        assert!(browser.tab_waiting_for_volume());
        assert!(browser.pointer(x, y));
        assert!(browser.pointer(x, y + 140.0));
        assert_eq!(browser.nav_hover, None);
        assert_eq!(browser.nav_focus, None);
        assert_eq!(browser.nav_tab, DOCUMENTALS_TAB);
    }

    #[test]
    fn a_tab_click_supersedes_an_inflight_section_read() {
        let mut browser = Browser::new();
        let volume = Volume { path: "/tmp/usb/sda/sda1".into(), label: None, mount_id: None, system: false };
        browser.scan.volumes.push(volume.clone());
        browser.enter_media(volume, 1, Vec::new());
        let (stale, pending) = mpsc::sync_channel(1);
        browser.pending = Some(pending);
        let rect = NAV_TABS[2];
        browser.click(rect.x + 5.0, rect.y + 5.0);
        assert_eq!(browser.nav_tab, 2);
        assert!(browser.pending.is_some());
        stale.send(DiskResult::Discovered(browser.volume().unwrap().clone(), 1, vec![])).unwrap();
        assert_eq!(browser.retired_reads.len(), 1);
        assert!(!browser.usb_reads_drained());
        assert!(matches!(browser.retired_reads[0].try_recv(), Ok(DiskResult::Discovered(_, 1, _))));
        assert_eq!(browser.nav_tab, 2);
    }

    #[test]
    fn settings_hover_click_and_dpad_target_the_same_rows() {
        let mut browser = Browser::new();
        browser.open_nav_tab(SETTINGS_TAB);
        let language = unified_setting_rect(5);
        assert!(browser.pointer(language.x + 15.0, language.y + 15.0));
        assert_eq!(browser.option, 5);
        browser.key(Key::Up, false);
        assert_eq!(browser.option, 12);
        browser.key(Key::Up, false);
        assert_eq!(browser.option, 4);
        // Keep the click on its normal path without writing real preferences.
        let (_load, pending) = mpsc::sync_channel(1);
        browser.settings_job = Some(SettingsJob::Load(pending));
        let accent = unified_setting_rect(6);
        browser.click(accent.x + 15.0, accent.y + 15.0);
        assert_eq!(browser.option, 6);
        assert!(matches!(browser.dialog, Some(Dialog::Settings)));
    }

    #[test]
    fn seven_navigation_destinations_stay_in_one_stationary_track() {
        assert_eq!(NAV_TABS.len(), 7);
        let first = NAV_TABS.first().unwrap();
        let last = NAV_TABS.last().unwrap();
        assert!(first.x >= NAV_PILL.x && first.y >= NAV_PILL.y);
        assert!(last.x + last.w <= NAV_PILL.x + NAV_PILL.w);
        assert!(last.y + last.h <= NAV_PILL.y + NAV_PILL.h);
        for pair in NAV_TABS.windows(2) {
            assert_eq!(pair[0].y, pair[1].y);
            assert_eq!(pair[0].h, pair[1].h);
            assert!(pair[0].x + pair[0].w < pair[1].x,
                "navigation actions must never overlap");
        }
        assert!(NAV_PILL.x + NAV_PILL.w <= 1920.0);
    }

    #[test]
    fn compact_video_dialog_wraps_the_original_filename_without_losing_characters() {
        let measure = plx_base::fontcov::advances::ShippedMeasure;
        let filename = format!("{} Causa abierta  S01E03.Last.Seen.mkv", "àç".repeat(52));
        for options in [EPISODE_VIDEO_OPTIONS.as_slice(), VIDEO_OPTIONS.as_slice()] {
            let (_, width) = video_dialog_column(options);
            let lines = video_filename_lines(&filename, width, &measure);
            assert!(lines.len() > 1);
            assert_eq!(lines.concat(), filename);
            assert!(lines.iter().all(|line| measure.width_str(line, theme::size::MICRO, false) <= width));
        }
    }

    #[test]
    fn compact_video_dialog_actions_fit_in_both_languages() {
        let measure = plx_base::fontcov::advances::ShippedMeasure;
        for options in [EPISODE_VIDEO_OPTIONS.as_slice(), VIDEO_OPTIONS.as_slice()] {
            let labels = [
                ["Reproducir", "Reprodueix"],
                ["Eliminar archivo", "Esborra el fitxer"],
                ["Marcar como no visto", "Marcar com a no vist"],
                ["Añadir", "Afegeix"],
            ];
            for (index, (&rect, labels)) in options.iter().zip(labels).enumerate() {
                let budget = rect.w - if index == 3 { 76.0 } else { 67.0 };
                for label in labels {
                    assert!(measure.width_str(label, theme::size::CAPTION, true) <= budget, "{label}");
                }
            }
            assert!(options.windows(2).all(|pair| pair[0].x + pair[0].w < pair[1].x));
        }
    }

    #[test]
    fn series_favorite_is_a_distinct_header_action_with_no_overlap() {
        assert_eq!(ACTIONS.len(), 3);
        assert!(matches!(ACTIONS[0].0, Action::ToggleFavorite));
        assert!(matches!(ACTIONS[1].0, Action::Select));
        assert!(matches!(ACTIONS[2].0, Action::DeleteFolders));
        for pair in ACTIONS.windows(2) {
            assert!(pair[0].1.x + pair[0].1.w < pair[1].1.x);
        }
        assert!(ACTIONS[2].1.x + ACTIONS[2].1.w <= 1920.0);
        assert_eq!(VIDEO_OPTIONS.len(), 4, "movies keep the favorite option");
    }

    #[test]
    fn reversible_favorite_fill_is_clamped_and_never_snaps_to_yellow() {
        assert_eq!(favorite_ease(-1.0), 0.0);
        assert_eq!(favorite_ease(1.5), 1.0);
        assert!(favorite_ease(0.25) < 0.25);
        assert!(favorite_ease(0.75) > 0.75);
        assert_eq!(favorite_lerp([0.0; 4], FAVORITE_YELLOW, 0.0), [0.0; 4]);
        assert_eq!(favorite_lerp([0.0; 4], FAVORITE_YELLOW, 1.0), FAVORITE_YELLOW);
        assert_eq!(FAVORITE_MOTION, Duration::from_millis(340));
    }

    #[test]
    fn series_favorites_use_one_root_from_episode_and_season_views() {
        let volume = Volume { path: "/tmp/usb/sdb/sdb1".into(), label: None, mount_id: Some(42), system: true };
        let series = volume.path.join("SERIES/Breaking Bad");
        let season = series.join("Season 01");
        let mut browser = Browser::new();
        browser.scan.volumes.push(volume.clone());
        let entry = |name: &str, kind: Kind| Entry {
            name: name.into(), kind, modified: None, size: None,
        };
        browser.folders.push(Folder {
            volume: volume.clone(), path: volume.path.join("SERIES"),
            entries: vec![entry("Breaking Bad", Kind::Folder)],
            collection: None, selected: 0, scroll: 0.0,
        });
        assert_eq!(browser.series_root(), None,
            "category folders must never be favoritable");
        browser.folders.push(Folder {
            volume: volume.clone(), path: series.clone(),
            entries: vec![entry("Season 01", Kind::Folder)],
            collection: None, selected: 0, scroll: 0.0,
        });
        assert_eq!(browser.series_root(), Some(series.clone()));
        let episode = season.join("Breaking Bad S01E01.mkv");
        browser.folders.push(Folder {
            volume: volume.clone(), path: season,
            entries: vec![entry("Breaking Bad S01E01.mkv", Kind::Video)],
            collection: None, selected: 0, scroll: 0.0,
        });
        assert_eq!(browser.series_root(), Some(series.clone()));
        assert_eq!(browser.video_favorite_target(&volume, &episode), series);
        assert!(browser.video_is_episode(&volume, &episode));
        browser.dialog = Some(Dialog::Video { volume: volume.clone(), path: episode });
        assert_eq!(browser.horizontal_options().len(), 3, "episodes never show a favorite action");
        assert!(!browser.option_available(3));
    }

    #[test]
    fn movie_favorites_keep_a_single_file_and_reopen_its_video_dialog() {
        let volume = Volume { path: "/tmp/usb/sdb/sdb1".into(), label: None, mount_id: Some(42), system: true };
        let movie = volume.path.join("PELIS/Crash.mkv");
        let mut browser = Browser::new();
        browser.scan.volumes.push(volume.clone());
        assert_eq!(browser.video_favorite_target(&volume, &movie), movie);
        browser.enter_favorites(volume.clone(), vec![movie.clone()]);
        assert_eq!(browser.len(), 1);
        assert_eq!(browser.item(0).unwrap().1, tr("Vídeo", "Vídeo"));
        assert_eq!(browser.item(0).unwrap().0, "Crash");
        browser.open();
        assert!(matches!(browser.dialog, Some(Dialog::Video { ref volume, ref path })
            if volume == &browser.scan.volumes[0] && path == &movie));
        assert_eq!(browser.horizontal_options().len(), 4);
        assert!(!browser.video_is_episode(&volume, &movie));
    }

    #[test]
    fn empty_series_still_has_one_series_root_not_a_season_root() {
        let volume = Volume { path: "/tmp/usb/sda/sda1".into(), label: None, mount_id: None, system: false };
        let mut browser = Browser::new();
        let series = volume.path.join("SERIES/Empty Show");
        browser.folders.push(Folder { volume: volume.clone(), path: series.clone(),
            entries: vec![], collection: Some(Cover::Series), selected: 0, scroll: 0.0 });
        assert_eq!(browser.series_root(), Some(series));
        assert!(!browser.video_is_episode(&volume, &volume.path.join("PELIS/Movie.mkv")));
        browser.folders.push(Folder { volume: volume.clone(), path: volume.path.join("SERIES/Empty Show/Season 01"),
            entries: vec![], collection: Some(Cover::Series), selected: 0, scroll: 0.0 });
        assert_eq!(browser.series_root(), Some(volume.path.join("SERIES/Empty Show")));
    }

    fn movie_quality_browser() -> Browser {
        let mut browser = Browser::new();
        let volume = Volume { path: "/tmp/usb/sda1".into(), label: None, mount_id: None, system: false };
        let path = volume.path.join("PELIS");
        browser.scan.volumes.push(volume.clone());
        browser.nav_tab = MOVIES_TAB;
        browser.media_view = Some(FavoriteView {
            volume: volume.clone(), paths: vec![path.clone()], depth: 0, selected: 0, scroll: 0.0,
            library_selected: 0, library_scroll: 0.0,
        });
        let entries = [("B.mkv", 30), ("C.mkv", 15), ("A.mkv", 20), ("D.mkv", 10)]
            .into_iter().map(|(name, size)| Entry {
                name: name.into(), kind: Kind::Video, modified: None, size: Some(size),
            }).collect();
        browser.folders.push(Folder { volume, path, entries, collection: Some(Cover::Movie), selected: 0, scroll: 0.0 });
        let folder = browser.folders.last_mut().unwrap();
        disk::sort_entries_by(&mut folder.entries, browser.settings.sort);
        for (name, resolution) in [("B.mkv", (3840, 1608)), ("C.mkv", (1920, 804)), ("A.mkv", (3836, 1600))] {
            let entry = folder.entries.iter().find(|entry| entry.name == name).unwrap();
            browser.details.insert(Browser::detail_key(folder, entry), plx_play_feed::LocalVideoDetails {
                resolution: Some(resolution), ..Default::default()
            });
        }
        browser.rebuild_movie_grid();
        browser
    }

    fn movie_names(browser: &Browser) -> Vec<String> {
        (0..browser.len()).map(|index| browser.folder_entry(index).unwrap().name.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn pointer_highlights_selection_cancel_and_delete_in_each_library() {
        for tab in [MOVIES_TAB, SERIES_TAB, DOCUMENTALS_TAB] {
            let mut browser = movie_quality_browser();
            browser.nav_tab = tab;
            let folder = browser.folders.last_mut().unwrap();
            folder.volume.system = true;
            folder.volume.mount_id = Some(42);
            if tab == SERIES_TAB {
                folder.entries = vec![disk::media_entry("Una sèrie".into(), true).unwrap()];
            }
            let center = |r: Rect| (r.x + r.w / 2.0, r.y + r.h / 2.0);
            let (x, y) = center(browser.content_actions()[1].1);
            assert!(browser.pointer(x, y));
            assert_eq!(browser.action_hover, Some(Action::Select));
            assert!(!browser.grid_has_focus());
            assert!(browser.selection.is_none());
            browser.click(x, y);
            assert!(browser.selection.is_some());
            assert_eq!(browser.action_hover, Some(Action::Select));
            browser.open();
            let (x, y) = center(browser.content_actions()[2].1);
            assert!(browser.pointer(x, y));
            assert_eq!(browser.action_hover, Some(Action::DeleteFolders));
            assert!(browser.dialog.is_none());
            browser.key(Key::PointerHidden, false);
            assert!(browser.action_hover.is_none());
            browser.click(x, y);
            assert!(matches!(browser.dialog, Some(Dialog::DeleteFolders)));
            let (confirm_x, confirm_y) = center(browser.horizontal_options()[1]);
            assert!(browser.pointer(confirm_x, confirm_y));
            assert_eq!(browser.option, 1);
            let (cancel_x, cancel_y) = center(browser.horizontal_options()[0]);
            assert!(browser.pointer(cancel_x, cancel_y));
            assert_eq!(browser.option, 0);
            assert!(browser.pending.is_none());
        }
    }

    #[test]
    fn deletion_selection_is_scoped_to_the_current_library_tab() {
        for tab in [MOVIES_TAB, SERIES_TAB, DOCUMENTALS_TAB] {
            let mut browser = movie_quality_browser();
            browser.nav_tab = tab;
            let folder = browser.folders.last_mut().unwrap();
            folder.volume.system = true;
            folder.volume.mount_id = Some(42);
            if tab == SERIES_TAB {
                folder.entries = vec![disk::media_entry("Una sèrie".into(), true).unwrap()];
            }
            browser.open_action(Action::Select);
            assert!(browser.selection.is_some());
            assert!(!browser.action_available(Action::DeleteFolders));
            assert_eq!(browser.delete_selection_mark(browser.selected), Some(false));
            browser.open();
            assert_eq!(browser.delete_selection_mark(browser.selected), Some(true));
            assert_eq!(browser.selection.as_ref().unwrap().len(), 1);
            assert!(browser.action_available(Action::DeleteFolders));
            browser.open_nav_tab(0);
            assert!(browser.selection.is_none());
            assert!(!browser.action_available(Action::Select));
            assert!(!browser.action_available(Action::DeleteFolders));
        }
    }

    #[test]
    fn metadata_only_changes_movie_membership_when_it_matches_the_active_filter() {
        let mut browser = movie_quality_browser();
        browser.select(1, true);
        let selected = browser.selected;
        let entries = browser.movie_entries.clone();
        let folder = browser.folders.last().unwrap();
        let pending = folder.entries.iter().find(|entry| entry.name == "D.mkv").unwrap();
        let key = Browser::detail_key(folder, pending);
        let hd = plx_play_feed::LocalVideoDetails { resolution: Some((1920, 1080)), ..Default::default() };
        browser.accept_video_details(key.clone(), hd);
        assert_eq!(browser.movie_entries, entries);
        assert_eq!(browser.selected, selected);
        assert_eq!(browser.movie_pending, 0);

        browser.choose_movie_filter(1);
        let uhd = plx_play_feed::LocalVideoDetails { resolution: Some((3840, 2160)), ..Default::default() };
        browser.accept_video_details(key, uhd);
        assert_eq!(movie_names(&browser), ["A.mkv", "B.mkv", "D.mkv"]);
    }

    #[test]
    fn movie_quality_filters_partition_sorted_movies_after_metadata_arrives() {
        let mut browser = movie_quality_browser();
        assert_eq!(movie_names(&browser), ["A.mkv", "B.mkv", "C.mkv", "D.mkv"]);
        assert_eq!((browser.movie_count, browser.movie_pending), (4, 1));
        browser.choose_movie_filter(1);
        assert_eq!(movie_names(&browser), ["A.mkv", "B.mkv"]);
        assert_eq!(browser.movie_count, 2);
        browser.choose_movie_filter(2);
        assert_eq!(movie_names(&browser), ["C.mkv"]);
        let (send, reply) = mpsc::sync_channel(1);
        let folder = browser.folders.last().unwrap();
        let entry = folder.entries.iter().find(|entry| entry.name == "D.mkv").unwrap();
        browser.details_job = Some(DetailsRequest {
            volume: folder.volume.clone(), key: Browser::detail_key(folder, entry), reply,
        });
        send.send(plx_play_feed::LocalVideoDetails {
            resolution: Some((1904, 800)), ..Default::default()
        }).unwrap();
        assert!(browser.update_video_details());
        assert_eq!(movie_names(&browser), ["C.mkv", "D.mkv"]);
        assert_eq!((browser.movie_count, browser.movie_pending), (2, 0));
        let mut partition = movie_names(&browser);
        browser.choose_movie_filter(1);
        partition.extend(movie_names(&browser));
        partition.sort();
        assert_eq!(partition, ["A.mkv", "B.mkv", "C.mkv", "D.mkv"]);
        assert!(browser.details_job.is_none());
        assert!(browser.pending.is_none(), "changing filters does not list the USB again");

        let mut settings = browser.settings;
        settings.sort = 2;
        let (send, reply) = mpsc::sync_channel(1);
        send.send(Ok(settings)).unwrap();
        browser.settings_started = true;
        browser.settings_job = Some(SettingsJob::Save(reply));
        assert!(browser.update_settings());
        assert_eq!(movie_names(&browser), ["B.mkv", "A.mkv"]);
        assert_eq!(browser.selected, 1, "sorting keeps A selected");
        browser.choose_movie_filter(0);
        assert_eq!(movie_names(&browser), ["B.mkv", "A.mkv", "C.mkv", "D.mkv"]);
    }

    #[test]
    fn movie_quality_selection_moves_with_pointer_and_remote_and_resets_on_exit() {
        let mut browser = movie_quality_browser();
        let initial_grid = browser.tile_rect(0);
        for _ in 0..90 { browser.step_movie_filter(1.0 / 60.0); }
        assert!(!browser.quality_strip.moving());
        browser.key(Key::Up, false);
        assert_eq!(browser.quality_focus, Some(0));
        browser.key(Key::Right { alt: false }, false);
        assert_eq!(browser.movie_filter, MovieFilter::All, "focus alone keeps the grid");
        browser.key(Key::Ok, false);
        assert_eq!(browser.movie_filter, MovieFilter::Uhd);
        assert!(browser.step_movie_filter(1.0 / 60.0));
        for _ in 0..90 { browser.step_movie_filter(1.0 / 60.0); }
        assert!(!browser.quality_strip.moving());
        assert_eq!(browser.tile_rect(0), initial_grid);
        browser.key(Key::Down, false);
        assert!(browser.grid_has_focus());
        let hd = QUALITY_TABS[2];
        assert!(browser.pointer(hd.x + 5.0, hd.y + 5.0));
        assert_eq!(browser.quality_hover, Some(2));
        assert_eq!(browser.movie_filter, MovieFilter::Uhd);
        browser.click(hd.x + 5.0, hd.y + 5.0);
        assert_eq!(movie_names(&browser), ["C.mkv"]);
        browser.open_nav_tab(MOVIES_TAB);
        assert_eq!(browser.movie_filter, MovieFilter::FullHd);
        // Returning from a subfolder keeps the movie quality filter.
        let volume = browser.folders[0].volume.clone();
        let nested = browser.folders[0].path.join("Subfolder");
        browser.folders.push(Folder { volume, path: nested, entries: vec![], collection: Some(Cover::Movie),
            selected: 0, scroll: 0.0 });
        browser.rebuild_movie_grid();
        assert_eq!(browser.movie_count, 0);
        browser.back();
        assert_eq!(movie_names(&browser), ["C.mkv"]);
        browser.back();
        assert!(!browser.movie_grid());
        assert_eq!(browser.movie_filter, MovieFilter::All);
        assert!(!browser.pointer(hd.x + 5.0, hd.y + 5.0));
        let volume = browser.scan.volumes[0].clone();
        browser.enter_media(volume, MOVIES_TAB, vec![]);
        assert_eq!(browser.movie_filter, MovieFilter::All);
        assert_eq!(browser.details.len(), 3, "re-entering keeps discovered resolutions");
        for section in [0, 2, DOCUMENTALS_TAB, FAVORITES_TAB] {
            browser.nav_tab = section;
            assert!(!browser.focus_movie_filter());
        }
    }

    #[test]
    fn pelis_folder_is_recognized_as_movies_not_series() {
        assert_eq!(media_folder_score("PELIS", 1), 3);
        assert_eq!(media_folder_score("pelis", 1), 3);
        assert_eq!(media_folder_score("PELIS", 2), 0);
        let root = plx_base::testscratch::process_dir("cineusb-pelis-category");
        std::fs::create_dir_all(root.join("PELIS")).unwrap();
        std::fs::create_dir_all(root.join("SERIES")).unwrap();
        let volume = Volume { path: root.clone(), label: None, mount_id: None, system: false };
        assert_eq!(discover_media_folders(&volume, 1), vec![root.join("PELIS")]);
        assert_eq!(discover_media_folders(&volume, 2), vec![root.join("SERIES")]);
        plx_base::testscratch::remove_tree(&root);
    }

    #[test]
    fn opening_movie_tab_rechecks_the_current_folder_names() {
        let volume = Volume { path: "/tmp/usb/sdb/sdb1".into(), label: None, mount_id: Some(17), system: true };
        let mut browser = Browser::new();
        browser.scan.volumes.push(volume.clone());
        browser.discovery_cache.insert((volume.path.clone(), 17, 1), vec![volume.path.join("Old Movies")]);
        browser.open_nav_tab(1);
        assert!(browser.pending.is_some(), "category discovery must query the live disk");
        assert!(!browser.discovery_cache.contains_key(&(volume.path.clone(), 17, 1)));
    }

    #[test]
    fn movie_navigation_returns_all_matching_folders_for_one_usb() {
        let volume = Volume { path: "/tmp/usb/sdb/sdb1".into(), label: None, mount_id: Some(121), system: true };
        let first = volume.path.join("PELIS");
        let second = volume.path.join("Películas 4K");
        let mut browser = Browser::new();
        browser.scan.volumes.push(volume.clone());
        browser.discovery_cache.insert((volume.path.clone(), 121, 1), vec![volume.path.join("Old Movies")]);
        browser.open_nav_tab(1);
        assert!(browser.pending.is_some(), "Pelis discovers fresh paths rather than using the old cache");
        browser.pending = None;
        browser.enter_media(volume.clone(), 1, vec![first.clone(), second.clone()]);
        assert_eq!(browser.nav_tab, 1);
        assert_eq!(browser.media_cards().unwrap().paths, vec![first.clone(), second]);
        assert!(browser.pending.is_some(), "Pelis loads the category's files directly");
        assert_eq!(browser.nav_tab, 1);
        assert!(browser.folders.is_empty(), "the category isn't a visible card");
        browser.pending = None;
        browser.folders.push(Folder { volume: volume.clone(), path: first.clone(),
            entries: vec![], collection: Some(Cover::Movie), selected: 0, scroll: 0.0 });
        assert!(browser.media_cards().is_none(), "category contents replace the folder picker");
        browser.back();
        assert!(browser.folders.is_empty(), "Back skips the category container");
        assert_eq!(browser.nav_tab, 0);
        browser.leave_media();
        assert_eq!(browser.nav_tab, 0);
        assert!(browser.media_cards().is_none());
    }

    #[test]
    fn series_tab_opens_directly_and_a_missing_category_has_an_empty_view() {
        let volume = Volume { path: "/tmp/usb/sdb/sdb1".into(), label: None, mount_id: None, system: false };
        let mut browser = Browser::new();
        browser.scan.volumes.push(volume.clone());
        let series = volume.path.join("SERIES");
        browser.discovery_cache.insert((volume.path.clone(), 0, 2), vec![series.clone()]);
        browser.open_nav_tab(2);
        assert!(browser.pending.is_some());
        browser.pending = None;
        browser.enter_media(volume.clone(), 2, vec![series]);
        assert_eq!(browser.nav_tab, 2);
        assert!(browser.pending.is_some());
        browser.pending = None;
        browser.leave_media();
        browser.discovery_cache.insert((volume.path.clone(), 0, 2), vec![]);
        browser.open_nav_tab(2);
        assert!(browser.pending.is_some(), "even empty category caches are rechecked");
        browser.pending = None;
        browser.enter_media(volume, 2, vec![]);
        assert!(browser.media_cards().is_some_and(|view| view.paths.is_empty()));
        assert_eq!(browser.len(), 0);
        assert!(browser.pending.is_none());
    }

    #[test]
    fn documentals_finds_both_folder_names_and_can_open_either_match() {
        assert_eq!(media_folder_score("DoCuMeNtAlS", DOCUMENTALS_TAB), 3);
        assert_eq!(media_folder_score("DOCUMENTALES", DOCUMENTALS_TAB), 3);
        assert_eq!(media_folder_score("Documentales 4K", DOCUMENTALS_TAB), 0);
        assert_eq!(media_folder_score("Series", DOCUMENTALS_TAB), 0);

        let root = plx_base::testscratch::process_dir("cineusb-documentals-category");
        let first = root.join("DOCUMENTALES");
        let second = root.join("Documentals");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::create_dir_all(root.join("Series")).unwrap();
        let volume = Volume { path: root.clone(), label: None, mount_id: None, system: false };
        let paths = discover_media_folders(&volume, DOCUMENTALS_TAB);
        assert_eq!(paths, vec![first, second.clone()]);

        let mut browser = Browser::new();
        browser.scan.volumes.push(volume.clone());
        browser.enter_media(volume, DOCUMENTALS_TAB, paths);
        assert_eq!(browser.len(), 2);
        browser.selected = 1;
        browser.open();
        let result = browser.pending.take().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(result, DiskResult::Folder(_, path, Ok(_)) if path == second));
        plx_base::testscratch::remove_tree(&root);
    }

    #[test]
    fn first_favorites_entry_pins_the_drive_and_never_exposes_usb_root_cards() {
        let mut browser = Browser::new();
        let first = Volume { path: "/tmp/usb/first".into(), label: None, mount_id: None, system: false };
        let selected = Volume { path: "/tmp/usb/selected".into(), label: None, mount_id: None, system: false };
        browser.scan.volumes = vec![first, selected.clone()];
        browser.selected = 1;
        browser.open_nav_tab(FAVORITES_TAB);
        assert!(browser.loading_favorites);
        assert_eq!(browser.len(), 0, "USB root cards must not appear while favorites load");
        assert!(browser.item(0).is_none(), "the loading view must not expose a clickable USB card");
        assert_eq!(browser.favorite_cards().map(|view| &view.volume), Some(&selected));
        assert_eq!(browser.volume(), Some(&selected), "resetting card focus must not select another drive");
        let show = selected.path.join("Series/La sèrie");
        let (send, reply) = mpsc::sync_channel(1);
        browser.pending = Some(reply);
        send.send(DiskResult::Favorites(selected.clone(), vec![show.clone()])).unwrap();
        // Suppress unrelated workers while consuming the actual result event.
        browser.next_scan = Instant::now() + Duration::from_secs(60);
        browser.home_refresh = browser.next_scan;
        browser.settings_started = true;
        browser.facts = Some((selected.clone(), (None, None)));
        browser.update();
        assert_eq!(browser.len(), 1);
        assert_eq!(browser.item(0).unwrap().0, "La sèrie");
        assert!(!browser.loading_favorites);
        assert_eq!(browser.favorite_cards().unwrap().paths, vec![show]);
    }

    #[test]
    fn favorites_keeps_its_section_through_folders_and_returns_to_its_cards() {
        let mut browser = Browser::new();
        let volume = Volume { path: "/tmp/usb/sda/sda1".into(), label: None, mount_id: Some(42), system: true };
        let path = volume.path.join("Sèries/Breaking Bad");
        browser.scan.volumes.push(volume.clone());
        browser.enter_favorites(volume.clone(), vec![path.clone()]);
        assert_eq!(browser.len(), 1);
        assert_eq!(browser.item(0).unwrap().0, "Breaking Bad");
        browser.folders.push(Folder { volume, path, entries: vec![], collection: None, selected: 0, scroll: 0.0 });
        browser.nav_focus = Some(SETTINGS_TAB);
        assert!(browser.favorite_section.0 && browser.favorite_cards().is_none());
        browser.nav_focus = None;
        browser.dialog = Some(Dialog::Settings);
        browser.back();
        assert!(browser.favorite_section.0 && browser.favorite_cards().is_none());
        browser.back();
        assert!(browser.favorite_section.0 && browser.favorite_cards().is_some());
        browser.back();
        assert!(!browser.favorite_section.0 && browser.favorite_cards().is_none());
    }

    #[test]
    fn restoring_folder_view_keeps_a_visible_selection_at_the_same_scroll() {
        let mut browser = Browser::new();
        browser.scan.volumes = (0..40).map(|i| Volume {
            path: format!("/tmp/usb/drive{i}").into(), label: None, mount_id: None, system: false,
        }).collect();
        browser.folders.push(Folder {
            volume: browser.scan.volumes[0].clone(), path: "/tmp/usb/drive0/Pel·lícules".into(),
            entries: (0..40).map(|index| disk::media_entry(format!("Pel·lícula {index}.mkv").into(), false).unwrap()).collect(),
            collection: Some(Cover::Movie), selected: 0, scroll: 0.0,
        });
        browser.restore_view(17, 410.0);
        assert_eq!(browser.selected, 17);
        assert_eq!(browser.scroll.pos, 410.0);
    }

    #[test]
    fn favorites_allow_usb_root_folders_and_remain_partition_scoped() {
        let volume = Volume { path: "/tmp/usb/sda/sda1".into(), label: None, mount_id: Some(42), system: true };
        let structural = volume.path.join("Vídeos");
        let nested = structural.join("Sèrie");
        assert_eq!(favorite_key("uuid:disk-a", &volume, &volume.path), None);
        assert!(favorite_key("uuid:disk-a", &volume, &structural).is_some());
        let key = favorite_key("uuid:disk-a", &volume, &nested).unwrap();
        assert_ne!(favorite_key("uuid:disk-b", &volume, &nested), Some(key.clone()));
        assert!(history_matches(&key, &path_key("uuid:disk-a", &volume, &nested).unwrap()));
        assert!(!history_matches(&key, &path_key("uuid:disk-b", &volume, &nested).unwrap()));
        assert!(!history_matches(&key, &path_key("uuid:disk-a", &volume, &structural.join("Sèri")).unwrap()));
        assert!(decode_relative("2e2e2f736563726574").is_none());
    }

    #[test]
    fn progress_can_survive_fat_exfat_ntfs_mtime_changes() {
        let volume = Volume { path: "/tmp/usb/sdb/sdb1".into(), label: None, mount_id: Some(42), system: true };
        let path = volume.path.join("Series/Episode.mkv");
        let old = entry_key("fat32:11223344", &volume, &path, 1024, 100).unwrap();
        let current = entry_key("fat32:11223344", &volume, &path, 1024, 200).unwrap();
        let different_size = entry_key("fat32:11223344", &volume, &path, 2048, 200).unwrap();
        let other_disk = entry_key("fat32:99887766", &volume, &path, 1024, 200).unwrap();
        let mut history = BTreeMap::new();
        history.insert(old.clone(), 150);
        assert_eq!(matching_size_resume(&history, &current), Some((old, 150)));
        assert_eq!(matching_size_resume(&history, &different_size), None);
        assert_eq!(matching_size_resume(&history, &other_disk), None);
    }

    #[test]
    fn progress_preserves_microseconds_and_watched_is_not_a_resume_point() {
        let position = 2_538_123_456_000;
        let duration = 3_600_000_000_000;
        assert_eq!(unpack_progress(pack_progress(position, Some(duration))), (position, duration));
        assert_eq!(unpack_progress(60), (60_000_000_000, 0));
        assert_eq!(unpack_progress(WATCHED), (0, 0));
    }

    #[test]
    fn finish_estimate_respects_actual_playback_rate_and_unknown_duration() {
        assert_eq!(remaining_wall_seconds(30_000_000_000, 90_000_000_000, 1500), Some(40));
        assert_eq!(remaining_wall_seconds(0, 0, 1000), None);
    }

    #[test]
    fn cinematic_quality_badges_do_not_promote_qhd_to_uhd() {
        for (width, height) in [(3840, 2160), (3840, 1608), (3840, 1600), (4096, 1716),
            (3840, 2076), (3838, 1596), (3860, 2190), (1608, 3840), (2880, 1200)] {
            assert_eq!(MovieQuality::of(width, height), Some(MovieQuality::Uhd));
            assert_eq!(resolution_label(width, height, true).as_deref(), Some("4K UHD"));
        }
        for (width, height) in [(1920, 1080), (1920, 800), (1920, 804), (1920, 816),
            (1920, 1040), (1904, 796), (1936, 1096), (2560, 1080), (2879, 1200), (1280, 536)] {
            assert_eq!(MovieQuality::of(width, height), Some(MovieQuality::FullHd));
            assert_eq!(resolution_label(width, height, true).as_deref(), Some("1080p"));
        }
        assert_eq!(MovieQuality::of(0, 1080), None);
        assert_eq!(resolution_label(3840, 0, true), None);
        assert_eq!(resolution_label(1280, 536, false).as_deref(), Some("720p"));
        assert_eq!(resolution_label(2560, 1440, false).as_deref(), Some("2560×1440"));
        for width in 1..=4096 {
            let quality = MovieQuality::of(width, 800);
            assert_ne!(MovieFilter::Uhd.matches(quality), MovieFilter::FullHd.matches(quality));
        }
    }

    #[test]
    fn home_returns_to_the_drive_picker() {
        let mut browser = Browser::new();
        let volume = Volume { path: "/tmp/usb/sda/sda1".into(), label: None, mount_id: Some(42), system: true };
        let folder = |path, selected, names: &[&str]| Folder {
            volume: volume.clone(), path, selected, collection: None, scroll: 0.0,
            entries: names.iter().map(|name| Entry { name: (*name).into(), kind: Kind::Folder, modified: None, size: None }).collect(),
        };
        browser.scan.volumes.push(volume.clone());
        browser.folders.push(folder(volume.path.clone(), 1, &["Pel·lícules", "Sèries"]));
        browser.folders.push(folder(volume.path.join("Sèries"), 0, &[]));
        let scan_at = browser.next_scan;
        browser.open_nav_tab(0);
        assert!(browser.folders.is_empty());
        assert_eq!(browser.volume(), Some(&volume));
        assert_eq!(browser.selected, 0);
        assert!(browser.pending.is_none());
        // Home invalidates stale NTFS paths and requests an immediate rescan.
        // Compare ordering, not exact Instant equality: Instant::now() advances.
        assert!(browser.next_scan >= scan_at);
    }

    #[test]
    fn home_drive_panel_keeps_every_usb_reachable() {
        let mut browser = Browser::new();
        browser.scan.volumes = (0..9).map(|index| Volume {
            path: format!("/tmp/usb/sda/sda{index}").into(), label: None, mount_id: None, system: false,
        }).collect();
        for index in 0..9 {
            assert_eq!(browser.selected, index);
            let card = browser.tile_rect(index);
            assert_eq!(card.x, HOME_DISK_X);
            assert!(card.x + card.w <= 1920.0 - plx_ui::consts::MARGIN_X);
            assert!(card.y >= HOME_HEADER_TOP && card.y + card.h <= HOME_DISK_BOTTOM);
            assert_eq!(browser.card_at(card.x + 20.0, card.y + 20.0), Some(index));
            browser.key(Key::Down, false);
        }
    }

    #[test]
    fn episode_delete_prompt_requires_end_of_file_and_defaults_to_keep() {
        let mut browser = Browser::new();
        let volume = Volume { path: "/tmp/usb/sda/sda1".into(), label: None, mount_id: Some(42), system: true };
        browser.scan.volumes.push(volume.clone());
        let episode = volume.path.join("Series/Una serie/Una serie S01E02.mkv");
        let play = |path| PlayingVideo {
            volume: volume.clone(), path, key: None, position: 0, duration: None, saved: 0,
            next_save: Instant::now(), seen_player: true, observed_playhead: true, paused: false,
        };
        browser.playing = Some(play(episode.clone()));
        browser.finish_video(false);
        assert!(browser.dialog.is_none());

        browser.playing = Some(play(volume.path.join("Películas/Película.mkv")));
        browser.finish_video(true);
        assert!(browser.dialog.is_none());

        browser.playing = Some(play(episode.clone()));
        browser.finish_video(true);
        assert!(matches!(browser.dialog.as_ref(),
            Some(Dialog::Delete { path, finished_episode: true, .. }) if path == &episode));
        assert_eq!(browser.option, 0);
        browser.choose_option();
        assert!(browser.dialog.is_none());
        assert!(browser.pending.is_none());
    }

    #[test]
    fn delete_from_video_menu_defaults_to_cancel() {
        let mut browser = Browser::new();
        let volume = Volume { path: "/tmp/usb/sda/sda1".into(), label: None, mount_id: Some(42), system: true };
        let path = volume.path.join("Pel·lícula.mkv");
        browser.scan.volumes.push(volume.clone());
        browser.folders.push(Folder {
            path: volume.path.clone(), volume, collection: None, selected: 0, scroll: 0.0,
            entries: vec![disk::media_entry("Pel·lícula.mkv".into(), false).unwrap()],
        });
        browser.open();
        assert!(matches!(browser.dialog.as_ref(), Some(Dialog::Video { path: selected, .. }) if *selected == path));
        assert_eq!(browser.option, 0);
        browser.option = 1;
        browser.choose_option();
        assert!(matches!(browser.dialog.as_ref(), Some(Dialog::Delete { path: selected, .. }) if *selected == path));
        assert_eq!(browser.option, 0);
        browser.choose_option();
        assert!(matches!(browser.dialog, Some(Dialog::Video { .. })));
        assert!(browser.pending.is_none());
        assert_eq!(browser.len(), 1);
    }

    fn media() -> plx_play_feed::LocalMedia {
        plx_play_feed::LocalMedia {
            title: None,
            duration_secs: None,
            video: "hevc".into(), audio: "eac3".into(), audio_channels: 6,
            audio_rate: 48_000, audio_depth: 16, audio_stream_index: 1,
            audio_language: Some("eng".into()), audio_title: "English 5.1".into(),
            audio_tracks: vec![plx_play_feed::LocalAudio {
                index: 1, codec: "eac3".into(), language: Some("eng".into()), title: "English 5.1".into(),
                channels: 6, rate: 48_000, depth: 16, default: true,
            }],
            width: 3840, height: 2160, fps: 23.976, dovi: None,
            subtitles: vec![
                plx_play_feed::LocalSubtitle { stream_index: 3, codec: "subrip".into(),
                    language: Some("spa".into()), title: "Full".into(),
                    forced: false, default: true, sdh: false },
                plx_play_feed::LocalSubtitle { stream_index: 5, codec: "ass".into(),
                    language: Some("eng".into()), title: "Signs".into(),
                    forced: true, default: false, sdh: false },
                plx_play_feed::LocalSubtitle { stream_index: 7, codec: "hdmv_pgs_subtitle".into(),
                    language: Some("eng".into()), title: "SDH".into(),
                    forced: false, default: false, sdh: true },
            ],
        }
    }

    #[test]
    fn movie_titles_discard_container_credits_and_filename_release_tags() {
        use std::path::Path;
        for (path, tag, expected) in [
            ("Pel·lícules/Comanchería.1080p.mkv", "montado por axo", "Comanchería"),
            ("Pel·lícules/T3P4K1NG- Crash.mkv", "T3P4K1NG- Crash", "Crash"),
            ("Pel·lícules/[T3P4K1NG] Crash (2004).mkv", "Crash", "Crash"),
            ("Pel·lícules/Margin Call m-1080p.guerrero.mkv", "Margin Call m-1080p.guerrero", "Margin Call"),
            ("Pel·lícules/Comanchería/title_t00.mkv", "montado por axo", "Comanchería"),
            ("Pel·lícules/title_t00.mkv", "La Odisea", "La Odisea"),
            ("Pel·lícules/Film.1999.1080p.Group.mkv", "credits", "Film"),
            ("Pel·lícules/Blade.Runner.2049.2160p.HDR10+.x265.mkv", "credits", "Blade Runner 2049"),
            ("Pel·lícules/S1m0ne.mkv", "credits", "S1m0ne"),
            ("Pel·lícules/The HDR Story.mkv", "credits", "The HDR Story"),
        ] {
            assert_eq!(movie_title(Path::new(path), Some(tag)), expected, "{path}");
        }
    }

    #[test]
    fn home_movie_titles_use_the_filename_before_container_credits() {
        use std::path::Path;
        assert_eq!(home_file_title(Path::new("Diamantes.En.Bruto.1080p.mkv"), Some("montado por axo")),
            "Diamantes En Bruto");
        assert_eq!(home_file_title(Path::new("Crash (2004) 1080p.mkv"), Some("T3P4K1NG-Crash")), "Crash");
        assert_eq!(home_file_title(Path::new("title_t00.mkv"), Some("La Odisea")), "La Odisea");
        assert_eq!(home_file_title(Path::new("S1m0ne.mkv"), Some("Release group")), "S1m0ne");
    }

    #[test]
    fn hud_title_prefers_the_tag_and_preserves_meaningful_numbers() {
        for (file, title) in [
            ("La Odisea - 2026 - BDremux 4K UHD - HDR.mkv", "La Odisea"),
            ("Blade.Runner.2049.2160p.HDR10+.x265-Group", "Blade Runner 2049"),
            ("1917 (2019) BluRay", "1917"),
            ("2012.1080p.WEB-DL", "2012"),
            ("Se7en", "Se7en"),
            ("Narcos.Mexico.2018.1080p.WEB-DL", "Narcos Mexico"),
            ("Planeta Tierra II", "Planeta Tierra II"),
            ("Nuestro.Planeta.Dolby.Vision.Dolby.Atmos", "Nuestro Planeta"),
            ("El viaje (2025) HDR10", "El viaje"),
            ("千と千尋の神隠し - 2001 - 1080p", "千と千尋の神隠し"),
            ("The HDR Story", "The HDR Story"),
        ] {
            assert_eq!(clean_media_title(file, None), title);
        }
        assert_eq!(clean_media_title("disc_0001", Some("Le Fabuleux Destin d’Amélie Poulain")),
            "Le Fabuleux Destin d’Amélie Poulain");
        assert_eq!(clean_media_title("Avatar", Some("  ")), "Avatar");
    }

    #[test]
    fn hud_format_follows_the_current_feed_and_never_infers_immersive_output() {
        use plx_playback_contract::feed::{FeedDiag, VideoFormat};
        let mut feed = FeedDiag { video_format: VideoFormat::DolbyVision as u8,
            load_a: 2, decoded_audio_channels: 6, ..FeedDiag::IDLE };
        assert_eq!(formats_from_feed(feed, true), Default::default(), "the Load alone is insufficient");
        feed.load_completed = true;
        feed.fed_a = 1;
        feed.fed_v = 1;
        assert_eq!(formats_from_feed(feed, false).video, None);
        assert_eq!(formats_from_feed(feed, true).video, Some("Dolby Vision"));
        assert_eq!(formats_from_feed(feed, true).audio, Some("Dolby Digital Plus"));
        assert_eq!(formats_from_feed(feed, true).channels, Some("6 ch"));
        feed.video_format = VideoFormat::Hdr10 as u8;
        feed.load_a = 5;
        feed.decoded_audio_channels = 2;
        let after = formats_from_feed(feed, false);
        assert_eq!((after.video, after.audio, after.channels), (Some("HDR10"), Some("PCM"), Some("2.0")));
    }

    #[test]
    fn embedded_usb_subtitle_rows_preserve_file_order_language_and_flags() {
        let item = local_playing_item(&media(), plx_data::metadata::Dovi::NONE);
        assert_eq!(item.subs.len(), 3);
        assert_eq!(item.audio.len(), 1);
        assert_eq!(item.audio[0].lang_code, "eng");
        assert_eq!(item.subs.iter().map(|s| s.index).collect::<Vec<_>>(), [3, 5, 7]);
        assert_eq!(item.subs[0].lang_code, "spa");
        assert!(item.subs[0].default);
        assert!(item.subs[1].forced);
        assert!(item.subs[2].sdh);
        for index in 0..item.subs.len() {
            assert_eq!(plx_data::metadata::sub_render_ordinal(&item.subs, index), index as i32);
        }
        // The existing track menu reads only MetadataView::playing(): test the
        // same store command used by the actual USB playback landing.
        let mut store = plx_data::stores::metadata::MetadataStore::default();
        assert!(store.run(plx_data::stores::metadata::MetadataCmd::InstallPlaying(Some(item))));
        let published = store.view().playing().expect("USB tracks must reach the menu store");
        assert_eq!(published.subs.len(), 3);
        assert!(published.subs[1].forced);
    }

    #[test]
    fn forced_only_uses_the_playing_audio_language_and_never_picks_full() {
        let item = local_playing_item(&media(), plx_data::metadata::Dovi::NONE);
        let names = plx_data::metadata::track_names::TrackNames::new();
        assert_eq!(plx_media::route::forced_only_pick(&item.subs, &names, "eng", ""), Some(1));
        assert_eq!(plx_media::route::forced_only_pick(&item.subs, &names, "spa", ""), None);
        assert_eq!(initial_forced_subtitle(&item, false), None);
        assert_eq!(initial_forced_subtitle(&item, true), Some((6, 1)));
        let mut other = item;
        other.audio[0].lang_code = "spa".into();
        assert_eq!(initial_forced_subtitle(&other, true), Some((0, -1)));
    }

    #[test]
    fn a_video_without_subtitles_does_not_invent_tracks() {
        let mut input = media();
        input.audio_stream_index = -1;
        input.audio_tracks.clear();
        input.subtitles.clear();
        let item = local_playing_item(&input, plx_data::metadata::Dovi::NONE);
        assert!(item.audio.is_empty());
        assert!(item.subs.is_empty());
    }
}
