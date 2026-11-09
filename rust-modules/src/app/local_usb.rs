//! CineUSB's standalone startup and local folder browser.
mod disk;

use super::*;
use disk::{Entry, Kind, Volume};
use plx_gfx::{gfx, text};
use plx_ui::{
    label::Label,
    table::{Row, Section, TableView},
    theme, Painter, Rect,
};
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

const LIST: Rect = Rect::new(140.0, 278.0, 1640.0, 625.0);

struct Folder {
    volume: Volume,
    path: PathBuf,
    entries: Vec<Entry>,
    selected: i32,
    scroll: f32,
}

enum ResultFromDisk {
    Volumes(Vec<Volume>),
    Folder(Volume, PathBuf, io::Result<Vec<Entry>>),
    File(String, io::Result<u64>),
}

struct Browser {
    volumes: Vec<Volume>,
    folders: Vec<Folder>,
    table: TableView,
    status: String,
    pending: Option<Receiver<ResultFromDisk>>,
    scanning: Option<Receiver<Vec<Volume>>>,
    next_scan: Instant,
}

impl Browser {
    fn new(now: Instant) -> Self {
        let mut table = TableView::new();
        table.compact = true;
        let mut browser = Self {
            volumes: Vec::new(),
            folders: Vec::new(),
            table,
            status: String::new(),
            pending: None,
            scanning: None,
            next_scan: now,
        };
        browser.rebuild(0, 0.0);
        browser
    }

    fn request(&mut self, run: impl FnOnce() -> ResultFromDisk + Send + 'static) {
        let (send, receive) = mpsc::sync_channel(1);
        if plx_base::task::spawn_off_frame("cineusb-disk", move |_| {
            let _ = send.send(run());
        }) {
            self.pending = Some(receive);
        } else {
            self.status = "No se pudo iniciar la lectura del disco.".into();
        }
    }

