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
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

const TOP: f32 = 338.0;
const COLS: usize = poster_grid::COLS;
const GRID: Geom = Geom::of(COLS, plx_ui::consts::MARGIN_X, &STYLE);

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
}

pub(super) struct Browser {
    scan: Scan,
    folders: Vec<Folder>,
    selected: usize,
    scroll: Spring,
    pop: GridPop,
    status: String,
    greeting: &'static str,
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
            greeting: "Te damos la bienvenida",
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
                        let collection = path.strip_prefix(&volume.path).ok().and_then(art::collection);
                        self.folders.push(Folder { volume, path, entries, collection, selected: 0 });
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
                    // A temporary Homebrew failure must not bring back an old mount from the jail.
                    if (!self.scan.system || scan.system)
                        && (self.scan != scan || self.status == "Buscando discos USB…") {
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
                self.next_scan = now + Duration::from_secs(5);
                let greeting = match plx_platform::i18n::local_hour() {
                    Some(6..=11) => "Buenos días",
                    Some(12..=19) => "Buenas tardes",
                    Some(_) => "Buenas noches",
                    None => "Te damos la bienvenida",
                };
                changed |= greeting != self.greeting;
                self.greeting = greeting;
                let (send, receive) = mpsc::sync_channel(1);
                if plx_base::task::spawn_off_frame("cineusb-volumes", move |_| {
                    let _ = send.send(scan_volumes());
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
        (row as f32 * GRID.pitch() + GRID.h - 570.0).max(0.0)
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
                    self.status = "Abriendo vídeo…".into();
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
            self.status = "Buscando discos USB…".into();
        }
    }

    fn open_folder(&mut self, volume: Volume, path: PathBuf) {
        self.status = "Abriendo carpeta…".into();
        self.request(move || {
            let entries = list_folder(&path, system_mount(&volume));
            if let Err(error) = &entries {
                plx_base::eventlog::log(&format!("cineusb: USB folder listing failed: {error}"));
            }
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

    fn click(&mut self, x: f32, y: f32) {
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
        }
        self.open();
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

    fn item(&self, index: usize) -> Option<(String, &'static str, Cover)> {
        if let Some(folder) = self.folders.last() {
            let entry = folder.entries.get(index)?;
            let (cover, kind) = art::entry_cover(entry, folder.collection);
            Some((display_name(entry), kind, cover))
        } else {
            self.scan.volumes.get(index).map(|volume| (volume_name(volume), "Disco USB", Cover::Drive))
        }
    }

    pub(super) fn draw(&self) {
        let (r, g, b) = theme::CLEAR_RGB;
        gfx::frame_clear(r, g, b);
        let p = Painter::root().alpha(plx_ui::nav::page_alpha());
        let x = plx_ui::consts::MARGIN_X;
        let width = 1920.0 - 2.0 * x;
        draw_line(p, "CineUSB  ·  Biblioteca local", Rect::new(x, 56.0, width, 34.0), theme::size::CAPTION, true);
        let breadcrumb = self.folders.last().map(|folder| {
            let relative = folder.path.strip_prefix(&folder.volume.path).unwrap_or(&folder.path);
            let mut parts = vec![volume_name(&folder.volume)];
            parts.extend(relative.components().map(|part| part.as_os_str().to_string_lossy().into_owned()));
            parts.join("   /   ")
        });
        let focused = self.item(self.selected);
        let (title, detail) = if breadcrumb.is_none() {
            (self.greeting, "Tus películas y series, desde USB")
        } else {
            focused.as_ref().map_or(("Biblioteca USB", "Carpeta"), |(title, kind, _)| (title.as_str(), *kind))
        };
        draw_line(p, title, Rect::new(x, 108.0, width, 96.0), theme::size::HERO, true);
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
            let cells = poster_grid::visible_in(&GRID, self.len(), TOP, self.scroll.pos);
            for i in cells.filter(|index| *index != self.selected) { self.draw_tile(p, i, false); }
            self.draw_tile(p, self.selected, true);
            p.clip_clear();
        }
        let hint = if self.status.is_empty() { "Flechas  Navegar      OK  Abrir / reproducir      Atrás  Volver" } else { &self.status };
        draw_line(p, hint, Rect::new(x, 990.0, width, 48.0), theme::size::CAPTION, false);
    }

    fn draw_tile(&self, p: Painter, index: usize, focused: bool) {
        let Some((title, kind, cover)) = self.item(index) else { return };
        let frame = poster_grid::cell_in(&GRID, index, TOP, self.scroll.pos, &[]);
        let scale = self.pop.scale(index, focused, &STYLE);
        let card = frame.scaled(scale);
        let radius = STYLE.tile_radius(card, scale);
        let lift = ((scale - 1.0) / (STYLE.focus_scale - 1.0)).clamp(0.0, 1.0);
        cover.draw(p, card, radius, lift);
        draw_line(p, kind, Rect::new(card.x + 16.0, card.y + card.h - 81.0, card.w - 32.0, 28.0), theme::size::CAPTION, false);
        widgets::poster_label(p, card, radius, &title, &TtfMeasure);
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
        sid: 0, ordinal: -1, codec: info.audio.clone(), channels: info.audio_channels,
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
    app.usb.status = "No se pudo iniciar el reproductor con este vídeo.".into();
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
