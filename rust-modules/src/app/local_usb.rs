//! USB browsing and its off-frame filesystem jobs.
mod art;
mod disk;

use art::Cover;
use super::{App, Key, playback};
use disk::{Entry, Kind, Scan, Volume};
use plx_gfx::{gfx, text::TtfMeasure};
use plx_machine::machine::Measure;
use plx_ui::{
    icons::{self, Icon},
    label::{HAlign, Label},
    poster_grid::{self, Card, Geom, GridPop, STYLE},
    screen::Dir,
    theme, widgets, Painter, Rect, Spring,
};
use std::fs::File;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use plx_platform::i18n::{self, Language, Preference};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

const TOP: f32 = 338.0;
const COLS: usize = poster_grid::COLS;
const GRID: Geom = Geom::of(COLS, plx_ui::consts::MARGIN_X, &STYLE);

fn tr(es: &'static str, ca: &'static str) -> &'static str {
    if i18n::current().language() == Language::Ca { ca } else { es }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action { Settings, Select, DeleteFolders, Eject }

enum Mutation { Delete, Eject }

const ACTIONS: [(Action, Rect); 4] = [
    (Action::Settings, Rect::new(758.0, 46.0, 150.0, 56.0)),
    (Action::Select, Rect::new(924.0, 46.0, 246.0, 56.0)),
    (Action::DeleteFolders, Rect::new(1186.0, 46.0, 176.0, 56.0)),
    (Action::Eject, Rect::new(1378.0, 46.0, 446.0, 56.0)),
];
const PANEL: Rect = Rect::new(510.0, 292.0, 900.0, 496.0);
const OPTIONS: [Rect; 2] = [
    Rect::new(555.0, 676.0, 390.0, 66.0),
    Rect::new(975.0, 676.0, 390.0, 66.0),
];
const SETTINGS_OPTIONS: [Rect; 3] = [
    Rect::new(550.0, 523.0, 820.0, 62.0),
    Rect::new(550.0, 597.0, 820.0, 62.0),
    Rect::new(550.0, 671.0, 820.0, 62.0),
];

enum Dialog {
    Settings,
    Language,
    ForcedSubtitles,
    Audio,
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
}