    fn update_disk(&mut self, now: Instant) -> bool {
        let mut changed = false;
        if let Some(result) = self.pending.as_ref().map(Receiver::try_recv) {
            match result {
                Ok(result) => {
                    self.pending = None;
                    changed = self.apply(result);
                }
                Err(TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.status = "Se interrumpió la lectura del disco.".into();
                    changed = true;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        // A background hotplug scan must never consume an OK or Back press. Apply its result
        // once a foreground folder/file operation has landed.
        if self.pending.is_none() {
            match self.scanning.as_ref().map(Receiver::try_recv) {
                Some(Ok(volumes)) => {
                    self.scanning = None;
                    changed |= self.apply(ResultFromDisk::Volumes(volumes));
                }
                Some(Err(TryRecvError::Disconnected)) => self.scanning = None,
                _ => {}
            }
            if self.scanning.is_none() && now >= self.next_scan {
                self.next_scan = now + Duration::from_secs(1);
                let (send, receive) = mpsc::sync_channel(1);
                if plx_base::task::spawn_off_frame("cineusb-volumes", move |_| {
                    let _ = send.send(disk::volumes());
                }) {
                    self.scanning = Some(receive);
                }
            }
        }
        changed
    }

    fn apply(&mut self, result: ResultFromDisk) -> bool {
        match result {
            ResultFromDisk::Volumes(volumes) => {
                if self.volumes == volumes {
                    return false;
                }
                let was_selected = self.volumes.get(self.table.sel as usize).cloned();
                self.volumes = volumes;
                if let Some(folder) = self.folders.last() {
                    if self.volumes.contains(&folder.volume) {
                        return false;
                    }
                    self.folders.clear();
                    self.status = "El disco ya no está disponible.".into();
                } else {
                    self.status.clear();
                }
                let selected = was_selected
                    .and_then(|volume| self.volumes.iter().position(|item| *item == volume))
                    .unwrap_or_default() as i32;
                self.rebuild(selected, 0.0);
            }
            ResultFromDisk::Folder(volume, path, Ok(entries)) => {
                if let Some(parent) = self.folders.last_mut() {
                    parent.selected = self.table.sel;
                    parent.scroll = self.table.scroll_pos();
                }
                self.folders.push(Folder {
                    volume,
                    path,
                    entries,
                    selected: 0,
                    scroll: 0.0,
                });
                self.status.clear();
                self.rebuild(0, 0.0);
            }
            ResultFromDisk::Folder(_, _, Err(error)) => {
                self.status = format!("No se pudo abrir la carpeta · {error}");
            }
            ResultFromDisk::File(name, Ok(bytes)) => {
                let mib = bytes as f64 / (1024.0 * 1024.0);
                self.status = format!("Lectura correcta · {name} · {mib:.1} MiB");
            }
            ResultFromDisk::File(name, Err(error)) => {
                self.status = format!("No se pudo leer {name} · {error}");
            }
        }
        true
    }

    fn rebuild(&mut self, selected: i32, scroll: f32) {
        let mut section = Section::new("");
        let empty = if let Some(folder) = self.folders.last() {
            section.rows = folder
                .entries
                .iter()
                .map(|entry| {
                    let row = Row::new(entry.name.to_string_lossy());
                    match entry.kind {
                        Kind::Folder => row.chevron(true),
                        Kind::Video => row,
                    }
                })
                .collect();
            "Esta carpeta no contiene subcarpetas ni vídeos MKV o MP4."
        } else {
            section.rows = self
                .volumes
                .iter()
                .map(|volume| Row::new(volume_name(volume)).chevron(true))
                .collect();
            "Conecta un disco USB para empezar."
        };
        self.table.set_empty_text(Some(empty));
        self.table
            .restore_sections(vec![section], Some(selected), scroll);
    }

    fn open_selected(&mut self) {
        if self.pending.is_some() {
            return;
        }
        if let Some(folder) = self.folders.last() {
            let Some(entry) = folder.entries.get(self.table.sel as usize) else {
                return;
            };
            let path = folder.path.join(&entry.name);
            match entry.kind {
                Kind::Folder => self.open_folder(folder.volume.clone(), path),
                Kind::Video => {
                    let name = entry.name.to_string_lossy().into_owned();
                    self.status = format!("Comprobando lectura · {name}");
                    self.request(move || ResultFromDisk::File(name, disk::verify_read(&path)));
                }
            }
        } else if let Some(volume) = self.volumes.get(self.table.sel as usize).cloned() {
            self.open_folder(volume.clone(), volume.path.clone());
        }
    }

    fn open_folder(&mut self, volume: Volume, path: PathBuf) {
        self.status = "Abriendo carpeta…".into();
        self.request(move || {
            let entries = disk::listing(&path);
            ResultFromDisk::Folder(volume, path, entries)
        });
    }

    fn back(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let Some(popped) = self.folders.pop() else {
            plx_platform::tv::home::go_home();
            return;
        };
        self.status.clear();
        if let Some(parent) = self.folders.last() {
            self.rebuild(parent.selected, parent.scroll);
        } else {
            let selected = self
                .volumes
                .iter()
                .position(|volume| *volume == popped.volume);
            self.rebuild(selected.unwrap_or_default() as i32, 0.0);
        }
    }

    fn key(&mut self, key: Key, repeat: bool) -> bool {
        match key {
            Key::Up => self.table.move_sel(-1),
            Key::Down => self.table.move_sel(1),
            Key::Ok | Key::Right { alt: false } if !repeat => self.open_selected(),
            Key::Back | Key::Left { alt: false } if !repeat => self.back(),
            _ => return false,
        }
        true
    }

    fn draw(&self) {
        let p = Painter::root();
        let (r, g, b) = theme::CLEAR_RGB;
        gfx::frame_clear(r, g, b);
        Label::new(c"CineUSB".as_ptr(), theme::size::HERO, theme::TEXT_PRIMARY)
            .bold()
            .draw(p, Rect::new(146.0, 88.0, 1628.0, 100.0));
        let mut crumb = String::from("USB");
        if let Some(folder) = self.folders.last() {
            crumb.push_str("  /  ");
            crumb.push_str(&volume_name(&folder.volume));
            if let Ok(relative) = folder.path.strip_prefix(&folder.volume.path) {
                for part in relative.components() {
                    crumb.push_str("  /  ");
                    crumb.push_str(&part.as_os_str().to_string_lossy());
                }
            }
        }
        draw_line(p, &crumb, Rect::new(146.0, 204.0, 1628.0, 48.0));
        self.table.draw(p, LIST, &text::TtfMeasure);
        let hint = if self.status.is_empty() {
            "↑ ↓  Navegar     ·     OK  Abrir o comprobar lectura     ·     Atrás  Volver"
        } else {
            &self.status
        };
        draw_line(p, hint, Rect::new(146.0, 920.0, 1628.0, 50.0));
    }
}

fn volume_name(volume: &Volume) -> String {
    volume
        .path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Disco USB".into())
}

fn draw_line(p: Painter, line: &str, frame: Rect) {
    use plx_machine::machine::Measure;
    let size = theme::size::CAPTION;
    let (line, _) = text::TtfMeasure.fit_line_measured(line, frame.w, size, false);
    Label::new(line.as_ptr(), size, theme::TEXT_SECONDARY).draw(p, frame);
}

pub(super) unsafe fn run() -> c_int {
    plx_base::eventlog::log(concat!("cineusb@", env!("PLX_VERSION")));
    plx_platform::tv::probe_device();
    let (graphics, _) = match super::boot::open_graphics(true, || None) {
        Ok(graphics) => graphics,
        Err(code) => return code,
    };
    let win = graphics.win;
    let mut previous = Instant::now();
    let mut browser = Browser::new(previous);
    let mut event = [0u8; 128];
    let mut dirty = true;
    let mut moving = false;
    'running: loop {
        while SDL_PollEvent(event.as_mut_ptr().cast()) != 0 {
            match rd_u32(&event, 0) {
                SDL_QUIT => break 'running,
                SDL_KEYDOWN => {
                    let (state, wcode, sym) = decode_key(&event);
                    let key = classify(sym, wcode);
                    if key == Key::Exit {
                        break 'running;
                    }
                    dirty |= browser.key(key, state & 0x100 != 0);
                }
                0x200 | 0x106 => dirty = true, // SDL window and foreground events
                _ => {}
            }
        }
        let now = Instant::now();
        dirty |= browser.update_disk(now);
        if dirty || moving {
            moving = browser
                .table
                .update_for_present((now - previous).as_secs_f32().min(0.05), LIST.h);
            dirty |= moving;
        }
        previous = now;
        plx_platform::tv::home::poll();
        if dirty {
            text::begin_frame();
            let (x, y, w, h) = plx_base::surface::viewport();
            glViewport(x, y, w, h);
            plx_ui::guard(|| browser.draw());
            SDL_GL_SwapWindow(win);
            dirty = false;
        } else {
            SDL_Delay(8);
        }
    }
    SDL_DestroyWindow(win);
    SDL_Quit();
    0
}
