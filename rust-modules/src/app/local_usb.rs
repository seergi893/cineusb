//! USB browsing and its off-frame filesystem jobs.
mod art;
mod disk;

use art::Cover;
use super::{App, Key, playback};
use disk::{Entry, Kind, Scan, Volume};
use plx_gfx::{gfx, text::TtfMeasure};
use plx_machine::machine::Measure;
use plx_ui::{
    label::{HAlign, Label},
    poster_grid::{self, Card, Geom, GridPop, STYLE},
    screen::Dir,
    theme, widgets, Painter, Rect, Spring,
};
use std::fs::File;
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
enum Action { Settings, Delete, Eject }

const ACTIONS: [(Action, Rect); 3] = [
    (Action::Settings, Rect::new(882.0, 46.0, 186.0, 56.0)),
    (Action::Delete, Rect::new(1086.0, 46.0, 240.0, 56.0)),
    (Action::Eject, Rect::new(1344.0, 46.0, 480.0, 56.0)),
];
const PANEL: Rect = Rect::new(510.0, 324.0, 900.0, 376.0);
const OPTIONS: [Rect; 2] = [
    Rect::new(555.0, 592.0, 390.0, 62.0),
    Rect::new(975.0, 592.0, 390.0, 62.0),
];

enum Dialog {
    Settings,
    Delete { volume: Volume, path: PathBuf },
    Eject(Volume),
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
    language: Option<(Preference, Receiver<bool>)>,
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
            hour: None, action: None, dialog: None, option: 0, mutating: false, language: None,
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
        if let Some((preference, result)) = self.language.as_ref() {
            match result.try_recv() {
                Ok(saved) => {
                    if saved {
                        i18n::apply_usb_language(*preference);
                        self.dialog = None;
                        self.status.clear();
                    } else {
                        self.status = tr("No se pudo guardar el idioma.", "No s’ha pogut desar l’idioma.").into();
                    }
                    self.language = None;
                    changed = true;
                }
                Err(TryRecvError::Disconnected) => {
                    self.language = None;
                    self.status = tr("No se pudo guardar el idioma.", "No s’ha pogut desar l’idioma.").into();
                    changed = true;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        match self.pending.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => {
                self.pending = None;
                self.mutating = false;
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
                            self.status = tr("Se ha retirado el disco USB.", "S’ha retirat el disc USB.").into();
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

    fn volume(&self) -> Option<&Volume> {
        self.folders.last().map(|folder| &folder.volume)
            .or_else(|| self.scan.volumes.get(self.selected))
    }

    fn selected_video(&self) -> Option<(&Volume, PathBuf)> {
        let folder = self.folders.last()?;
        let entry = folder.entries.get(self.selected)?;
        (entry.kind == Kind::Video).then(|| (&folder.volume, folder.path.join(&entry.name)))
    }

    fn action_available(&self, action: Action) -> bool {
        match action {
            Action::Settings => true,
            Action::Delete => self.selected_video().is_some_and(|(volume, _)| system_mount(volume).is_some()),
            Action::Eject => self.volume().and_then(system_mount).is_some(),
        }
    }

    fn open_action(&mut self, action: Action) {
        if self.pending.is_some() || !self.action_available(action) { return; }
        self.option = 0;
        self.status.clear();
        self.dialog = match action {
            Action::Settings => {
                self.option = usize::from(i18n::current().language() == Language::Ca);
                Some(Dialog::Settings)
            }
            Action::Delete => self.selected_video().map(|(volume, path)| Dialog::Delete { volume: volume.clone(), path }),
            Action::Eject => self.volume().cloned().map(Dialog::Eject),
        };
    }

    fn choose_option(&mut self) {
        if self.language.is_some() { return; }
        match self.dialog.take() {
            Some(Dialog::Settings) => {
                let preference = if self.option == 0 { Preference::Es } else { Preference::Ca };
                let (reply, receive) = mpsc::channel();
                super::preferences::execute(plx_screens::registry::PreferenceCmd::Language { language: preference, reply });
                self.language = Some((preference, receive));
                self.dialog = Some(Dialog::Settings);
            }
            Some(Dialog::Delete { volume, path }) if self.option == 1 => {
                if !self.scan.volumes.contains(&volume) { return; }
                self.status = tr("Eliminando archivo…", "Esborrant el fitxer…").into();
                self.request(move || {
                    let result = mutate_usb(Action::Delete, &volume, &path);
                    DiskResult::Delete(path, result)
                });
                self.mutating = self.pending.is_some();
            }
            Some(Dialog::Eject(volume)) if self.option == 1 => {
                if !self.scan.volumes.contains(&volume) { return; }
                self.status = tr("Expulsando disco…", "Expulsant el disc…").into();
                self.request(move || {
                    let result = mutate_usb(Action::Eject, &volume, &volume.path);
                    DiskResult::Eject(volume, result)
                });
                self.mutating = self.pending.is_some();
            }
            _ => {}
        }
    }

    fn finish_mutation(&mut self, result: Result<(), String>, success: &str, failure: &str) {
        self.scanning = None;
        self.next_scan = Instant::now();
        self.status = match result {
            Ok(()) => success.into(),
            Err(error) => {
                plx_base::eventlog::log(&format!("cineusb: USB action failed: {error}"));
                failure.into()
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
            let path = folder.path.join(&entry.name);
            match entry.kind {
                Kind::Folder => self.open_folder(folder.volume.clone(), path),
                Kind::Video => {
                    let title = display_name(entry);
                    let mount_id = system_mount(&folder.volume);
                    self.status = tr("Abriendo vídeo…", "Obrint el vídeo…").into();
                    self.request(move || {
                        let result = inspect_video(&path, mount_id).map(|(file, info)| (file, info, title));
                        DiskResult::Video(result)
                    });
                }
            }
        } else if let Some(volume) = self.scan.volumes.get(self.selected).cloned() {
            self.open_folder(volume.clone(), volume.path.clone());
        } else {
            self.next_scan = Instant::now();
            self.status = tr("Buscando discos USB…", "Buscant discos USB…").into();
        }
    }

    fn open_folder(&mut self, volume: Volume, path: PathBuf) {
        self.status = tr("Abriendo carpeta…", "Obrint la carpeta…").into();
        self.request(move || {
            let entries = list_folder(&path, system_mount(&volume));
            DiskResult::Folder(volume, path, entries)
        });
    }

    fn back(&mut self) {
        if self.mutating || self.language.is_some() { return; }
        if self.dialog.take().is_some() || self.action.take().is_some() { return; }
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
        if self.mutating || self.language.is_some() { return; }
        if self.dialog.is_some() {
            if let Some(index) = OPTIONS.iter().position(|rect| hit(*rect, x, y)) {
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
        if self.mutating || self.language.is_some() { return; }
        if self.dialog.is_some() {
            match key {
                Key::Left { alt: false } => self.option = 0,
                Key::Right { alt: false } => self.option = 1,
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
                self.action = Some(Action::Settings);
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
                    Action::Delete => tr("Eliminar archivo", "Esborra el fitxer"),
                    Action::Eject => tr("Desconectar USB de forma segura", "Expulsa l’USB amb seguretat"),
                };
                draw_button(p, rect, label, self.action == Some(action), action == Action::Delete);
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
        draw_line(p, detail, Rect::new(x, 206.0, width, 42.0), theme::size::BODY, false);
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
        let hint = if self.status.is_empty() {
            tr("↑ Acciones    ·    OK Abrir o reproducir    ·    Atrás Volver",
                "↑ Accions    ·    OK Obre o reprodueix    ·    Enrere Torna")
        } else { &self.status };
        draw_line(p, hint, Rect::new(x, 990.0, width, 48.0), theme::size::CAPTION, false);
        if let Some(dialog) = &self.dialog { self.draw_dialog(p, dialog); }
    }

    fn draw_dialog(&self, p: Painter, dialog: &Dialog) {
        let cancel = tr("Cancelar", "Cancel·la");
        let (title, subject, detail, options) = match dialog {
            Dialog::Settings => (tr("Idioma de CineUSB", "Idioma del CineUSB"), String::new(),
                if self.language.is_some() { tr("Guardando…", "Desant…") }
                else if self.status.is_empty() { tr("Elige el idioma de la app.", "Tria en quin idioma vols fer servir l’app.") }
                else { &self.status }, ["Castellano", "Català"]),
            Dialog::Delete { path, .. } => (tr("¿Eliminar este archivo?", "Vols esborrar aquest fitxer?"),
                path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                tr("Se borrará del disco y no se podrá deshacer.", "S’esborrarà del disc i no el podràs recuperar."),
                [cancel, tr("Eliminar archivo", "Esborra el fitxer")]),
            Dialog::Eject(volume) => (tr("Desconectar USB de forma segura", "Expulsa l’USB amb seguretat"),
                volume_name(volume), tr("Espera el aviso antes de retirar el disco.", "Espera l’avís abans de treure el disc."),
                [cancel, tr("Expulsar disco", "Expulsa el disc")]),
        };
        let (x, w) = (PANEL.x + 45.0, PANEL.w - 90.0);
        p.rect(Rect::new(0.0, 0.0, 1920.0, 1080.0), 0.0, theme::scrim_black(0.62), theme::scrim_black(0.62), 0.0);
        p.rect_sheened(PANEL, theme::ALERT_PANEL_RAD, theme::PANEL_TOP, theme::PANEL_BOT);
        draw_line(p, title, Rect::new(x, 361.0, w, 60.0), theme::size::HEADLINE, true);
        draw_line(p, &subject, Rect::new(x, 435.0, w, 48.0), theme::size::BODY, true);
        draw_line(p, detail, Rect::new(x, 495.0, w, 42.0), theme::size::CAPTION, false);
        for (index, (rect, label)) in OPTIONS.into_iter().zip(options).enumerate() {
            draw_button(p, rect, label, self.option == index, index == 1 && matches!(dialog, Dialog::Delete { .. }));
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
        draw_line(p, kind, Rect::new(card.x + 16.0, card.y + card.h - 81.0, card.w - 32.0, 28.0), theme::size::CAPTION, false);
        widgets::poster_label(p, card, radius, &title, &TtfMeasure);
    }
}

fn hit(rect: Rect, x: f32, y: f32) -> bool {
    x >= rect.x && x < rect.x + rect.w && y >= rect.y && y < rect.y + rect.h
}

fn draw_button(p: Painter, rect: Rect, label: &str, focused: bool, danger: bool) {
    let (fill, ink) = if focused { (theme::ACCENT, theme::ACCENT_INK) }
        else if danger { (theme::CONTROL_DANGER_IDLE_FILL, theme::CONTROL_DANGER_IDLE_INK) }
        else { (theme::CONTROL_IDLE_FILL, theme::TEXT_PRIMARY) };
    widgets::draw_control_face(p, rect, fill, focused, widgets::ControlGround::Keyed);
    let run = TtfMeasure.fit_line(label, rect.w - 24.0, theme::size::CAPTION, true);
    Label::new(run.as_ptr(), theme::size::CAPTION, ink).h(HAlign::Center).bold().draw(p, rect);
}

fn mutate_usb(action: Action, volume: &Volume, path: &std::path::Path) -> Result<(), String> {
    #[cfg(not(feature = "hostsim"))]
    {
        let id = system_mount(volume).ok_or("missing live USB mount")?;
        let result = match action {
            Action::Delete => crate::port::delete_usb_video(path, id),
            Action::Eject => crate::port::eject_usb_disk(path, id),
            Action::Settings => unreachable!(),
        };
        result.map_err(|error| format!("{error:?}"))
    }
    #[cfg(feature = "hostsim")]
    {
        let _ = (action, volume, path);
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
            volumes: mounts.into_iter().map(|(id, path)| Volume {
                path, mount_id: Some(id), system: true,
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

fn inspect_video(path: &std::path::Path, _mount_id: Option<u64>) -> Result<(File, plx_play_feed::LocalMedia), String> {
    let inspect = |file: File| plx_play_feed::probe_local(&file).map(|info| (file, info));
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
    // Only advertise the audio stream that the current local playback actually uses.
    // Presenting every audio stream would offer switches that the USB route has not wired.
    let audio = if info.audio_stream_index >= 0 {
        let (lang_code, lang) = language(info.audio_language.as_deref());
        vec![Stream {
            id: i64::from(info.audio_stream_index) + 1,
            index: i64::from(info.audio_stream_index),
            lang, lang_code, codec: info.audio.clone(), title: info.audio_title.clone(),
            channels: info.audio_channels, bit_depth: info.audio_depth,
            sampling_rate: info.audio_rate, default: true, selected: true,
            ..Stream::default()
        }]
    } else { Vec::new() };
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
        // The demux ordinal stays -1, so FFmpeg still chooses its probed best audio.
        sid: i64::from(info.audio_stream_index) + 1, ordinal: -1,
        codec: info.audio.clone(), channels: info.audio_channels,
        can_normalize_loudness: false, immersive: false, sample_rate: info.audio_rate,
        bit_depth: info.audio_depth,
    });
    let ok = plx_media::route::set_stream_declaration(ps, &info.video, &info.audio, info.fps, dovi, false)
        && plx_media::route::set_local_source(ps, file, &title, audio, playback::start_caps());
    if ok {
        plx_media::route::set_stream_source_raster(ps, info.width, info.height);
        if playback::start_playback(ps, &mut app.adapters.player, 0, playback::Origin::Here,
            4500, None, &mut app.pages, &mut app.bridge) {
            app.bridge.metadata_run(plx_data::stores::metadata::MetadataCmd::InstallPlaying(
                Some(local_playing_item(&info, dovi))));
            return;
        }
        plx_media::route::clear_url(ps);
    }
    app.usb.status = tr("No se pudo iniciar el reproductor con este vídeo.", "No s’ha pogut reproduir aquest vídeo.").into();
}

#[cfg(test)]
mod usb_subtitle_tests {
    use super::*;

    fn media() -> plx_play_feed::LocalMedia {
        plx_play_feed::LocalMedia {
            video: "hevc".into(), audio: "eac3".into(), audio_channels: 6,
            audio_rate: 48_000, audio_depth: 16, audio_stream_index: 1,
            audio_language: Some("eng".into()), audio_title: "English 5.1".into(),
            width: 3840, height: 2160, fps: 23.976, dovi: None,
            subtitles: vec![
                plx_play_feed::LocalSubtitle { stream_index: 3, codec: "subrip".into(),
                    language: Some("spa".into()), title: "Full".into(),
                    forced: false, default: true, sdh: false },
                plx_play_feed::ff::LocalSubtitle { stream_index: 5, codec: "ass".into(),
                    language: Some("eng".into()), title: "Signs".into(),
                    forced: true, default: false, sdh: false },
                plx_play_feed::ff::LocalSubtitle { stream_index: 7, codec: "hdmv_pgs_subtitle".into(),
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
    }

    #[test]
    fn a_video_without_subtitles_does_not_invent_tracks() {
        let mut input = media();
        input.audio_stream_index = -1;
        input.subtitles.clear();
        let item = local_playing_item(&input, plx_data::metadata::Dovi::NONE);
        assert!(item.audio.is_empty());
        assert!(item.subs.is_empty());
    }
}