enum DiskResult {
    Folder(Volume, PathBuf, io::Result<Vec<Entry>>),
    Video(Result<(File, plx_play_feed::LocalMedia, String), String>),
    Delete(PathBuf, Result<(), String>),
    DeleteFolders { removed: Vec<OsString>, total: usize, error: Option<String>, entries: io::Result<Vec<Entry>> },
    Eject(Volume, Result<(), String>),
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
            settings: Settings { language: Preference::Es, forced_subtitles: false, spanish_audio: false },
            settings_job: None, settings_started: false,
            pending: None, scanning: None, next_scan: now, last_tick: now,
        }
    }

    fn len(&self) -> usize {
        self.folders.last().map_or(self.scan.volumes.len(), |folder| folder.entries.len())
    }

    fn request(&mut self, run: impl FnOnce() -> DiskResult + Send + 'static) {
        let (send, receive) = mpsc::sync_channel(1);
        if plx_base::task::spawn_off_frame("cineusb-disk", move |_| { let _ = send.send(run()); }) {
            self.pending = Some(receive);
        } else {
            self.status = tr("No se pudo iniciar la operación.", "No s’ha pogut iniciar l’operació.").into();
        }
    }

    fn update(&mut self) -> Option<(File, plx_play_feed::LocalMedia, String)> {
        let now = Instant::now();
        let dt = (now - self.last_tick).as_secs_f32().min(0.05);
        self.last_tick = now;
        let mut changed = false;
        let mut video = None;
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
                    DiskResult::Folder(volume, path, Ok(entries)) => {
                        if let Some(folder) = self.folders.last_mut() { folder.selected = self.selected; }
                        let collection = path.strip_prefix(&volume.path).ok().and_then(art::collection);
                        self.folders.push(Folder { volume, path, entries, collection, selected: 0 });
                        self.select(0, true);
                        self.status.clear();
                    }
                    DiskResult::Folder(_, _, Err(error)) => {
                        plx_base::eventlog::log(&format!("cineusb: USB folder listing failed: {error}"));
                        self.status = tr("No se pudo abrir la carpeta.", "No s’ha pogut obrir la carpeta.").into();
                    }
                    DiskResult::Video(Ok(media)) => {
                        video = Some(media);
                        self.status.clear();
                    }
                    DiskResult::Video(Err(error)) => {
                        plx_base::eventlog::log(&format!("cineusb: video probe failed: {error}"));
                        self.status = tr("No se pudo abrir el vídeo.", "No s’ha pogut obrir el vídeo.").into();
                    }
                    DiskResult::Delete(path, result) => {
                        if result.is_ok() {
                            if let Some(folder) = self.folders.last_mut() {
                                folder.entries.retain(|entry| path.file_name() != Some(entry.name.as_os_str()));
                            }
                            self.select(self.selected, true);
                        }
                        self.finish_mutation(result, tr("Archivo eliminado.", "Fitxer esborrat."),
                            tr("No se pudo eliminar el archivo. Comprueba si el disco admite escritura.",
                                "No s’ha pogut esborrar el fitxer. Comprova si es pot escriure al disc."));
                    }
                    DiskResult::DeleteFolders { removed, total, error, entries } => {
                        if let Some(folder) = self.folders.last_mut() {
                            match entries {
                                Ok(entries) => folder.entries = entries,
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
                    DiskResult::Eject(volume, result) => {
                        if result.is_ok() {
                            self.scan.volumes.retain(|item| *item != volume);
                            self.folders.clear();
                            self.action = None;
                            self.select(0, true);
                        }
                        self.finish_mutation(result, tr("Ya puedes retirar el disco USB.", "Ja pots treure el disc USB."),
                            tr("No se pudo expulsar el USB. Cierra las apps que lo usan y vuelve a intentarlo.",
                                "No s’ha pogut expulsar l’USB. Tanca les aplicacions que el fan servir i torna-ho a provar."));
                    }
                }
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.pending = None;
                self.mutating = false;
                self.progress = None;
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
                        self.scan = scan;
                        if self.folders.last().is_some_and(|folder| !self.scan.volumes.contains(&folder.volume)) {
                            self.folders.clear();
                            self.dialog = None;
                            self.selection = None;
                            if !self.status.is_empty() { self.status.push_str(" · "); }
                            self.status.push_str(tr("Se ha retirado el disco USB.", "S’ha retirat el disc USB."));
                            video = None;
                        }
                        if self.folders.is_empty() {
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
        if self.action.is_some_and(|action| !self.action_available(action)) {
            self.action = None;
            changed = true;
        }

        let target = self.scroll_target();
        let before = self.scroll.pos;
        self.scroll.step(target, 240.0, dt);
        self.scroll.land(target, dt);
        let focused = (self.len() > 0).then_some(Card { cell: self.selected, key: self.selected as u64 });
        let scale = self.pop.scale(self.selected, true, &STYLE);
        self.pop.tick(focused, &STYLE, dt);
        if changed || before != self.scroll.pos || scale != self.pop.scale(self.selected, true, &STYLE) {
            plx_machine::idle::invalidate();
        }
        video
    }

    fn scroll_target(&self) -> f32 {
        let row = self.selected / COLS;
        (row as f32 * GRID.pitch() + GRID.h - 570.0).max(0.0)
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
                self.settings = settings;
                i18n::apply_usb_language(settings.language);
                if saving {
                    self.option = match self.dialog {
                        Some(Dialog::ForcedSubtitles) => 1,
                        Some(Dialog::Audio) => 2,
                        _ => 0,
                    };
                    self.dialog = Some(Dialog::Settings);
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
        self.folders.last().map(|folder| &folder.volume)
            .or_else(|| self.scan.volumes.get(self.selected))
    }

    fn action_available(&self, action: Action) -> bool {
        match action {
            Action::Settings => self.selection.is_none(),
            Action::Select => self.selection.is_some() || self.folders.last().is_some_and(|folder| {
                system_mount(&folder.volume).is_some() && folder.entries.iter().any(|entry| entry.kind == Kind::Folder)
            }),
            Action::DeleteFolders => self.selection.as_ref().is_some_and(|marked| !marked.is_empty()),
            Action::Eject => self.selection.is_none() && self.volume().and_then(system_mount).is_some(),
        }
    }

    fn open_action(&mut self, action: Action) {
        if self.pending.is_some() || !self.action_available(action) { return; }
        self.option = 0;
        self.status.clear();
        self.dialog = match action {
            Action::Settings => Some(Dialog::Settings),
            Action::Eject => self.volume().cloned().map(Dialog::Eject),
            Action::Select => {
                self.selection = self.selection.is_none().then(BTreeSet::new);
                self.action = None;
                None
            }
            Action::DeleteFolders => Some(Dialog::DeleteFolders),
        };
    }

    fn choose_option(&mut self) {
        if self.settings_job.is_some() { return; }
        match self.dialog.take() {
            Some(Dialog::Settings) => {
                self.status.clear();
                let (dialog, selected) = match self.option {
                    0 => (Dialog::Language, self.settings.language == Preference::Ca),
                    1 => (Dialog::ForcedSubtitles, self.settings.forced_subtitles),
                    _ => (Dialog::Audio, self.settings.spanish_audio),
                };
                self.dialog = Some(dialog);
                self.option = usize::from(selected);
            }
            Some(Dialog::Language) => {
                let language = if self.option == 0 { Preference::Es } else { Preference::Ca };
                self.dialog = Some(Dialog::Language);
                self.change_settings(Settings { language, ..self.settings });
            }
            Some(Dialog::ForcedSubtitles) => {
                self.dialog = Some(Dialog::ForcedSubtitles);
                self.change_settings(Settings { forced_subtitles: self.option == 1, ..self.settings });
            }
            Some(Dialog::Audio) => {
                self.dialog = Some(Dialog::Audio);
                self.change_settings(Settings { spanish_audio: self.option == 1, ..self.settings });
            }
            Some(Dialog::DeleteFolders) if self.option == 1 => self.delete_marked_folders(),
            Some(Dialog::Video { volume, path }) => {
                if !self.scan.volumes.contains(&volume) { return; }
                if self.option == 0 {
                    self.play_video(&volume, path);
                } else if system_mount(&volume).is_some() {
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
                self.request(move || {
                    let result = mutate_usb(Mutation::Eject, &volume, &volume.path);
                    DiskResult::Eject(volume, result)
                });
                self.mutating = self.pending.is_some();
            }
            _ => {}
        }
    }

    fn delete_marked_folders(&mut self) {
        let Some(folder) = self.folders.last() else { return };
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
        let title = path.file_stem().or_else(|| path.file_name()).unwrap_or_default().to_string_lossy().into_owned();
        let mount_id = system_mount(volume);
        let spanish = self.settings.spanish_audio;
        self.status = tr("Abriendo vídeo…", "Obrint el vídeo…").into();
        self.request(move || {
            let result = inspect_video(&path, mount_id, spanish).map(|(file, info)| (file, info, title));
            DiskResult::Video(result)
        });
    }

    fn open_folder(&mut self, volume: Volume, path: PathBuf) {
        self.status = tr("Abriendo carpeta…", "Obrint la carpeta…").into();
        self.request(move || {
            let entries = list_folder(&path, system_mount(&volume));
            DiskResult::Folder(volume, path, entries)
        });
    }

    fn back(&mut self) {
        if self.mutating || matches!(self.settings_job, Some(SettingsJob::Save(_))) { return; }
        if let Some(dialog) = self.dialog.take() {
            self.dialog = match dialog {
                Dialog::Language | Dialog::ForcedSubtitles | Dialog::Audio => Some(Dialog::Settings),
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
            self.status.clear();
            return;
        }
        let Some(folder) = self.folders.pop() else {
            plx_platform::tv::home::go_home();
            return;
        };
        let index = self.folders.last().map(|parent| parent.selected)
            .or_else(|| self.scan.volumes.iter().position(|volume| *volume == folder.volume));
        self.select(index.unwrap_or_default(), true);
        self.status.clear();
    }

    fn click(&mut self, x: f32, y: f32) {
        if self.mutating || matches!(self.settings_job, Some(SettingsJob::Save(_))) { return; }
        if self.dialog.is_some() {
            let options = if matches!(self.dialog, Some(Dialog::Settings)) { &SETTINGS_OPTIONS[..] } else { &OPTIONS[..] };
            if let Some(index) = options.iter().position(|rect| hit(*rect, x, y)) {
                if index == 1 && !self.second_option_available() { return; }
                self.option = index;
                self.choose_option();
            }
            return;
        }
        if let Some((action, _)) = ACTIONS.iter().find(|(action, rect)| self.action_available(*action) && hit(*rect, x, y)) {
            self.action = Some(*action);
            self.open_action(*action);
            return;
        }
        // Magic Remote clicks are pointer events on webOS; the standalone USB
        // browser has no shared hit-map. Pick the clicked card when possible,
        // or activate the focused card (the physical OK button may arrive with
        // the pointer parked outside the cards).
        if let Some(index) = (0..self.len()).find(|&index| {
            let rect = poster_grid::cell_in(&GRID, index, TOP, self.scroll.pos, &[]);
            x >= rect.x && x < rect.x + rect.w
                && y >= rect.y && y < rect.y + rect.h
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
        if self.mutating || matches!(self.settings_job, Some(SettingsJob::Save(_))) { return; }
        if self.dialog.is_some() {
            let settings = matches!(self.dialog, Some(Dialog::Settings));
            match key {
                Key::Up if settings => self.option = self.option.saturating_sub(1),
                Key::Down if settings => self.option = (self.option + 1).min(2),
                Key::Left { alt: false } if !settings => self.option = 0,
                Key::Right { alt: false } if !settings && self.second_option_available() => self.option = 1,
                Key::Ok if !repeat => self.choose_option(),
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
                Key::Back if !repeat => self.back(),
                Key::Ok if !repeat => self.open_action(action),
                _ => {}
            }
            return;
        }
        let dir = match key {
            Key::Up if self.selected < COLS => {
                self.action = if self.selection.is_some() { Some(Action::Select) } else { Some(Action::Settings) };
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
        if let Some(index) = poster_grid::neighbour(self.selected, self.len(), COLS, dir) {
            self.select(index, false);
        }
    }

    fn second_option_available(&self) -> bool {
        match self.dialog.as_ref() {
            Some(Dialog::Video { volume, .. }) => system_mount(volume).is_some(),
            _ => true,
        }
    }

    fn item(&self, index: usize) -> Option<(String, &'static str, Cover)> {
        if let Some(folder) = self.folders.last() {
            let entry = folder.entries.get(index)?;
            let (cover, kind) = art::entry_cover(entry, folder.collection);
            Some((display_name(entry), kind, cover))
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
        draw_line(p, "CineUSB  ·  Biblioteca local", Rect::new(x, 56.0, 720.0, 34.0), theme::size::CAPTION, true);
        for (action, rect) in ACTIONS {
            if self.action_available(action) {
                let label = match action {
                    Action::Settings => tr("Ajustes", "Ajustos"),
                    Action::Select if self.selection.is_some() => tr("Cancelar selección", "Cancel·la la selecció"),
                    Action::Select => tr("Seleccionar", "Selecciona"),
                    Action::DeleteFolders => tr("Eliminar", "Esborra"),
                    Action::Eject => tr("Desconectar USB de forma segura", "Expulsa l’USB amb seguretat"),
                };
                let glyph = match action {
                    Action::Eject => Some(Glyph::Eject),
                    Action::DeleteFolders => Some(Glyph::Trash),
                    _ => None,
                };
                draw_button(p, rect, label, self.action == Some(action), action == Action::DeleteFolders, glyph);
            }
        }
        let breadcrumb = self.folders.last().map(|folder| {
            let relative = folder.path.strip_prefix(&folder.volume.path).unwrap_or(&folder.path);
            let mut parts = vec![volume_name(&folder.volume)];
            parts.extend(relative.components().map(|part| part.as_os_str().to_string_lossy().into_owned()));
            parts.join("   /   ")
        });
        let focused = self.item(self.selected);
        let (title, detail) = if breadcrumb.is_none() {
            let greeting = match self.hour {
                Some(6..=11) => tr("Buenos días", "Bon dia"),
                Some(12..=19) => tr("Buenas tardes", "Bona tarda"),
                Some(_) => tr("Buenas noches", "Bona nit"),
                None => tr("Te damos la bienvenida", "Benvingut a CineUSB"),
            };
            (greeting, tr("Tus películas y series, desde USB", "Les teves pel·lícules i sèries, a l’USB"))
        } else {
            focused.as_ref().map_or(("Biblioteca USB", "Carpeta"), |(title, kind, _)| (title.as_str(), *kind))
        };
        draw_line(p, title, Rect::new(x, 108.0, width, 96.0), theme::size::HERO, true);
        let count = self.selection.as_ref().map(|marked| {
            format!("{} {}", marked.len(), tr("carpetas seleccionadas", "carpetes seleccionades"))
        });
        draw_line(p, count.as_deref().unwrap_or(detail), Rect::new(x, 206.0, width, 42.0), theme::size::BODY, false);
        let section = breadcrumb.as_deref().unwrap_or(tr("Discos conectados", "Discos connectats"));
        draw_line(p, section, Rect::new(x, 272.0, width, 48.0), theme::size::HEADLINE, true);

        if self.len() == 0 {
            let message = if self.folders.last().is_some() {
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
            p.clip(Rect::new(0.0, TOP - 18.0, 1920.0, 626.0));
            let cells = poster_grid::visible_in(&GRID, self.len(), TOP, self.scroll.pos);
            for i in cells.filter(|index| *index != self.selected) { self.draw_tile(p, i, false); }
            self.draw_tile(p, self.selected, true);
            p.clip_clear();
        }
        let hint = if !self.status.is_empty() { &self.status }
        else if self.selection.is_some() {
            tr("OK Marcar carpeta    ·    ↑ Acciones    ·    Atrás Cancelar selección",
                "OK Marca la carpeta    ·    ↑ Accions    ·    Enrere Cancel·la la selecció")
        } else {
            tr("↑ Acciones    ·    OK Abrir    ·    Atrás Volver",
                "↑ Accions    ·    OK Obre    ·    Enrere Torna")
        };
        draw_line(p, hint, Rect::new(x, 990.0, width, 48.0), theme::size::CAPTION, false);
        if let Some(dialog) = &self.dialog { self.draw_dialog(p, dialog); }
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
            Dialog::Language => (tr("Idioma de CineUSB", "Idioma del CineUSB"), String::new(),
                save_status(tr("Elige el idioma de la app.", "Tria en quin idioma vols fer servir l’app.")),
                ["Castellano", "Català"]),
            Dialog::ForcedSubtitles => (tr("Subtítulos forzados", "Subtítols forçats"), String::new(),
                save_status(tr("Se activan al reproducir si hay una pista compatible.",
                    "S’activen quan poses un vídeo si hi ha una pista compatible.")),
                [tr("Desactivados", "Desactivats"), tr("Activados", "Activats")]),
            Dialog::Audio => (tr("Idioma de audio predeterminado", "Idioma d’àudio per defecte"), String::new(),
                save_status(tr("Se aplica al abrir cada vídeo.", "S’aplica quan obres un vídeo.")),
                [tr("Automático", "Automàtic"), tr("Español", "Castellà")]),
            Dialog::Video { path, .. } => (tr("Opciones del vídeo", "Opcions del vídeo"),
                path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                tr("Elige qué quieres hacer con este archivo.", "Què vols fer amb aquest fitxer?"),
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
        if matches!(dialog, Dialog::Settings) {
            let settings_labels = [tr("Idioma", "Idioma"), tr("Subtítulos forzados", "Subtítols forçats"),
                tr("Idioma de audio predeterminado", "Idioma d’àudio per defecte")];
            for (index, (rect, label)) in SETTINGS_OPTIONS.into_iter().zip(settings_labels).enumerate() {
                draw_button(p, rect, label, self.option == index, false, None);
            }
            return;
        }
        draw_line(p, &subject, Rect::new(x, title_y + 67.0, w, 52.0), theme::size::BODY, true);
        draw_line(p, detail, Rect::new(x, title_y + 133.0, w, 48.0), theme::size::CAPTION, false);
        for (index, (rect, label)) in OPTIONS.into_iter().zip(options).enumerate() {
            if index == 1 && !self.second_option_available() { continue; }
            let glyph = match (dialog, index) {
                (Dialog::Video { .. }, 0) => Some(Glyph::Play),
                (Dialog::Video { .. } | Dialog::Delete { .. } | Dialog::DeleteFolders, 1) => Some(Glyph::Trash),
                (Dialog::Eject(_), 1) => Some(Glyph::Eject),
                _ => None,
            };
            draw_button(p, rect, label, self.option == index, destructive && index == 1, glyph);
        }
    }

    fn draw_tile(&self, p: Painter, index: usize, focused: bool) {
        let Some((title, kind, cover)) = self.item(index) else { return };
        let frame = poster_grid::cell_in(&GRID, index, TOP, self.scroll.pos, &[]);
        let scale = self.pop.scale(index, focused && self.action.is_none(), &STYLE);
        let card = frame.scaled(scale);
        let radius = STYLE.tile_radius(card, scale);
        let lift = ((scale - 1.0) / (STYLE.focus_scale - 1.0)).clamp(0.0, 1.0);
        cover.draw(p, card, radius, lift);
        if let Some(marked) = &self.selection {
            if let Some(entry) = self.folders.last().and_then(|folder| folder.entries.get(index)).filter(|entry| entry.kind == Kind::Folder) {
                let checked = marked.contains(&entry.name);
                let mark = Rect::new(card.x + card.w - 57.0, card.y + 17.0, 40.0, 40.0);
                let fill = if checked { theme::ACCENT } else { theme::scrim_black(0.6) };
                p.rect(mark, 20.0, fill, fill, 0.0);
                p.rring(mark, 20.0, 2.0, theme::TEXT_PRIMARY);
                if checked {
                    p.rring(card, radius, 3.0, theme::ACCENT);
                    icons::draw(p, Icon::Check, Rect::new(mark.x + 6.0, mark.y + 6.0, 28.0, 28.0), theme::ACCENT_INK);
                }
            }
        }
        draw_line(p, kind, Rect::new(card.x + 16.0, card.y + card.h - 81.0, card.w - 32.0, 28.0), theme::size::CAPTION, false);
        widgets::poster_label(p, card, radius, &title, &TtfMeasure);
    }
}

fn hit(rect: Rect, x: f32, y: f32) -> bool {
    x >= rect.x && x < rect.x + rect.w && y >= rect.y && y < rect.y + rect.h
}

#[derive(Clone, Copy)]
enum Glyph { Play, Trash, Eject }

fn draw_glyph(p: Painter, glyph: Glyph, rect: Rect, ink: [f32; 4]) {
    let asset = match glyph {
        Glyph::Play => return icons::draw(p, Icon::Play, rect, ink),
        Glyph::Trash => include_str!("../../../assets/icons/cineusb-trash.svg"),
        Glyph::Eject => include_str!("../../../assets/icons/cineusb-eject.svg"),
    };
    icons::draw_svg(p, icons::Svg::new(asset), rect, ink);
}

fn draw_button(p: Painter, rect: Rect, label: &str, focused: bool, danger: bool, glyph: Option<Glyph>) {
    let (fill, ink) = if danger { (theme::CONTROL_DELETE_FILL, theme::CONTROL_IDLE_INK) }
        else if focused { (theme::ACCENT, theme::ACCENT_INK) }
        else { (theme::CONTROL_IDLE_FILL, theme::TEXT_PRIMARY) };
    widgets::draw_control_face(p, rect, fill, focused, widgets::ControlGround::Keyed);
    if focused && danger { p.rring(rect, rect.h * 0.5, 3.0, theme::CONTROL_FOCUS_RING); }
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
    if fields.next().is_some() { return Err("invalid settings record".into()); }
    Ok(Some(Settings { language, forced_subtitles, spanish_audio }))
}

fn save_settings(settings: Settings) -> Result<(), String> {
    let catalan = settings.language == Preference::Ca;
    #[cfg(not(feature = "hostsim"))]
    return crate::port::save_usb_preferences(catalan, settings.forced_subtitles, settings.spanish_audio)
        .map_err(|error| format!("{error:?}"));
    #[cfg(feature = "hostsim")]
    {
        let record = format!("{} {} {}", if catalan { "ca" } else { "es" },
            u8::from(settings.forced_subtitles), u8::from(settings.spanish_audio));
        plx_plex::plex::session::write_atomic(&plx_base::paths::in_runtime_dir("cineusb-settings"), record.as_bytes())
            .map_err(|error| format!("{error:?}"))
    }
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

fn list_folder(path: &std::path::Path, _mount_id: Option<u64>) -> io::Result<Vec<Entry>> {
    #[cfg(not(feature = "hostsim"))]
    if let Some(id) = _mount_id {
        let entries = crate::port::list_usb_folder(path, id)
            .map_err(|error| io::Error::other(format!("Homebrew USB ({error:?})")))?;
        let mut entries: Vec<_> = entries.into_iter().filter_map(|(name, dir)| disk::media_entry(name, dir)).collect();
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

fn display_name(entry: &Entry) -> String {
    let name = std::path::Path::new(&entry.name);
    match entry.kind {
        Kind::Folder => entry.name.to_string_lossy().into_owned(),
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
        duration_ms: None, audio, subs, video_fps: info.fps,
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
    let Some((file, info, title)) = app.usb.update() else { return };
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
        if playback::start_playback(ps, &mut app.adapters.player, 0, playback::Origin::Here,
            4500, None, &mut app.pages, &mut app.bridge) {
            app.bridge.metadata_run(plx_data::stores::metadata::MetadataCmd::InstallPlaying(
                Some(item)));
            return;
        }
        plx_media::route::clear_url(ps);
    }
    app.usb.status = tr("No se pudo iniciar el reproductor con este vídeo.", "No s’ha pogut reproduir aquest vídeo.").into();
}

#[cfg(test)]
mod usb_browser_tests {
    use super::*;

    #[test]
    fn delete_from_video_menu_defaults_to_cancel() {
        let mut browser = Browser::new();
        let volume = Volume { path: "/tmp/usb/sda/sda1".into(), label: None, mount_id: Some(42), system: true };
        let path = volume.path.join("Pel·lícula.mkv");
        browser.scan.volumes.push(volume.clone());
        browser.folders.push(Folder {
            path: volume.path.clone(), volume, collection: None, selected: 0,
            entries: vec![Entry { kind: Kind::Video, name: "Pel·lícula.mkv".into() }],
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
