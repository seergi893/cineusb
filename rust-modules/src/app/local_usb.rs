//! USB browsing and its off-frame filesystem jobs.
mod disk;

use super::{App, Key, playback};
use disk::{Entry, Kind, Scan, Volume};
use plx_gfx::{gfx, text::TtfMeasure};
use plx_machine::machine::Measure;
use plx_ui::{
    icons::{self, Icon},
    label::{HAlign, Label},
    poster_grid::{self, Card, GridPop, STYLE},
    screen::Dir,
    theme, widgets::{self, Art}, Painter, Rect, Spring,
};
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

const TOP: f32 = 338.0;
const COLS: usize = poster_grid::COLS;

struct Folder {
    volume: Volume,
    path: PathBuf,
    entries: Vec<Entry>,
    selected: usize,
}

enum DiskResult {
    Folder(Volume, PathBuf, io::Result<Vec<Entry>>),
    Video(Result<(File, plx_play_feed::LocalMedia, String), String>),
}

pub(super) struct Browser {
    scan: Scan,
    folders: Vec<Folder>,
    selected: usize,
    scroll: Spring,
    pop: GridPop,
    status: String,
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
            scroll: Spring::at(0.0), pop: GridPop::new(), status: "Buscando discos USB…".into(),
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
            self.status = "No se pudo iniciar la lectura del disco.".into();
        }
    }

    fn update(&mut self) -> Option<(File, plx_play_feed::LocalMedia, String)> {
        let now = Instant::now();
        let dt = (now - self.last_tick).as_secs_f32().min(0.05);
        self.last_tick = now;
        let mut changed = false;
        let mut video = None;
        match self.pending.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => {
                self.pending = None;
                changed = true;
                match result {
                    DiskResult::Folder(volume, path, Ok(entries)) => {
                        if let Some(folder) = self.folders.last_mut() { folder.selected = self.selected; }
                        self.folders.push(Folder { volume, path, entries, selected: 0 });
                        self.select(0, true);
                        self.status.clear();
                    }
                    DiskResult::Folder(_, _, Err(error)) => {
                        self.status = format!("No se pudo abrir la carpeta · {error}");
                    }
                    DiskResult::Video(Ok(media)) => {
                        video = Some(media);
                        self.status.clear();
                    }
                    DiskResult::Video(Err(error)) => self.status = error,
                }
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.pending = None;
                self.status = "Se interrumpió la lectura del disco.".into();
                changed = true;
            }
            _ => {}
        }
        if self.pending.is_none() {
            match self.scanning.as_ref().map(Receiver::try_recv) {
                Some(Ok(scan)) => {
                    self.scanning = None;
                    if self.scan != scan || self.status == "Buscando discos USB…" {
                        let selected = self.scan.volumes.get(self.selected).cloned();
                        self.scan = scan;
                        if self.folders.last().is_some_and(|folder| !self.scan.volumes.contains(&folder.volume)) {
                            self.folders.clear();
                            self.status = "Se ha retirado el disco USB.".into();
                            video = None;
                        }
                        if self.folders.is_empty() {
                            let index = selected.and_then(|volume| self.scan.volumes.iter().position(|item| *item == volume));
                            self.select(index.unwrap_or_default(), true);
                        }
                        if self.status == "Buscando discos USB…" { self.status.clear(); }
                        changed = true;
                    }
                }
                Some(Err(TryRecvError::Disconnected)) => self.scanning = None,
                _ => {}
            }
            if self.scanning.is_none() && now >= self.next_scan {
                self.next_scan = now + Duration::from_secs(4);
                let (send, receive) = mpsc::sync_channel(1);
                if plx_base::task::spawn_off_frame("cineusb-volumes", move |_| {
                    let scan = disk::volumes();
                    #[cfg(not(feature = "hostsim"))]
                    let scan = if scan.hidden_mounts || scan.volumes.is_empty() || scan.volumes.iter().any(Volume::bridged) {
                        let _ = crate::port::sync_usb_volumes();
                        disk::volumes()
                    } else { scan };
                    let _ = send.send(scan);
                }) {
                    self.scanning = Some(receive);
                }
            }
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
        let pitch = poster_grid::Geom::DEFAULT.pitch();
        row.saturating_sub(1) as f32 * pitch
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
                    self.status = "Abriendo vídeo…".into();
                    self.request(move || {
                        let result = disk::open_video(&path).map_err(|err| format!("No se pudo abrir el vídeo · {err}"))
                            .and_then(|file| plx_play_feed::probe_local(&file)
                                .map(|info| (file, info, title)).map_err(str::to_owned));
                        DiskResult::Video(result)
                    });
                }
            }
        } else if let Some(volume) = self.scan.volumes.get(self.selected).cloned() {
            self.open_folder(volume.clone(), volume.path.clone());
        } else {
            self.next_scan = Instant::now();
            self.status = "Buscando discos USB…".into();
        }
    }

    fn open_folder(&mut self, volume: Volume, path: PathBuf) {
        self.status = "Abriendo carpeta…".into();
        self.request(move || {
            let entries = disk::listing(&path);
            DiskResult::Folder(volume, path, entries)
        });
    }

    fn back(&mut self) {
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

    fn key(&mut self, key: Key, repeat: bool) {
        let dir = match key {
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

    fn item(&self, index: usize) -> Option<(String, &'static str, Icon)> {
        if let Some(folder) = self.folders.last() {
            let entry = folder.entries.get(index)?;
            let (kind, icon) = match entry.kind {
                Kind::Folder => ("Carpeta", Icon::Chevron),
                Kind::Video => ("Vídeo", Icon::Play),
            };
            Some((display_name(entry), kind, icon))
        } else {
            self.scan.volumes.get(index).map(|volume| (volume_name(volume), "Disco USB", Icon::Chevron))
        }
    }

    pub(super) fn draw(&self) {
        let (r, g, b) = theme::CLEAR_RGB;
        gfx::frame_clear(r, g, b);
        let p = Painter::root().alpha(plx_ui::nav::page_alpha());
        let x = plx_ui::consts::MARGIN_X;
        let width = 1920.0 - 2.0 * x;
        draw_line(p, "CINEUSB    /    BIBLIOTECA LOCAL", Rect::new(x, 56.0, width, 34.0), theme::size::CAPTION, false);
        let breadcrumb = self.folders.last().map(|folder| {
            let relative = folder.path.strip_prefix(&folder.volume.path).unwrap_or(&folder.path);
            let mut parts = vec![volume_name(&folder.volume)];
            parts.extend(relative.components().map(|part| part.as_os_str().to_string_lossy().into_owned()));
            parts.join("   /   ")
        });
        let focused = self.item(self.selected);
        let title = focused.as_ref().map_or("Tus discos USB", |(title, _, _)| title);
        draw_line(p, title, Rect::new(x, 108.0, width, 96.0), theme::size::HERO, true);
        let detail = focused.as_ref().map_or("Películas, series y carpetas", |(_, kind, _)| *kind);
        draw_line(p, detail, Rect::new(x, 206.0, width, 42.0), theme::size::BODY, false);
        let section = breadcrumb.as_deref().unwrap_or("Discos conectados");
        draw_line(p, section, Rect::new(x, 272.0, width, 48.0), theme::size::HEADLINE, true);

        if self.len() == 0 {
            let message = if self.folders.last().is_some() {
                "Esta carpeta no contiene subcarpetas ni vídeos compatibles."
            } else if self.scan.hidden_mounts {
                "webOS detecta el USB, pero CineUSB todavía no puede leer su montaje. Pulsa OK para reintentar."
            } else {
                "Conecta un disco USB directamente a la televisión."
            };
            draw_line(p, message, Rect::new(x, 380.0, width, 70.0), theme::size::BODY, false);
        } else {
            p.clip(Rect::new(0.0, TOP - 18.0, 1920.0, 626.0));
            let cells = poster_grid::visible_in(&poster_grid::Geom::DEFAULT, self.len(), TOP, self.scroll.pos, &[]);
            for i in cells.filter(|index| *index != self.selected) { self.draw_tile(p, i, false); }
            self.draw_tile(p, self.selected, true);
            p.clip_clear();
        }
        let hint = if self.status.is_empty() { "Flechas  Navegar      OK  Abrir / reproducir      Atrás  Volver" } else { &self.status };
        draw_line(p, hint, Rect::new(x, 990.0, width, 48.0), theme::size::CAPTION, false);
    }

    fn draw_tile(&self, p: Painter, index: usize, focused: bool) {
        let Some((title, _, icon)) = self.item(index) else { return };
        let frame = poster_grid::cell(index, TOP, self.scroll.pos, &[]);
        let scale = self.pop.scale(index, focused, &STYLE);
        let card = frame.scaled(scale);
        let radius = STYLE.tile_radius(card, scale);
        widgets::card(p, frame, Art::Poster(None), radius, focused, scale, 0.0);
        icons::draw(p, icon, Rect::new(card.x + (card.w - 64.0) / 2.0,
            card.y + (card.h - 64.0) / 2.0, 64.0, 64.0), theme::TEXT_SECONDARY);
        widgets::poster_label(p, card, radius, &title, &TtfMeasure);
    }
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
        .map_or_else(|| name.into_owned(), |(_, label)| label.to_owned())
}

fn draw_line(p: Painter, line: &str, frame: Rect, size: i32, strong: bool) {
    let (line, _) = TtfMeasure.fit_line_measured(line, frame.w, size, strong);
    let ink = if strong { theme::TEXT_PRIMARY } else { theme::TEXT_SECONDARY };
    let label = Label::new(line.as_ptr(), size, ink).h(HAlign::Left);
    if strong { label.bold().draw(p, frame); } else { label.draw(p, frame); }
}

pub(super) fn key(app: &mut App, key: Key, repeat: bool) {
    if key == Key::Exit { app.running = false; }
    else if !super::bridge::player_arriving(&app.pages) { app.usb.key(key, repeat); }
    plx_machine::idle::invalidate();
}

pub(super) fn tick(app: &mut App) {
    let Some((file, info, title)) = app.usb.update() else { return };
    let ps = &mut app.player.session;
    let dovi = info.dovi.map_or(plx_data::metadata::Dovi::NONE, |record| plx_data::metadata::Dovi {
        present: record.dv_profile > 0, profile: record.dv_profile,
        bl_compat: record.dv_bl_signal_compatibility_id, el_present: record.el_present_flag != 0,
        ..plx_data::metadata::Dovi::NONE
    });
    let ok = plx_media::route::set_stream_declaration(ps, &info.video, &info.audio, info.fps, dovi, false)
        && plx_media::route::set_local_source(ps, file, &title);
    if ok {
        plx_media::route::set_stream_source_raster(ps, info.width, info.height);
        if playback::start_playback(ps, &mut app.adapters.player, 0, playback::Origin::Here,
            4500, None, &mut app.pages, &mut app.bridge) { return; }
        plx_media::route::clear_url(ps);
    }
    app.usb.status = "No se pudo iniciar el reproductor con este vídeo.".into();
}
