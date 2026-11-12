//! USB browsing and its off-frame filesystem jobs.
mod art;
mod continuity;
mod disk;

use art::Cover;
use super::{App, Key, playback};
use disk::{Entry, Kind, Scan, Volume};
use plx_gfx::{gfx, text::TtfMeasure};
use plx_machine::machine::Measure;
use plx_ui::{
    icons::{self, Icon},
    label::{HAlign, Label},
    poster_grid::{self, Card, Geom, GridPop, USB_STYLE},
    screen::Dir,
    theme, widgets, Painter, Rect, Spring,
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
const WATCHED: u64 = u64::MAX;
const FAVORITE: u64 = WATCHED - 1;
const PACKED: u64 = 1 << 63;
const PLAYHEAD_MASK: u64 = (1 << 43) - 1;
const DURATION_MASK: u64 = (1 << 20) - 1;

fn motion_alpha(started: Instant) -> f32 {
    let t = (started.elapsed().as_secs_f32() / MOTION.as_secs_f32()).min(1.0);
    t * t * (3.0 - 2.0 * t)
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
const GRID_BOTTOM: f32 = 965.0;
const GRID_WIDTH: f32 = 1920.0 - 2.0 * plx_ui::consts::MARGIN_X;

fn grid_geometry(len: usize, size: usize) -> Geom {
    let (limit, gap, max_width) = [(8, 22.0, 214.0), (10, 18.0, 174.0), (6, 26.0, 270.0)][size];
    let rows = len.max(1).div_ceil(limit);
    let cols = len.max(1).div_ceil(rows);
    let width = ((GRID_WIDTH - (cols - 1) as f32 * gap) / cols as f32).min(max_width);
    Geom { cols, left: plx_ui::consts::MARGIN_X, w: width, h: width * 1.5, gap, value_line: false }
}

fn tr(es: &'static str, ca: &'static str) -> &'static str {
    if i18n::current().language() == Language::Ca { ca } else { es }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action { ToggleFavorite, Select, DeleteFolders }

enum Mutation { Delete, Eject }

const PANEL: Rect = Rect::new(510.0, 292.0, 900.0, 496.0);
const OPTIONS: [Rect; 2] = [
    Rect::new(555.0, 676.0, 390.0, 66.0),
    Rect::new(975.0, 676.0, 390.0, 66.0),
];
const VIDEO_OPTIONS: [Rect; 4] = [
    Rect::new(530.0, 676.0, 190.0, 66.0),
    Rect::new(731.0, 676.0, 190.0, 66.0),
    Rect::new(932.0, 676.0, 206.0, 66.0),
    Rect::new(1149.0, 676.0, 241.0, 66.0),
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

// Stable six-action PlxNative-style navigation track.
const NAV_PILL: Rect = Rect::new(352.0, 32.0, 1216.0, 70.0);
const NAV_TABS: [Rect; 6] = [
    Rect::new(365.0, 42.0, 125.0, 48.0),
    Rect::new(500.0, 42.0, 205.0, 48.0),
    Rect::new(715.0, 42.0, 155.0, 48.0),
    Rect::new(880.0, 42.0, 195.0, 48.0),
    Rect::new(1085.0, 42.0, 170.0, 48.0),
    Rect::new(1265.0, 42.0, 290.0, 48.0),
];
// Folder-specific controls sit in the content header, not the navigation.
const ACTIONS: [(Action, Rect); 3] = [
    (Action::ToggleFavorite, Rect::new(1300.0, 222.0, 520.0, 45.0)),
    (Action::Select, Rect::new(1030.0, 222.0, 185.0, 45.0)),
    (Action::DeleteFolders, Rect::new(1225.0, 222.0, 185.0, 45.0)),
];

const UNIFIED_SETTINGS_COUNT: usize = 8;
fn unified_setting_rect(index: usize) -> Rect {
    Rect::new(825.0, 168.0 + index as f32 * 98.0, 975.0, 58.0)
}

fn unified_setting_text(index: usize, settings: &Settings) -> (&'static str, &'static str, String) {
    let on = |enabled: bool| if enabled { tr("Activado", "Activat") } else { tr("Desactivado", "Desactivat") };
    match index {
        0 => (tr("Ordenar biblioteca", "Ordena la biblioteca"),
            tr("Cambia el orden de archivos y carpetas.", "Canvia l'ordre dels fitxers i les carpetes."),
            [tr("Nombre", "Nom"), tr("Fecha", "Data"), tr("Tamaño", "Mida")][settings.sort].into()),
        1 => (tr("Mostrar extensiones", "Mostra les extensions"),
            tr("Muestra el tipo de archivo en los títulos.", "Mostra el tipus de fitxer als títols."),
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
            if settings.language == Preference::Ca { "Català" } else { "Español" }.into()),
        6 => (tr("Color de acento", "Color de ressaltat"),
            tr("Personaliza el color de selección.", "Personalitza el color de selecció."),
            [tr("Morado", "Lila"), tr("Azul", "Blau"), tr("Gris neutro", "Gris neutre")][settings.accent].into()),
        _ => (tr("Tamaño de tarjetas", "Mida de les targetes"),
            tr("Ajusta el tamaño de los pósteres.", "Ajusta la mida dels pòsters."),
            [tr("Normal", "Normal"), tr("Compacto", "Compacte"), tr("Grande", "Gran")][settings.cards].into()),
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
    Delete { volume: Volume, path: PathBuf },
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

struct DetailsRequest {
    volume: Volume,
    key: DetailKey,
    reply: Receiver<plx_play_feed::LocalVideoDetails>,
}

pub(super) struct Browser {
    scan: Scan,
    folders: Vec<Folder>,
    selected: usize,
    scroll: Spring,
    pop: GridPop,
    status: String,
    hour: Option<i32>,
    action: Option<Action>,
    dialog: Option<Dialog>,
    option: usize,
    mutating: bool,
    selection: Option<BTreeSet<OsString>>,
    progress: Option<Receiver<(usize, usize)>>,
    settings: Settings,
    settings_job: Option<SettingsJob>,
    settings_started: bool,
    pending: Option<Receiver<DiskResult>>,
    scanning: Option<Receiver<Scan>>,
    details_job: Option<DetailsRequest>,
    details: BTreeMap<DetailKey, plx_play_feed::LocalVideoDetails>,
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
    nav_strip: widgets::TabStrip,
    discovery_cache: BTreeMap<(PathBuf, u64, usize), Vec<PathBuf>>,
    favorite_section: (bool, Instant, f32),
    dialog_started: Option<Instant>,
    button_motion: Option<(Rect, Instant)>,
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
            scan: Scan::default(), folders: Vec::new(), selected: 0,
            scroll: Spring::at(0.0), pop: GridPop::new(), status: String::new(),
            hour: None, action: None, dialog: None, option: 0, mutating: false,
            selection: None, progress: None,
            settings: Settings { language: Preference::Es, forced_subtitles: false, spanish_audio: false, auto_next: true, accent: 0, cards: 0, sort: 0, extensions: false },
            settings_job: None, settings_started: false,
            pending: None, scanning: None, next_scan: now, last_tick: now,
            details_job: None, details: BTreeMap::new(),
            facts: None, facts_job: None, prepared: None, ready: None, playing: None,
            progress_job: None, queued_progress: Default::default(), countdown_display: 0,
            history: BTreeMap::new(), history_job: None, history_loaded: BTreeSet::new(),
            queued_history_clears: BTreeSet::new(), loading_favorites: false, favorite_view: None,
            media_view: None,
            nav_tab: 0, nav_focus: None, nav_strip: widgets::TabStrip::new(), discovery_cache: BTreeMap::new(),
            favorite_section: (false, now, 0.0),
            dialog_started: None, button_motion: None, favorite_motion: None,
            ejecting: None, eject_feedback: None, eject_uncertain: false,
            estimate: None, next_estimate: None, estimate_anchor: None,
        }
    }

    fn len(&self) -> usize {
        if let Some(view) = self.media_cards() { return view.paths.len(); }
        self.favorite_cards().map_or_else(|| self.folders.last().map_or(self.scan.volumes.len(), |folder| folder.entries.len()), |view| view.paths.len())
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
            self.status = format!("{} {processed}/{total}", tr("Carpetas procesadas", "Carpetes processades"));
            changed = true;
        }
        match self.pending.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => {
                self.pending = None;
                self.mutating = false;
                self.progress = None;
                changed = true;
                match result {
                    DiskResult::Folder(volume, path, Ok(mut entries)) => {
                        disk::sort_entries_by(&mut entries, self.settings.sort);
                        let collection = path.strip_prefix(&volume.path).ok().and_then(art::collection);
                        self.folders.push(Folder { volume, path, entries, collection, selected: 0, scroll: 0.0 });
                        self.select(0, true);
                        self.status.clear();
                    }
                    DiskResult::Folder(_, _, Err(error)) => {
                        plx_base::eventlog::log(&format!("cineusb: USB folder listing failed: {error}"));
                        self.status = tr("No se pudo abrir la carpeta.", "No s’ha pogut obrir la carpeta.").into();
                    }
                    DiskResult::Video(Ok(media)) => {
                        if media.resume > 0 {
                            self.dialog = Some(Dialog::Resume(media.resume));
                            self.option = 0;
                            self.prepared = Some(media);
                        } else { video = Some(media); }
                        self.status.clear();
                    }
                    DiskResult::Video(Err(error)) => {
                        plx_base::eventlog::log(&format!("cineusb: video probe failed: {error}"));
                        self.status = tr("No se pudo abrir el vídeo.", "No s’ha pogut obrir el vídeo.").into();
                    }
                    DiskResult::Delete(path, result) => {
                        if result.is_ok() {
                            if let Some(volume) = self.volume().cloned() { self.remove_history_under(&volume, &path); }
                        }
                        if result.is_ok() {
                            if let Some(folder) = self.folders.last_mut() {
                                folder.entries.retain(|entry| path.file_name() != Some(entry.name.as_os_str()));
                            }
                            self.select(self.selected, true);
                        }
                        self.facts = None;
                        self.finish_mutation(result, tr("Archivo eliminado.", "Fitxer esborrat."),
                            tr("No se pudo eliminar el archivo. Comprueba si el disco admite escritura.",
                                "No s’ha pogut esborrar el fitxer. Comprova si es pot escriure al disc."));
                    }
                    DiskResult::DeleteFolders { removed, total, error, entries } => {
                        if let Some(folder) = self.folders.last() {
                            let (volume, parent) = (folder.volume.clone(), folder.path.clone());
                            for name in &removed { self.remove_history_under(&volume, &parent.join(name)); }
                        }
                        self.facts = None;
                        if let Some(folder) = self.folders.last_mut() {
                            match entries {
                                Ok(mut entries) => {
                                    disk::sort_entries_by(&mut entries, self.settings.sort);
                                    folder.entries = entries;
                                }
                                Err(_) => folder.entries.retain(|entry| !removed.contains(&entry.name)),
                            }
                        }
                        self.selection = None;
                        self.action = None;
                        self.select(self.selected, true);
                        self.status = format!("{}: {}/{total}", tr("Carpetas eliminadas", "Carpetes esborrades"), removed.len());
                        if let Some(error) = error {
                            self.status.push_str(" · ");
                            self.status.push_str(&error);
                        }
                        self.scanning = None;
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
                        if self.volume() == Some(&volume) {
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
                Some(Ok(scan)) => {
                    self.scanning = None;
                    // A temporary Homebrew failure must not bring back an old mount from the jail.
                    if (!self.scan.system || scan.system)
                        && (self.scan != scan || self.status == tr("Buscando discos USB…", "Buscant discos USB…")) {
                        let selected = self.scan.volumes.get(self.selected).cloned();
                        self.details.clear();
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
                        if self.folders.is_empty() && self.favorite_view.is_none() {
                            let index = selected.and_then(|volume| self.scan.volumes.iter().position(|item| *item == volume));
                            self.select(index.unwrap_or_default(), true);
                        }
                        if self.status == tr("Buscando discos USB…", "Buscant discos USB…") { self.status.clear(); }
                        changed = true;
                    }
                }
                Some(Err(TryRecvError::Disconnected)) => self.scanning = None,
                _ => {}
            }
            if self.scanning.is_none() && now >= self.next_scan {
                self.next_scan = now + Duration::from_secs(5);
                let hour = i18n::local_hour();
                changed |= hour != self.hour;
                self.hour = hour;
                let (send, receive) = mpsc::sync_channel(1);
                if plx_base::task::spawn_off_frame("cineusb-volumes", move |_| {
                    let _ = send.send(scan_volumes());
                }) {
                    self.scanning = Some(receive);
                }
            }
        }
        changed |= self.update_volume_facts();
        changed |= self.update_history();
        if self.loading_favorites && self.facts.as_ref().is_some_and(|(disk, (_, identity))|
            self.volume() == Some(disk) && identity.as_ref().is_none_or(|identity| self.history_loaded.contains(&history_prefix(identity)))) {
            self.loading_favorites = false;
            self.show_favorites();
        }
        if self.dialog.is_some() {
            let started = *self.dialog_started.get_or_insert(now);
            changed |= now.duration_since(started) < MOTION;
        } else { self.dialog_started = None; }
        changed |= self.button_motion.is_some_and(|(_, started)| now.duration_since(started) < MOTION);
        changed |= self.favorite_motion.as_ref().is_some_and(|(_, _, started)| now.duration_since(*started) < MOTION);
        changed |= self.favorite_section.1.elapsed() < MOTION;
        let selected_tab = if self.favorite_section.0 { 3 } else { self.nav_tab };
        let focused_tab = self.nav_focus.unwrap_or(selected_tab);
        self.nav_strip.update(selected_tab as i32, focused_tab as i32,
            |index| NAV_TABS.get(index).map(|rect| (rect.x, rect.w)), widgets::SelMark::Travels, dt);
        changed |= self.nav_strip.moving();
        if self.eject_feedback.as_ref().is_some_and(|(_, success, start)| *success && start.elapsed() >= Duration::from_secs(3)) {
            self.eject_feedback = None;
            changed = true;
        }
        changed |= self.eject_feedback.as_ref().is_some_and(|(_, success, start)|
            *success && start.elapsed() < Duration::from_secs(3));
        changed |= self.ejecting.as_ref().is_some_and(|(_, start)| start.elapsed() < Duration::from_millis(800));
        changed |= self.update_video_details();
        if self.action.is_some_and(|action| !self.action_available(action)) {
            self.action = None;
            changed = true;
        }

        let target = self.scroll_target();
        let before = self.scroll.pos;
        self.scroll.step(target, 240.0, dt);
        self.scroll.land(target, dt);
        let focused = (self.len() > 0).then_some(Card { cell: self.selected, key: self.selected as u64 });
        let scale = self.pop.scale(self.selected, true, &USB_STYLE);
        self.pop.tick(focused, &USB_STYLE, dt);
        if changed || before != self.scroll.pos || scale != self.pop.scale(self.selected, true, &USB_STYLE) {
            plx_machine::idle::invalidate();
        }
        video
    }

    fn scroll_target(&self) -> f32 {
        let grid = grid_geometry(self.len(), self.settings.cards);
        let row = self.selected / grid.cols;
        let viewport = GRID_BOTTOM - TOP - 22.0;
        let top = row as f32 * grid.pitch();
        self.scroll.pos.clamp((top + grid.h - viewport).max(0.0), top)
    }

    fn tile_rect(&self, index: usize) -> Rect {
        let mut grid = grid_geometry(self.len(), self.settings.cards);
        let first = index / grid.cols * grid.cols;
        let count = self.len().saturating_sub(first).min(grid.cols).max(1);
        let row_width = count as f32 * grid.w + (count - 1) as f32 * grid.gap;
        grid.left = (1920.0 - row_width) * 0.5;
        poster_grid::cell_in(&grid, index, TOP, self.scroll.pos, &[])
    }

    fn update_volume_facts(&mut self) -> bool {
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
        if !completed || !self.settings.auto_next { return; }
        let Some(folder) = self.folders.last().filter(|folder|
            folder.volume == playing.volume && playing.path.parent() == Some(folder.path.as_path())) else { return };
        let Some(next) = playing.path.file_name().and_then(|name| continuity::next_episode(name, &folder.entries)) else { return };
        self.countdown_display = 10;
        self.dialog = Some(Dialog::Next { volume: playing.volume, path: folder.path.join(next),
            deadline: Instant::now() + Duration::from_secs(10) });
        self.option = 1;
        self.action = None;
        plx_machine::idle::invalidate();
    }

    fn identity(&self, volume: &Volume) -> Option<&str> {
        self.facts.as_ref().filter(|(disk, _)| disk == volume).and_then(|(_, (_, identity))| identity.as_deref())
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

    fn change_history(&mut self, key: String, value: u64) {
        // The identity is separated from the filename by an encoded NUL. Only look at byte pairs.
        let Some(end) = key.as_bytes().chunks_exact(2).position(|pair| pair == b"00") else { return };
        let prefix = &key[..2 * (end + 1)];
        let records = self.history.entry(prefix.to_owned()).or_default();
        records.insert(key.clone(), value);
        self.queued_progress.insert(key, value);
        self.flush_progress(false);
        plx_machine::idle::invalidate();
    }

    fn remove_history_under(&mut self, volume: &Volume, path: &std::path::Path) {
        let Some(identity) = self.identity(volume) else { return };
        let Some(base) = path_key(identity, volume, path) else { return };
        let prefix = history_prefix(identity);
        if let Some(records) = self.history.get_mut(&prefix) {
            records.retain(|key, _| !history_matches(key, &base));
        }
        self.queued_progress.retain(|key, _| !history_matches(key, &base));
        self.queued_history_clears.insert(base);
        self.flush_progress(false);
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
    }

    fn media_cards(&self) -> Option<&FavoriteView> {
        self.media_view.as_ref().filter(|view| view.depth == self.folders.len())
    }

    fn leave_media(&mut self) {
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
    }

    fn open_nav_tab(&mut self, tab: usize) {
        if self.pending.is_some() || self.mutating { return; }
        self.nav_focus = None;
        self.action = None;
        self.option = 0;
        match tab {
            0 => {
                let selected = self.volume().and_then(|volume|
                    self.scan.volumes.iter().position(|item| item == volume)).unwrap_or(self.selected);
                self.leave_media();
                self.leave_favorites();
                self.folders.clear();
                self.loading_favorites = false;
                self.nav_tab = 0;
                self.restore_view(selected, 0.0);
                self.status.clear();
            }
            1 | 2 => {
                let Some(volume) = self.volume().cloned() else {
                    self.status = tr("Selecciona primero un disco USB.", "Tria primer un disc USB.").into();
                    return;
                };
                let key = (volume.path.clone(), volume.mount_id.unwrap_or(0), tab);
                if let Some(paths) = self.discovery_cache.get(&key).cloned() {
                    self.enter_media(volume, tab, paths);
                } else {
                    self.status = tr("Buscando carpetas multimedia…", "Buscant carpetes multimèdia…").into();
                    self.request(move || DiskResult::Discovered(volume.clone(), tab, discover_media_folders(&volume, tab)));
                }
            }
            3 => {
                if self.volume().is_none() {
                    self.status = tr("Selecciona primero un disco USB.", "Tria primer un disc USB.").into();
                    return;
                }
                self.leave_media();
                self.nav_tab = 3;
                self.set_favorite_section(true);
                self.show_favorites();
            }
            4 => {
                self.dialog = Some(Dialog::Settings);
                self.status.clear();
            }
            5 => {
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
        self.nav_tab = 3;
        self.action = None;
        self.restore_view(0, 0.0);
    }

    fn leave_favorites(&mut self) {
        if let Some(view) = self.favorite_view.take() {
            self.folders.truncate(view.depth);
            self.restore_view(view.library_selected, view.library_scroll);
        }
        self.set_favorite_section(false);
        if self.nav_tab == 3 { self.nav_tab = 0; }
    }

    fn restore_view(&mut self, selected: usize, scroll: f32) {
        self.scroll = Spring::at(scroll);
        self.select(selected, true);
    }

    // Resolve a season back to its series. Never mark the category folder itself.
    fn series_root(&self) -> Option<PathBuf> {
        let folder = self.folders.last()?;
        if folder.path == folder.volume.path || self.favorite_cards().is_some() { return None; }
        let has_content = folder.entries.iter().any(|entry| entry.kind == Kind::Video
            || (entry.kind == Kind::Folder && season_folder(&entry.name.to_string_lossy())));
        if !has_content { return None; }
        let relative = folder.path.strip_prefix(&folder.volume.path).ok()?;
        let parts: Vec<_> = relative.components().filter_map(|component| match component {
            std::path::Component::Normal(name) => Some(name),
            _ => None,
        }).collect();
        if let Some(index) = parts.iter().position(|name| media_folder_score(&name.to_string_lossy(), 2) > 0) {
            if parts.get(index + 1).is_some() {
                let mut root = folder.volume.path.clone();
                for part in &parts[..=index + 1] { root.push(part); }
                return Some(root);
            }
            return None;
        }
        if folder.path.file_name().is_some_and(|name| season_folder(&name.to_string_lossy())) {
            return folder.path.parent().filter(|parent| *parent != folder.volume.path).map(std::path::Path::to_path_buf);
        }
        // Without a recognized category, require episode-like titles or season folders.
        folder.entries.iter().any(|entry| (entry.kind == Kind::Video && continuity::episode(&entry.name).is_some())
            || (entry.kind == Kind::Folder && season_folder(&entry.name.to_string_lossy())))
            .then(|| folder.path.clone())
    }

    fn folder_favorite_key(&self) -> Option<String> {
        let folder = self.folders.last()?;
        let root = self.series_root()?;
        let identity = self.identity(&folder.volume)?;
        self.history_loaded.contains(&history_prefix(identity)).then_some(())?;
        favorite_key(identity, &folder.volume, &root)
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

    fn toggle_folder_favorite(&mut self) {
        let Some(key) = self.folder_favorite_key() else { return };
        let marked = self.history_value(&key) == Some(FAVORITE);
        let path = self.series_root();
        self.change_history(key, if marked { 0 } else { FAVORITE });
        if let Some(path) = path {
            if let Some(view) = &mut self.favorite_view {
                if marked { view.paths.retain(|item| *item != path); }
                else if !view.paths.contains(&path) {
                    view.paths.push(path.clone());
                    view.paths.sort();
                }
            }
            self.favorite_motion = Some((path, !marked, Instant::now()));
        }
        self.status = if marked { tr("Serie quitada de favoritos.", "Sèrie treta dels favorits.") }
            else { tr("Serie añadida a favoritos.", "Sèrie afegida als favorits.") }.into();
    }

    fn detail_key(folder: &Folder, entry: &Entry) -> DetailKey {
        (folder.path.join(&entry.name), entry.size, entry.modified)
    }

    fn detail_key_for(&self, path: &std::path::Path) -> DetailKey {
        self.folders.last().and_then(|folder| folder.entries.iter()
            .find(|entry| path.file_name() == Some(entry.name.as_os_str()))
            .map(|entry| Self::detail_key(folder, entry)))
            .unwrap_or_else(|| (path.to_path_buf(), None, None))
    }

    fn update_video_details(&mut self) -> bool {
        let mut changed = false;
        if let Some(job) = &self.details_job {
            match job.reply.try_recv() {
                Err(TryRecvError::Empty) => return false,
                result => {
                    if self.scan.volumes.contains(&job.volume) {
                        if self.details.len() >= 1024 { self.details.clear(); }
                        self.details.insert(job.key.clone(), result.unwrap_or_default());
                    }
                    self.details_job = None;
                    changed = true;
                }
            }
        }
        let target = match &self.dialog {
            Some(Dialog::Video { volume, path }) => {
                let key = self.detail_key_for(path);
                (!self.details.contains_key(&key)).then(|| (volume.clone(), key))
            }
            None if self.favorite_cards().is_none() && self.pending.is_none() => {
                self.folders.last().and_then(|folder| {
                    let grid = grid_geometry(self.len(), self.settings.cards);
                    poster_grid::visible_in(&grid, self.len(), TOP, self.scroll.pos)
                        .filter_map(|index| folder.entries.get(index))
                        .filter(|entry| entry.kind == Kind::Video)
                        .map(|entry| Self::detail_key(folder, entry))
                        .find(|key| !self.details.contains_key(key))
                        .map(|key| (folder.volume.clone(), key))
                })
            }
            _ => None,
        };
        let Some((volume, key)) = target else { return changed };
        let mount = system_mount(&volume);
        let file = key.0.clone();
        let (send, reply) = mpsc::sync_channel(1);
        if plx_base::task::spawn_off_frame("cineusb-video-details", move |_| {
            let details = open_for_details(&file, mount).map_or_else(|_| Default::default(), |file| {
                plx_play_feed::probe_local_details(&file)
            });
            let _ = send.send(details);
        }) {
            self.details_job = Some(DetailsRequest { volume, key, reply });
        } else {
            self.details.insert(key, Default::default());
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
                let sort_changed = self.settings.sort != settings.sort;
                let size_changed = self.settings.cards != settings.cards;
                self.settings = settings;
                if sort_changed {
                    let last = self.folders.len().saturating_sub(1);
                    for (index, folder) in self.folders.iter_mut().enumerate() {
                        let old = if index == last { self.selected } else { folder.selected };
                        folder.selected = disk::resort(&mut folder.entries, old, settings.sort);
                        if index == last { self.selected = folder.selected; }
                    }
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
        if plx_base::task::spawn_off_frame("cineusb-settings", move |_| {
            let _ = reply.send(save_settings(settings).map(|()| settings));
        }) {
            self.settings_job = Some(SettingsJob::Save(receive));
        } else {
            self.status = tr("No se pudieron guardar los ajustes.", "No s’han pogut desar els ajustos.").into();
        }
    }

    fn volume(&self) -> Option<&Volume> {
        self.media_cards().map(|view| &view.volume)
            .or_else(|| self.favorite_cards().map(|view| &view.volume))
            .or_else(|| self.folders.last().map(|folder| &folder.volume))
            .or_else(|| self.scan.volumes.get(self.selected))
    }

    fn action_available(&self, action: Action) -> bool {
        match action {
            Action::ToggleFavorite => self.selection.is_none() && self.folder_favorite_key().is_some(),
            Action::Select => self.selection.is_some() || (self.favorite_cards().is_none() && self.folders.last().is_some_and(|folder| {
                folder.path != folder.volume.path && system_mount(&folder.volume).is_some() && folder.entries.iter().any(|entry| entry.kind == Kind::Folder)
            })),
            Action::DeleteFolders => self.selection.as_ref().is_some_and(|marked| !marked.is_empty()),
        }
    }

    fn open_action(&mut self, action: Action) {
        if self.pending.is_some() || !self.action_available(action) { return; }
        self.option = 0;
        self.status.clear();
        self.loading_favorites = false;
        self.dialog = match action {
            Action::ToggleFavorite => { self.toggle_folder_favorite(); None }
            Action::Select => {
                self.selection = self.selection.is_none().then(BTreeSet::new);
                self.action = None;
                None
            }
            Action::DeleteFolders => Some(Dialog::DeleteFolders),
        };
    }

    fn cycle_unified_setting(&mut self, direction: i32) {
        let mut next = self.settings;
        let step = |value: usize, count: usize| (value as i32 + direction).rem_euclid(count as i32) as usize;
        match self.option {
            0 => next.sort = step(next.sort, 3),
            1 => next.extensions = !next.extensions,
            2 => next.auto_next = !next.auto_next,
            3 => next.forced_subtitles = !next.forced_subtitles,
            4 => next.spanish_audio = !next.spanish_audio,
            5 => next.language = if next.language == Preference::Ca { Preference::Es } else { Preference::Ca },
            6 => next.accent = step(next.accent, 3),
            7 => next.cards = step(next.cards, 3),
            _ => return,
        }
        self.change_settings(next);
    }

    fn choose_option(&mut self) {
        if self.settings_job.is_some() { return; }
        self.dialog_started = None;
        match self.dialog.take() {
            Some(Dialog::Settings) => {
                self.dialog = Some(Dialog::Settings);
                self.cycle_unified_setting(1);
            }
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
                if let Some(index) = self.folders.last().and_then(|folder| folder.entries.iter()
                    .position(|entry| Some(entry.name.as_os_str()) == path.file_name())) {
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
                } else if self.option == 3 {
                    if let Some(key) = self.video_favorite_key(&volume, &path) {
                        let target = self.video_favorite_target(&volume, &path);
                        let series = target != path;
                        let marked = self.history_value(&key) == Some(FAVORITE);
                        self.change_history(key, if marked { 0 } else { FAVORITE });
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
                } else if self.option == 1 && system_mount(&volume).is_some() {
                    self.dialog = Some(Dialog::Delete { volume, path });
                    self.option = 0;
                }
            }
            Some(Dialog::Delete { volume, path }) if self.option == 1 => {
                if !self.scan.volumes.contains(&volume) { return; }
                self.status = tr("Eliminando archivo…", "Esborrant el fitxer…").into();
                self.request(move || {
                    let result = mutate_usb(Mutation::Delete, &volume, &path);
                    DiskResult::Delete(path, result)
                });
                self.mutating = self.pending.is_some();
            }
            Some(Dialog::Delete { volume, path }) => {
                self.dialog = Some(Dialog::Video { volume, path });
                self.option = 0;
            }
            Some(Dialog::Eject(volume)) if self.option == 1 => {
                if !self.scan.volumes.contains(&volume) { return; }
                self.status = tr("Expulsando disco…", "Expulsant el disc…").into();
                self.ejecting = Some((volume.clone(), Instant::now()));
                self.eject_feedback = None;
                self.eject_uncertain = false;
                self.request(move || {
                    let result = mutate_usb(Mutation::Eject, &volume, &volume.path);
                    DiskResult::Eject(volume, result)
                });
                self.mutating = self.pending.is_some();
                if !self.mutating { self.ejecting = None; }
            }
            _ => {}
        }
    }

    fn delete_marked_folders(&mut self) {
        let Some(folder) = self.folders.last() else { return };
        if folder.path == folder.volume.path { return; }
        let Some(mount) = system_mount(&folder.volume) else { return };
        let Some(marked) = self.selection.take() else { return };
        let parent = folder.path.clone();
        let (progress, updates) = mpsc::channel();
        let total = marked.len();
        self.status = format!("{} 0/{total}", tr("Carpetas procesadas", "Carpetes processades"));
        self.request(move || {
            let mut removed = Vec::new();
            let mut error = None;
            for (index, name) in marked.into_iter().enumerate() {
                match delete_folder(&parent.join(&name), mount) {
                    Ok(()) => removed.push(name),
                    Err(detail) => { error.get_or_insert(detail); }
                }
                let _ = progress.send((index + 1, total));
            }
            let entries = list_folder(&parent, Some(mount));
            DiskResult::DeleteFolders { removed, total, error, entries }
        });
        self.mutating = self.pending.is_some();
        if self.mutating { self.progress = Some(updates); }
    }

    fn finish_mutation(&mut self, result: Result<(), String>, success: &str, failure: &str) {
        self.scanning = None;
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
        if snap {
            self.scroll = Spring::at(self.scroll_target());
            self.pop.reset();
        }
        if self.len() > 0 {
            self.pop.arm(Card { cell: self.selected, key: self.selected as u64 });
        }
    }

    fn open(&mut self) {
        if self.pending.is_some() { return; }
        if let Some(view) = self.media_cards().or_else(|| self.favorite_cards()) {
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
        if let Some(folder) = self.folders.last() {
            let Some(entry) = folder.entries.get(self.selected) else { return };
            if let Some(marked) = &mut self.selection {
                if entry.kind == Kind::Folder && !marked.remove(&entry.name) {
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
        let title = path.file_stem().or_else(|| path.file_name()).unwrap_or_default().to_string_lossy().into_owned();
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
                PreparedVideo { file, info, volume, path, title, key, resume, saved }
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
        if std::mem::take(&mut self.loading_favorites) && self.favorite_view.is_none() { self.set_favorite_section(false); }
        if let Some(dialog) = self.dialog.take() {
            self.prepared = None;
            self.dialog = match dialog {
                Dialog::Appearance => Some(Dialog::Settings),
                Dialog::Accent | Dialog::Cards | Dialog::Extensions => Some(Dialog::Appearance),
                Dialog::Delete { volume, path } => Some(Dialog::Video { volume, path }),
                _ => None,
            };
            self.option = 0;
            self.status.clear();
            return;
        }
        if self.action.take().is_some() { return; }
        if self.selection.take().is_some() { self.status.clear(); return; }
        if self.pending.take().is_some() {
            if self.favorite_view.is_none() { self.set_favorite_section(false); }
            self.status.clear();
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

    fn click(&mut self, x: f32, y: f32) {
        if self.dialog.is_none() && self.eject_feedback.is_none() && !self.mutating {
            if let Some(tab) = NAV_TABS.iter().position(|rect| hit(*rect, x, y)) {
                self.open_nav_tab(tab);
                return;
            }
        }
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
        if let Some((action, rect)) = ACTIONS.iter().find(|(action, rect)| self.action_available(*action) && hit(*rect, x, y)) {
            self.press_button(*rect);
            self.action = Some(*action);
            self.open_action(*action);
            return;
        }
        // Magic Remote clicks are pointer events on webOS; the standalone USB
        // browser has no shared hit-map. Pick the clicked card when possible,
        // or activate the focused card (the physical OK button may arrive with
        // the pointer parked outside the cards).
        let grid = grid_geometry(self.len(), self.settings.cards);
        let visible = poster_grid::visible_in(&grid, self.len(), TOP, self.scroll.pos);
        if let Some(index) = visible.into_iter().find(|&index| {
            y >= TOP - 14.0 && y < GRID_BOTTOM && hit(self.tile_rect(index), x, y)
        }) {
            self.select(index, false);
            self.action = None;
        }
        if let Some(action) = self.action {
            self.open_action(action);
        } else {
            self.open();
        }
    }

    fn key(&mut self, key: Key, repeat: bool) {
        if let Some(tab) = self.nav_focus {
            match key {
                Key::Left { alt: false } => self.nav_focus = Some(tab.saturating_sub(1)),
                Key::Right { alt: false } => self.nav_focus = Some((tab + 1).min(NAV_TABS.len() - 1)),
                Key::Down => {
                    self.nav_focus = None;
                    self.action = ACTIONS.iter().find(|(action, _)| self.action_available(*action))
                        .map(|(action, _)| *action);
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
                Key::Up if settings => self.option = self.option.saturating_sub(1),
                Key::Down if settings => self.option = (self.option + 1).min(vertical - 1),
                Key::Left { alt: false } if matches!(self.dialog, Some(Dialog::Settings)) => self.cycle_unified_setting(-1),
                Key::Right { alt: false } if matches!(self.dialog, Some(Dialog::Settings)) => self.cycle_unified_setting(1),
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
        if let Some(action) = self.action {
            let actions: Vec<_> = ACTIONS.iter().map(|(action, _)| *action)
                .filter(|action| self.action_available(*action)).collect();
            let index = actions.iter().position(|candidate| *candidate == action).unwrap_or(0);
            match key {
                Key::Left { alt: false } => self.action = actions.get(index.saturating_sub(1)).copied(),
                Key::Right { alt: false } => self.action = actions.get(index + 1).copied().or(Some(action)),
                Key::Down => self.action = None,
                Key::Up => { self.action = None; self.nav_focus = Some(self.nav_tab); },
                Key::Back if !repeat => self.back(),
                Key::Ok if !repeat => {
                    if let Some((_, rect)) = ACTIONS.iter().find(|(item, _)| *item == action) { self.press_button(*rect); }
                    self.open_action(action);
                }
                _ => {}
            }
            return;
        }
        let dir = match key {
            Key::Up if self.selected < grid_geometry(self.len(), self.settings.cards).cols => {
                self.nav_focus = Some(self.nav_tab);
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
        if let Some(index) = poster_grid::neighbour(self.selected, self.len(), grid_geometry(self.len(), self.settings.cards).cols, dir) {
            self.select(index, false);
        }
    }

    fn horizontal_options(&self) -> &'static [Rect] {
        if matches!(self.dialog, Some(Dialog::Video { .. })) { &VIDEO_OPTIONS } else { &OPTIONS }
    }

    fn option_available(&self, index: usize) -> bool {
        match self.dialog.as_ref() {
            Some(Dialog::Video { volume, .. }) if index == 1 => system_mount(volume).is_some(),
            Some(Dialog::Video { volume, path }) if index == 2 => {
                self.identity(volume).is_some_and(|identity| self.history_loaded.contains(&history_prefix(identity)))
                    && self.video_history_key(volume, path).is_some()
            }
            Some(Dialog::Video { volume, path }) if index == 3 => self.video_favorite_key(volume, path).is_some(),
            _ => true,
        }
    }

    fn video_history_key(&self, volume: &Volume, path: &std::path::Path) -> Option<String> {
        let identity = self.identity(volume)?;
        let entry = self.folders.last()?.entries.iter().find(|entry| path.file_name() == Some(entry.name.as_os_str()))?;
        entry_key(identity, volume, path, entry.size?, entry.modified?)
    }

    fn item(&self, index: usize) -> Option<(String, &'static str, Cover)> {
        if let Some(view) = self.media_cards() {
            let path = view.paths.get(index)?;
            let name = path.file_name()?.to_string_lossy().into_owned();
            let cover = path.strip_prefix(&view.volume.path).ok().and_then(art::collection).unwrap_or(Cover::Folder);
            return Some((name, tr("Carpeta", "Carpeta"), cover));
        }
        if let Some(view) = self.favorite_cards() {
            let path = view.paths.get(index)?;
            if is_video_path(path) {
                return Some((path.file_stem()?.to_string_lossy().into_owned(), tr("Vídeo", "Vídeo"), Cover::Movie));
            }
            let name = path.file_name()?.to_string_lossy().into_owned();
            let cover = path.strip_prefix(&view.volume.path).ok().and_then(art::collection).unwrap_or(Cover::Folder);
            return Some((name, tr("Sèrie", "Sèrie"), cover));
        }
        if let Some(folder) = self.folders.last() {
            let entry = folder.entries.get(index)?;
            let (cover, kind) = art::entry_cover(entry, folder.collection);
            Some((display_name(entry, self.settings.extensions), kind, cover))
        } else {
        self.scan.volumes.get(index).map(|volume| (volume_name(volume), tr("Disco USB", "Disc USB"), Cover::Drive))
        }
    }

    pub(super) fn draw(&self) {
        let (r, g, b) = theme::CLEAR_RGB;
        gfx::frame_clear(r, g, b);
        let p = Painter::root().alpha(plx_ui::nav::page_alpha());
        let x = plx_ui::consts::MARGIN_X;
        let width = 1920.0 - 2.0 * x;
        self.draw_nav_pill(p);
        draw_line(p, "CineUSB", Rect::new(x, 45.0, 260.0, 50.0), theme::size::HEADLINE, true);
        for (action, rect) in ACTIONS {
            if self.action_available(action) {
                let label = match action {
                    Action::ToggleFavorite => if self.folder_favorite_key().and_then(|key| self.history_value(&key)) == Some(FAVORITE) {
                        tr("Quitar de favoritos", "Treure dels favorits")
                    } else { tr("Añadir a favoritos", "Afegir als favorits") },
                    Action::Select if self.selection.is_some() => tr("Cancelar", "Cancel·la"),
                    Action::Select => tr("Seleccionar", "Selecciona"),
                    Action::DeleteFolders => tr("Eliminar", "Esborra"),
                };
                let glyph = match action {
                    Action::ToggleFavorite => None,
                    Action::Select => Some(Glyph::Shared(Icon::CheckCircleFill)),
                    Action::DeleteFolders => Some(Glyph::Trash),

                };
                if action == Action::ToggleFavorite {
                    self.draw_favorite_toggle(p, rect, label);
                } else if self.action != Some(action) && action != Action::DeleteFolders {
                    draw_button_content(p, rect, label, glyph, theme::TEXT_PRIMARY);
                } else {
                    self.draw_button(p, rect, label, self.action == Some(action), action == Action::DeleteFolders, glyph);
                }
            }
        }
        let breadcrumb = self.folders.last().filter(|_| self.favorite_cards().is_none()).map(|folder| {
            let relative = folder.path.strip_prefix(&folder.volume.path).unwrap_or(&folder.path);
            let mut parts = vec![volume_name(&folder.volume)];
            parts.extend(relative.components().map(|part| part.as_os_str().to_string_lossy().into_owned()));
            parts.join("   /   ")
        });
        let focused = self.item(self.selected);
        let (title, detail, greeting_icon) = if self.media_cards().is_some() {
            (if self.nav_tab == 1 { tr("Películas", "Pel·lícules") } else { tr("Series", "Sèries") }, "", None)
        } else if self.favorite_cards().is_some() {
            (tr("Carpetas favoritas", "Carpetes favorites"), "", None)
        } else if breadcrumb.is_none() {
            let greeting = match self.hour {
                Some(6..=11) => (tr("Buenos días", "Bon dia"), Some(Glyph::Sun)),
                Some(12..=19) => (tr("Buenas tardes", "Bona tarda"), Some(Glyph::Sunset)),
                Some(_) => (tr("Buenas noches", "Bona nit"), Some(Glyph::Moon)),
                None => (tr("Te damos la bienvenida", "Benvingut a CineUSB"), None),
            };
            (greeting.0, tr("Tus películas y series, desde USB", "Les teves pel·lícules i sèries, a l’USB"), greeting.1)
        } else {
            focused.as_ref().map_or(("Biblioteca USB", "Carpeta", None), |(title, kind, _)| (title.as_str(), *kind, None))
        };
        let title_size = if TtfMeasure.width_str(title, theme::size::DISPLAY, true) <= width {
            theme::size::DISPLAY
        } else { theme::size::TITLE };
        plx_ui::text_view::TextView::new(title, title_size, theme::TEXT_PRIMARY)
            .with_measure(&TtfMeasure).bold().break_long_words().leading(42.0).max_lines(2)
            .draw(p, Rect::new(x, 116.0, width, 84.0));
        if let Some(icon) = greeting_icon {
            let after_title = x + TtfMeasure.width_str(title, title_size, true) + 16.0;
            draw_glyph(p, icon, Rect::new(after_title, 112.0, 39.0, 39.0), theme::USB_FAVORITES);
        }
        let count = self.selection.as_ref().map(|marked| {
            format!("{} {}", marked.len(), tr("carpetas seleccionadas", "carpetes seleccionades"))
        });
        let section = breadcrumb.as_deref().unwrap_or(if self.favorite_cards().is_some() { "" } else { tr("Discos conectados", "Discos connectats") });
        draw_line(p, section, Rect::new(x, 191.0, width, 33.0), theme::size::BODY, false);
        draw_line(p, count.as_deref().unwrap_or(detail), Rect::new(x, 226.0, 920.0, 37.0), theme::size::CAPTION, false);

        if self.len() == 0 {
            let message = if self.media_cards().is_some() {
                tr("No hay carpetas de esta categoría. Pulsa Inicio para navegar manualmente.",
                    "No hi ha carpetes d'aquesta categoria. Prem Inici per navegar manualment.")
            } else if self.favorite_cards().is_some() {
                tr("No hay carpetas favoritas en este USB.", "No hi ha cap carpeta favorita en aquest USB.")
            } else if self.folders.last().is_some() {
                tr("Esta carpeta no contiene subcarpetas ni vídeos compatibles.",
                    "En aquesta carpeta no hi ha subcarpetes ni vídeos compatibles.")
            } else if self.scan.hidden_mounts {
                tr("CineUSB aún no puede leer el disco. Pulsa OK para volver a intentarlo.",
                    "CineUSB encara no pot llegir el disc. Prem OK per tornar-ho a provar.")
            } else {
                tr("Conecta un disco USB directamente a la televisión.", "Connecta un disc USB a la tele.")
            };
            draw_line(p, message, Rect::new(x, 380.0, width, 70.0), theme::size::BODY, false);
        } else {
            p.clip(Rect::new(0.0, TOP - 14.0, 1920.0, GRID_BOTTOM - TOP + 14.0));
            let grid = grid_geometry(self.len(), self.settings.cards);
            let cells = poster_grid::visible_in(&grid, self.len(), TOP, self.scroll.pos);
            for i in cells.filter(|index| *index != self.selected) { self.draw_tile(p, i, false); }
            self.draw_tile(p, self.selected, true);
            p.clip_clear();
        }
        let storage = self.facts.as_ref().filter(|(disk, _)| self.volume() == Some(disk))
            .and_then(|(disk, (capacity, _))| capacity.map(|facts| (disk, facts)));
        if self.dialog.is_none() {
            if let Some((disk, facts)) = storage {
                self.draw_storage(p, disk, facts);
            }
        }
        if let Some(dialog) = &self.dialog {
            self.draw_dialog(p, dialog);
        } else if !self.status.is_empty() {
            draw_line(p, &self.status, Rect::new(x, 990.0, width - if storage.is_some() { 610.0 } else { 0.0 }, 48.0), theme::size::CAPTION, false);
        } else {
            let primary = if self.selection.is_some() && self.action.is_none() {
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
            if self.action.is_none() {
                cursor = draw_hint(p, cursor, HintKey::Icon(Icon::ChevronUp), tr("Barra superior", "Barra superior"));
            } else {
                cursor = draw_hint(p, cursor, HintKey::Icon(Icon::ChevronDown), tr("Tarjetas", "Targetes"));
            }
            cursor = draw_hint(p, cursor, HintKey::Ok, primary);
            draw_hint(p, cursor, HintKey::Icon(Icon::ChevronLeft), back);
        }
        self.draw_eject_feedback(p);
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
        let description = match result {
            Some(true) => tr("Ya puedes desconectar el dispositivo con seguridad.", "Ja pots desconnectar el dispositiu amb seguretat."),
            Some(false) if self.eject_uncertain => tr("No desconectes el disco: webOS aún muestra particiones montadas.",
                "No desconnectis el disc: webOS encara mostra particions muntades."),
            Some(false) => tr("Comprueba que no haya archivos en uso. Pulsa OK para reintentar o Atrás para cerrar.",
                "Comprova que no hi hagi fitxers oberts. Prem OK per reintentar o Enrere per tancar."),
            None => tr("Espera a que termine la operación.", "Espera que acabi l'operació."),
        };
        draw_line(p, description, Rect::new(620.0, 602.0, 690.0, 85.0), theme::size::CAPTION, false);
    }

    fn draw_nav_pill(&self, p: Painter) {
        p.rect_sheened(NAV_PILL, 35.0, theme::CONTROL_IDLE_FILL, theme::CONTROL_IDLE_FILL);
        p.rring(NAV_PILL, 35.0, 1.0, theme::TEXT_SECONDARY);
        // Same spring capsules as PlxNative; labels and track stay in place.
        self.nav_strip.draw(p, NAV_TABS[0].y, NAV_TABS[0].h, widgets::TabGround::Tracked { glass: false });
        for (index, rect) in NAV_TABS.into_iter().enumerate() {
            let (title, glyph) = match index {
                0 => (tr("Inicio", "Inici"), None),
                1 => (tr("Películas", "Pel·lícules"), None),
                2 => (tr("Series", "Sèries"), None),
                3 => (tr("Favoritos", "Favorits"), Some(Glyph::Star(false))),
                4 => (tr("Ajustes", "Ajustos"), Some(Glyph::Settings)),
                _ => (tr("Expulsar disco", "Expulsa el disc"), Some(Glyph::Eject)),
            };
            let (focus, selected) = self.nav_strip.mixes((rect.x, rect.w));
            let ink = widgets::TabPill::mixed_ink(focus, selected);
            draw_button_content(p, rect, title, glyph, ink);
            if self.nav_focus == Some(index) { p.rring(rect, 24.0, 1.5, self.accent()); }
        }
    }

    fn draw_storage(&self, p: Painter, volume: &Volume, (total, used, free, readonly): (u64, u64, u64, bool)) {
        let rect = Rect::new(1234.0, 984.0, 590.0, 56.0);
        p.rect(rect, 16.0, theme::CONTROL_IDLE_FILL, theme::CONTROL_IDLE_FILL, 0.0);
        draw_glyph(p, Glyph::Drive, Rect::new(rect.x + 13.0, rect.y + 8.0, 19.0, 19.0), theme::TEXT_PRIMARY);
        let name = volume_name(volume);
        draw_line(p, &name, Rect::new(rect.x + 39.0, rect.y + 4.0, 110.0, 30.0), theme::size::DIAGNOSTIC, true);
        let fraction = if total == 0 { 0.0 } else { (used as f64 / total as f64).clamp(0.0, 1.0) };
        let usage = format!("{:.0} % · {} / {} {}", fraction * 100.0, video_size(free), video_size(total),
            tr("libres", "lliures"));
        let mode = if readonly { tr(" · Solo lectura", " · Només lectura") } else { "" };
        draw_line(p, &format!("{usage}{mode}"), Rect::new(rect.x + 154.0, rect.y + 4.0, 420.0, 30.0), theme::size::DIAGNOSTIC, false);
        let bar = Rect::new(rect.x + 15.0, rect.y + 39.0, rect.w - 30.0, 5.0);
        p.rect(bar, 2.5, theme::RAIL_TRACK, theme::RAIL_TRACK, 0.0);
        if fraction > 0.0 {
            let fill = Rect::new(bar.x, bar.y, bar.w * fraction as f32, bar.h);
            p.rect(fill, 2.5, self.accent(), self.accent(), 0.0);
        }
    }

    fn draw_unified_settings(&self, p: Painter) {
        // Full-page PlxNative-like view, not the alert/modal glass surface.
        let p = p.alpha(self.dialog_started.map_or(1.0, |at| 0.86 + motion_alpha(at) * 0.14));
        p.rect(Rect::new(0.0, 0.0, 1920.0, 1080.0), 0.0, theme::scrim_black(0.98), theme::scrim_black(0.98), 0.0);
        draw_line(p, tr("Configuración", "Configuració"),
            Rect::new(110.0, 169.0, 650.0, 80.0), theme::size::HEADLINE, true);
        let (_, description, _) = unified_setting_text(self.option, &self.settings);
        plx_ui::text_view::TextView::new(description, theme::size::BODY, theme::TEXT_SECONDARY)
            .with_measure(&TtfMeasure).break_long_words().leading(34.0).max_lines(5)
            .draw(p, Rect::new(110.0, 281.0, 590.0, 240.0));
        draw_line(p, tr("← → Cambiar · OK Seleccionar · Atrás Cerrar",
                "← → Canvia · OK Selecciona · Enrere Tanca"),
            Rect::new(110.0, 879.0, 650.0, 50.0), theme::size::CAPTION, false);
        // All eight current settings fit without scrolling at 1920×1080.
        // Leave a separate heading in the gutter between grouped rows.
        for i in 0..UNIFIED_SETTINGS_COUNT {
            let rect = unified_setting_rect(i);
            let focused = self.option == i;
            if matches!(i, 0 | 2 | 5 | 6) {
                let heading = match i {
                    0 => tr("BIBLIOTECA / USB", "BIBLIOTECA / USB"),
                    2 => tr("REPRODUCCIÓN", "REPRODUCCIÓ"),
                    5 => tr("SISTEMA", "SISTEMA"),
                    _ => tr("APARIENCIA", "APARENÇA"),
                };
                draw_line(p, heading, Rect::new(rect.x + 10.0, rect.y - 28.0, rect.w - 20.0, 25.0),
                    theme::size::MICRO, true);
            }
            if focused {
                p.rect_sheened(rect, 14.0, theme::TEXT_PRIMARY, theme::TEXT_PRIMARY);
            }
            let (name, _, value) = unified_setting_text(i, &self.settings);
            let foreground = if focused { theme::ACCENT_INK } else { theme::TEXT_PRIMARY };
            let value_ink = if focused { theme::ACCENT_INK } else { theme::TEXT_SECONDARY };
            let label = TtfMeasure.fit_line(name, 630.0, theme::size::CAPTION, focused);
            Label::new(label.as_ptr(), theme::size::CAPTION, foreground)
                .h(HAlign::Left).draw(p, Rect::new(rect.x + 24.0, rect.y + 6.0, 630.0, 46.0));
            let fit = TtfMeasure.fit_line(&value, 280.0, theme::size::CAPTION, focused);
            Label::new(fit.as_ptr(), theme::size::CAPTION, value_ink)
                .h(HAlign::Right).draw(p, Rect::new(rect.x + rect.w - 322.0, rect.y + 6.0, 292.0, 46.0));
            // The selected color appears as a swatch while focus remains white.
            if i == 6 {
                let swatch = theme::usb_accent(self.settings.accent);
                p.rect_sheened(Rect::new(rect.x + rect.w - 344.0, rect.y + 18.0, 17.0, 17.0),
                    8.0, swatch, swatch);
            }
        }
        if !self.status.is_empty() {
            draw_line(p, &self.status, Rect::new(110.0, 941.0, 1630.0, 53.0), theme::size::CAPTION, false);
        }
    }

    fn draw_dialog(&self, p: Painter, dialog: &Dialog) {
        if matches!(dialog, Dialog::Settings) {
            self.draw_unified_settings(p);
            return;
        }
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
                path.file_stem().unwrap_or_default().to_string_lossy().into_owned(), "",
                [tr("Reproducir ahora", "Reprodueix ara"), cancel]),
            Dialog::Video { path, .. } => (tr("Opciones del vídeo", "Opcions del vídeo"),
                path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                "",
                [tr("Reproducir", "Reprodueix"), tr("Eliminar archivo", "Esborra el fitxer")]),
            Dialog::Delete { path, .. } => (tr("¿Eliminar este archivo?", "Vols esborrar aquest fitxer?"),
                path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                tr("Se borrará del disco y no se podrá deshacer.", "S’esborrarà del disc i no el podràs recuperar."),
                [cancel, tr("Eliminar archivo", "Esborra el fitxer")]),
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
        p.rect(Rect::new(0.0, 0.0, 1920.0, 1080.0), 0.0, theme::scrim_black(0.48), theme::scrim_black(0.48), 0.0);
        p.shadow_outside(PANEL, theme::ALERT_PANEL_RAD, theme::MENU_SHADOW_BLUR, theme::MENU_SHADOW_DY, theme::MENU_SHADOW);
        p.rect_sheened(PANEL, theme::ALERT_PANEL_RAD, theme::MENU_GLASS_TOP, theme::MENU_GLASS_BOT);
        let destructive = matches!(dialog, Dialog::Delete { .. } | Dialog::DeleteFolders);
        let eject = matches!(dialog, Dialog::Eject(_));
        let glyph_rect = Rect::new(x, 326.0, 80.0, 80.0);
        if destructive { icons::draw(p, Icon::Alert, glyph_rect, theme::DANGER); }
        if eject { draw_glyph(p, Glyph::Eject, glyph_rect, theme::TEXT_PRIMARY); }
        let title_y = if destructive || eject { 414.0 } else { 343.0 };
        draw_line(p, title, Rect::new(x, title_y, w, 60.0), theme::size::HEADLINE, true);
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
                self.draw_button(p, rect, label, self.option == index, false, None);
            }
            if !self.status.is_empty() {
                draw_line(p, &self.status, Rect::new(x, 737.0, w, 40.0), theme::size::MICRO, false);
            }
            return;
        }
        draw_line(p, &subject, Rect::new(x, title_y + 67.0, w, 52.0), theme::size::BODY, true);
        draw_line(p, detail, Rect::new(x, title_y + 133.0, w, 48.0), theme::size::CAPTION, false);
        if let Dialog::Video { path, .. } = dialog {
            if let Some(details) = self.details.get(&self.detail_key_for(path)) {
                draw_video_details(p, *details, x, 490.0);
            }
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
            Some(if marked { (tr("Quitar de favoritos", "Treu dels favorits"), Glyph::Star(true)) }
                else { (tr("Añadir a favoritos", "Afegeix als favorits"), Glyph::Star(false)) })
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
            self.draw_button(p, rect, label, self.option == index, destructive && index == 1, glyph);
        }
    }

    fn draw_favorite_toggle(&self, p: Painter, rect: Rect, label: &str) {
        let marked = self.folder_favorite_key().and_then(|key| self.history_value(&key)) == Some(FAVORITE);
        let focused = self.action == Some(Action::ToggleFavorite);
        let animation = self.favorite_motion.as_ref().filter(|(path, _, started)|
            started.elapsed() < MOTION && self.series_root().as_ref() == Some(path));
        let Some((_, _, started)) = animation else {
            self.draw_button(p, rect, label, focused, false, Some(Glyph::Star(marked)));
            return;
        };
        let t = motion_alpha(*started);
        let pulse = (t * std::f32::consts::PI).sin();
        let rect = rect.scaled(1.0 + 0.025 * pulse);
        let fill = if focused { self.accent() } else { theme::CONTROL_IDLE_FILL };
        let ink = if focused { theme::ACCENT_INK } else { theme::TEXT_PRIMARY };
        widgets::draw_control_face(p, rect, fill, focused, widgets::ControlGround::Keyed);
        p.alpha(pulse).rring(rect, rect.h * 0.5, 2.0, theme::USB_FAVORITES);
        let (run, width) = TtfMeasure.fit_line_measured(label, rect.w - 67.0, theme::size::CAPTION, true);
        let x = rect.x + (rect.w - width - 39.0) * 0.5;
        let star = Rect::new(x, rect.y + (rect.h - 26.0) * 0.5, 26.0, 26.0).scaled(1.0 + 0.16 * pulse);
        draw_glyph(p.alpha(1.0 - t), Glyph::Star(!marked), star, ink);
        draw_glyph(p.alpha(t), Glyph::Star(marked), star, ink);
        Label::new(run.as_ptr(), theme::size::CAPTION, ink).h(HAlign::Center).bold()
            .draw(p, Rect::new(x + 39.0, rect.y, width, rect.h));
    }

    fn accent(&self) -> [f32; 4] {
        theme::usb_accent(self.settings.accent)
    }

    fn draw_button(&self, p: Painter, rect: Rect, label: &str, focused: bool, danger: bool, glyph: Option<Glyph>) {
        let scale = self.button_motion.filter(|(pressed, at)| *pressed == rect && at.elapsed() < MOTION)
            .map_or(1.0, |(_, at)| 0.975 + motion_alpha(at) * 0.025);
        draw_button(p, rect.scaled(scale), label, focused, danger, glyph, self.accent());
    }

    fn draw_tile(&self, p: Painter, index: usize, focused: bool) {
        let Some((title, kind, cover)) = self.item(index) else { return };
        let frame = self.tile_rect(index);
        let scale = self.pop.scale(index, focused && self.action.is_none(), &USB_STYLE);
        let card = frame.scaled(scale);
        let radius = USB_STYLE.tile_radius(card, scale);
        let lift = ((scale - 1.0) / (USB_STYLE.focus_scale - 1.0)).clamp(0.0, 1.0);
        let current = self.folders.last().filter(|_| self.favorite_cards().is_none() && self.media_cards().is_none()).and_then(|folder| folder.entries.get(index).map(|entry| (folder, entry)));
        let record = current.and_then(|(folder, entry)| {
            let identity = self.identity(&folder.volume)?;
            let path = folder.path.join(&entry.name);
            let key = if entry.kind == Kind::Folder { favorite_key(identity, &folder.volume, &path) }
                else { entry_key(identity, &folder.volume, &path, entry.size?, entry.modified?) }?;
            self.history_value(&key)
        }).unwrap_or_default();
        if record == WATCHED {
            if lift > 0.0 {
                p.shadow_outside(card, radius, 14.0, 5.0, theme::scrim_black(lift * 0.24));
            }
            p.rect_sheened(card, radius, theme::USB_WATCHED_TOP, theme::USB_WATCHED_BOTTOM);
            p.rring(card, radius, 2.0, theme::USB_WATCHED_EDGE);
        } else { cover.draw(p, card, radius, lift); }
        if let Some((folder, entry)) = current {
            let star = Rect::new(card.x + card.w - 40.0, card.y + 13.0, 26.0, 26.0);
            let animation = self.favorite_motion.as_ref().filter(|(path, _, at)|
                at.elapsed() < MOTION && *path == folder.path.join(&entry.name));
            if let Some((_, marked, started)) = animation {
                let alpha = motion_alpha(*started);
                draw_glyph(p.alpha(1.0 - alpha), Glyph::Star(!marked), star, self.accent());
                draw_glyph(p.alpha(alpha), Glyph::Star(*marked), star, self.accent());
            } else if record == FAVORITE { draw_glyph(p, Glyph::Star(true), star, self.accent()); }
        }
        let video = current.is_some_and(|(_, entry)| entry.kind == Kind::Video);
        if video { widgets::art_scrim(p, card, radius, 68.0, widgets::STILL_SCRIM_A); }
        let caption = widgets::poster_title(p, card, radius, &title, video, &TtfMeasure);
        let status_y = if video { card.y + card.h - 44.0 } else { caption.y - 30.0 };
        let (position, duration) = unpack_progress(record);
        if duration > 0 {
            let bar = Rect::new(card.x + 14.0, status_y - 9.0, card.w - 28.0, 5.0);
            p.rect_sheened(bar, 2.5, theme::CONTROL_IDLE_FILL, theme::CONTROL_IDLE_FILL);
            let fill = Rect::new(bar.x, bar.y, bar.w * (position as f64 / duration as f64).clamp(0.0, 1.0) as f32, bar.h);
            p.rect_sheened(fill, 2.5, self.accent(), self.accent());
        }
        if focused && self.action.is_none() {
            let focus = self.accent();
            if record == WATCHED { p.rring(card.scaled(0.96), radius, 1.5, focus); }
            else { p.rring(card, radius, theme::CONTROL_FOCUS_RING_W, focus); }
        }
        if let Some(marked) = &self.selection {
            if let Some(entry) = self.folders.last().and_then(|folder| folder.entries.get(index)).filter(|entry| entry.kind == Kind::Folder) {
                let checked = marked.contains(&entry.name);
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
        if let Some((folder, entry)) = current.filter(|(_, entry)| entry.kind == Kind::Video) {
            let details = self.details.get(&Self::detail_key(folder, entry));
            let episode = continuity::episode(&entry.name).map(|(_, season, episode)| format!("{season}×{episode:02}"));
            let duration = details.and_then(|details| details.duration_secs).map(video_duration);
            let quality = details.and_then(|details| details.resolution)
                .map(|(width, height)| resolution_label(width, height));
            let badges: Vec<String> = episode.into_iter().chain(duration).chain(quality).collect();
            let size = theme::size::DIAGNOSTIC;
            let available = card.w - 24.0;
            let mut x = card.x + 12.0;
            let mut y = caption.y + caption.h + 5.0;
            for text in badges {
                let (run, width) = TtfMeasure.fit_line_measured(&text, available - 10.0, size, true);
                let badge_width = width + 10.0;
                if x + badge_width > card.x + card.w - 12.0 && x > card.x + 12.0 {
                    x = card.x + 12.0;
                    y += 37.0;
                }
                let badge = Rect::new(x, y, badge_width, 32.0);
                p.rect_sheened(badge, 10.0, theme::scrim_black(0.75), theme::scrim_black(0.75));
                if focused && self.action.is_none() { p.rring(badge, 10.0, 1.0, self.accent()); }
                Label::new(run.as_ptr(), size, theme::TEXT_PRIMARY).h(HAlign::Center).bold().draw(p, badge);
                x += badge.w + 5.0;
            }
        }
        if record == WATCHED {
            icons::draw(p, Icon::Check, Rect::new(card.x + 15.0, status_y + 5.0, 22.0, 22.0), theme::USB_WATCHED_EDGE);
            draw_line(p, tr("Visto", "Vist"), Rect::new(card.x + 44.0, status_y, card.w - 58.0, 28.0), theme::size::MICRO, true);
        } else {
        draw_line(p, kind, Rect::new(card.x + 16.0, status_y, card.w - 32.0, 28.0), theme::size::CAPTION, false);
        }
    }
}

fn hit(rect: Rect, x: f32, y: f32) -> bool {
    x >= rect.x && x < rect.x + rect.w && y >= rect.y && y < rect.y + rect.h
}

enum HintKey { Ok, Icon(Icon) }

fn draw_hint(p: Painter, x: f32, key: HintKey, text: &str) -> f32 {
    let width = if matches!(key, HintKey::Ok) { 44.0 } else { 34.0 };
    let rect = Rect::new(x, 999.0, width, 30.0);
    p.rect_sheened(rect, 9.0, theme::CONTROL_IDLE_FILL, theme::CONTROL_IDLE_FILL);
    match key {
        HintKey::Ok => {
            let run = TtfMeasure.fit_line("OK", width, 17, true);
            Label::new(run.as_ptr(), 17, theme::TEXT_READING).h(HAlign::Center).bold().draw(p, rect);
        }
        HintKey::Icon(icon) => icons::draw(p, icon, Rect::new(x + 6.0, 1003.0, 22.0, 22.0), theme::TEXT_READING),
    }
    let (run, text_width) = TtfMeasure.fit_line_measured(text, 300.0, theme::size::CAPTION, false);
    Label::new(run.as_ptr(), theme::size::CAPTION, theme::TEXT_SECONDARY).h(HAlign::Left)
        .draw(p, Rect::new(x + width + 10.0, 990.0, text_width, 48.0));
    x + width + 10.0 + text_width + 28.0
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

fn resolution_label(width: u32, height: u32) -> String {
    // Use the encoded width for cropped cinema, not the smaller visible height.
    // Do not promote QHD (2560 pixels wide) to 4K.
    let wide = width.max(height);
    let short = width.min(height);
    match (wide, short) {
        (3840..=4096, 1600..=2160) => "4K UHD".into(),
        (1920, 800..=1080) => "1080p".into(),
        (1280, 500..=720) => "720p".into(),
        (1..=1024, 1..=576) => "SD".into(),
        _ => format!("{width}×{height}"),
    }
}

fn draw_video_details(p: Painter, details: plx_play_feed::LocalVideoDetails, start: f32, mut y: f32) {
    use plx_play_feed::VideoRange;
    let dolby = matches!(details.range, Some(VideoRange::DolbyVision | VideoRange::DolbyVisionHdr10));
    let formats = [
        dolby.then_some("Dolby Vision"),
        details.hdr10_plus.then_some("HDR10+"),
        details.hdr10_base.then_some("HDR10"),
        (details.range == Some(VideoRange::Sdr)).then_some("SDR"),
    ];
    let size = details.bytes.map(video_size);
    let duration = details.duration_secs.map(video_duration);
    let resolution = details.resolution.map(|(width, height)| resolution_label(width, height));
    let labels = resolution.as_deref().into_iter().map(|text| (text, false)).chain(
        formats.into_iter().flatten().map(|text| (text, true))).chain(
        [size.as_deref(), duration.as_deref()].into_iter().flatten().map(|text| (text, false)));
    let mut x = start;
    for (text, format) in labels {
        let (run, width) = TtfMeasure.fit_line_measured(text, 360.0, theme::size::MICRO, true);
        let icon = if format { 24.0 } else { 0.0 };
        let chip_width = width + icon + 25.0;
        if x + chip_width > PANEL.x + PANEL.w - 45.0 {
            x = start;
            y += 52.0;
        }
        let rect = Rect::new(x, y, chip_width, 43.0);
        p.rect_sheened(rect, 10.0, theme::CONTROL_IDLE_FILL, theme::CONTROL_IDLE_FILL);
        if format {
            p.rring(rect, 10.0, 1.0, theme::TEXT_SECONDARY);
            let svg = icons::Svg::new(include_str!("../../../assets/icons/cineusb-dynamic-range.svg"));
            icons::draw_svg(p, svg, Rect::new(x + 10.0, y + 12.0, 19.0, 19.0), theme::TEXT_READING);
        }
        Label::new(run.as_ptr(), theme::size::MICRO, theme::TEXT_READING).h(HAlign::Center).bold()
            .draw(p, Rect::new(x + icon, y, rect.w - icon, rect.h));
        x += rect.w + 9.0;
    }
}

#[derive(Clone, Copy)]
enum Glyph { Play, Trash, Eject, Drive, Settings, Sun, Sunset, Moon, Star(bool), Shared(Icon) }

fn draw_glyph(p: Painter, glyph: Glyph, rect: Rect, ink: [f32; 4]) {
    let asset = match glyph {
        Glyph::Play => return icons::draw(p, Icon::Play, rect, ink),
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

fn draw_button(p: Painter, rect: Rect, label: &str, focused: bool, danger: bool, glyph: Option<Glyph>, accent: [f32; 4]) {
    let (fill, ink) = if danger { (theme::CONTROL_DELETE_FILL, theme::CONTROL_IDLE_INK) }
        else if focused { (accent, theme::ACCENT_INK) }
        else { (theme::CONTROL_IDLE_FILL, theme::TEXT_PRIMARY) };
    widgets::draw_control_face(p, rect, fill, focused, widgets::ControlGround::Keyed);
    if focused && danger { p.rring(rect, rect.h * 0.5, 3.0, theme::CONTROL_FOCUS_RING); }
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
    let record = crate::port::usb_preferences().map_err(|error| format!("{error:?}"))?;
    #[cfg(feature = "hostsim")]
    let record = match std::fs::read_to_string(plx_base::paths::in_runtime_dir("cineusb-settings")) {
        Ok(record) => record,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if record.is_empty() { return Ok(None); }
    let mut fields = record.split(' ');
    let language = match fields.next() {
        Some("es") => Preference::Es,
        Some("ca") => Preference::Ca,
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
    if fields.next().is_some() { return Err("invalid settings record".into()); }
    Ok(Some(Settings { language, forced_subtitles, spanish_audio, auto_next, accent, cards, sort, extensions }))
}

fn save_settings(settings: Settings) -> Result<(), String> {
    let record = format!("{} {} {} {} {} {} {} {}", if settings.language == Preference::Ca { "ca" } else { "es" },
        u8::from(settings.forced_subtitles), u8::from(settings.spanish_audio), u8::from(settings.auto_next),
        settings.accent, settings.cards, settings.sort, u8::from(settings.extensions));
    #[cfg(not(feature = "hostsim"))]
    return crate::port::save_usb_preferences(&record).map_err(|error| format!("{error:?}"));
    #[cfg(feature = "hostsim")]
    plx_plex::plex::session::write_atomic(&plx_base::paths::in_runtime_dir("cineusb-settings"), record.as_bytes())
        .map_err(|error| format!("{error:?}"))
}

fn mutate_usb(action: Mutation, volume: &Volume, path: &std::path::Path) -> Result<(), String> {
    #[cfg(not(feature = "hostsim"))]
    {
        let id = system_mount(volume).ok_or("missing live USB mount")?;
        match action {
            Mutation::Delete => crate::port::delete_usb_video(path, id).map_err(|error| format!("{error:?}")),
            Mutation::Eject => crate::port::eject_usb_disk(path, id),
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
    return crate::port::delete_usb_folder(path, mount);
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
    match crate::port::list_usb_volumes() {
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

fn volume_facts(volume: &Volume) -> VolumeFacts {
    #[cfg(not(feature = "hostsim"))]
    if let Some(mount) = system_mount(volume) {
        return crate::port::usb_volume_details(&volume.path, mount).unwrap_or_default();
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
    records.iter().find_map(|(key, value)| {
        (key.len() == current.len() && key.starts_with(prefix) && *value != 0 && *value != WATCHED
            && *value != FAVORITE).then(|| (key.clone(), *value))
    })
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

fn history_matches(key: &str, base: &str) -> bool {
    key.strip_prefix(base).is_some_and(|suffix| suffix.starts_with("00") || suffix.starts_with("2f"))
}

fn clear_history(base: &str) -> Result<(), String> {
    #[cfg(not(feature = "hostsim"))]
    return crate::port::clear_usb_history(base).map_err(|error| format!("{error:?}"));
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
    return crate::port::usb_history(prefix).ok();
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
    return crate::port::usb_resume(key, position).map_err(|error| format!("{error:?}"));
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
        let entries = crate::port::list_usb_folder(path, id)
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
        let file = crate::port::open_usb_file(path, _mount_id)
            .map_err(|error| format!("No se pudo abrir el vídeo desde Homebrew USB ({error:?})."))?;
        return inspect(file);
    }
    let direct = disk::open_video(path).map_err(|error| format!("No se pudo abrir el vídeo · {error}"))
        .and_then(inspect);
    #[cfg(not(feature = "hostsim"))]
    if direct.is_err() {
        match crate::port::open_usb_file(path, None) {
            Ok(file) => return inspect(file),
            Err(error) => plx_base::eventlog::log(&format!("cineusb: Homebrew USB descriptor failed: {error:?}")),
        }
    }
    direct
}

fn open_for_details(path: &std::path::Path, _mount_id: Option<u64>) -> Result<File, String> {
    #[cfg(not(feature = "hostsim"))]
    if _mount_id.is_some() {
        return crate::port::open_usb_file(path, _mount_id).map_err(|error| format!("{error:?}"));
    }
    let direct = disk::open_video(path).map_err(|error| error.to_string());
    #[cfg(not(feature = "hostsim"))]
    if direct.is_err() {
        return crate::port::open_usb_file(path, None).map_err(|error| format!("{error:?}"));
    }
    direct
}

fn display_name(entry: &Entry, extensions: bool) -> String {
    let name = std::path::Path::new(&entry.name);
    match entry.kind {
        Kind::Folder => entry.name.to_string_lossy().into_owned(),
        Kind::Video if extensions => entry.name.to_string_lossy().into_owned(),
        Kind::Video => name.file_stem().unwrap_or(&entry.name).to_string_lossy().into_owned(),
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

pub(super) fn click(app: &mut App, x: f32, y: f32) {
    app.usb.click(x, y);
    plx_machine::idle::invalidate();
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
    let Some(PreparedVideo { file, info, title, volume, path, key, resume, saved }) = app.usb.update() else { return };
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
        && plx_media::route::set_local_source(ps, file, &title, audio, forced, &item, playback::start_caps());
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
    fn six_navigation_destinations_stay_in_one_stationary_track() {
        assert_eq!(NAV_TABS.len(), 6);
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
        browser.open();
        assert!(matches!(browser.dialog, Some(Dialog::Video { ref volume, ref path })
            if volume == &browser.scan.volumes[0] && path == &movie));
        assert_eq!(VIDEO_OPTIONS.len(), 4);
    }

    #[test]
    fn movie_navigation_returns_all_matching_folders_for_one_usb() {
        let volume = Volume { path: "/tmp/usb/sdb/sdb1".into(), label: None, mount_id: Some(121), system: true };
        let first = volume.path.join("PELIS");
        let second = volume.path.join("Películas 4K");
        let mut browser = Browser::new();
        browser.scan.volumes.push(volume.clone());
        browser.discovery_cache.insert((volume.path.clone(), 121, 1), vec![first.clone(), second.clone()]);
        browser.open_nav_tab(1);
        assert_eq!(browser.nav_tab, 1);
        assert_eq!(browser.media_cards().unwrap().paths, vec![first, second]);
        browser.leave_media();
        assert_eq!(browser.nav_tab, 0);
        assert!(browser.media_cards().is_none());
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
        browser.nav_focus = Some(4);
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
        assert_eq!(resolution_label(3840, 2160), "4K UHD");
        assert_eq!(resolution_label(3840, 1608), "4K UHD");
        assert_eq!(resolution_label(4096, 1716), "4K UHD");
        assert_eq!(resolution_label(1920, 800), "1080p");
        assert_eq!(resolution_label(1280, 536), "720p");
        assert_eq!(resolution_label(2560, 1440), "2560×1440");
        assert_eq!(resolution_label(2560, 1080), "2560×1080");
        assert_eq!(resolution_label(1608, 3840), "4K UHD");
        assert_eq!(video_duration(95 * 60), "1 h 35 min");
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
        assert_eq!(browser.next_scan, scan_at);
    }

    #[test]
    fn compact_grid_balances_short_rows_and_centres_the_last_row() {
        let mut browser = Browser::new();
        browser.scan.volumes = (0..9).map(|index| Volume {
            path: format!("/tmp/usb/sda/sda{index}").into(), label: None, mount_id: None, system: false,
        }).collect();
        assert_eq!(grid_geometry(browser.len(), 0).cols, 5);
        for (first, last) in [(0, 4), (5, 8)] {
            let left = browser.tile_rect(first);
            let right = browser.tile_rect(last);
            assert!((left.x + right.x + right.w - 1920.0).abs() < 0.1);
        }
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
