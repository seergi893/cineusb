//! Optional, offline Christmas decorations and menu audio.
use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void};
use plx_gfx::{gfx, img};
use plx_ui::{Painter, Rect, theme};

#[derive(Default)]
pub(super) struct Art {
    garland: Cell<Option<u32>>,
    tree: Cell<Option<u32>>,
}

impl Art {
    pub(super) fn draw(&self, p: Painter, home: bool, settings: bool) {
        if p.is_recording() { return; }
        let garland = texture(&self.garland, include_bytes!("../../../../assets/cineusb/christmas-garland.png"));
        if garland != 0 {
            p.tex(garland, Rect::new(0.0, 0.0, 115.0, 77.0), 0.0, theme::TINT_WHITE);
            p.tex_uv(garland, [1.0, 0.0, 0.0, 1.0], Rect::new(1805.0, 0.0, 115.0, 77.0), 0.0, theme::TINT_WHITE);
        }
        let area = if settings { Rect::new(114.0, 665.0, 337.5, 225.0) }
            else if home { Rect::new(1600.0, 921.0, 207.0, 138.0) }
            else { return };
        let tree = texture(&self.tree, include_bytes!("../../../../assets/cineusb/christmas-tree.png"));
        if tree != 0 { p.tex(tree, area, 0.0, theme::TINT_WHITE); }
    }
}

impl Drop for Art {
    fn drop(&mut self) {
        for id in [self.garland.get(), self.tree.get()].into_iter().flatten().filter(|id| *id != 0) {
            gfx::delete_tex(id);
        }
    }
}

fn texture(slot: &Cell<Option<u32>>, bytes: &[u8]) -> u32 {
    if let Some(id) = slot.get() { return id; }
    let id = img::img_decode_owned(bytes).map_or(0, |(w, h, rgba)| {
        gfx::upload_rgba(0, w as c_int, h as c_int, rgba.as_ptr())
    });
    slot.set(Some(id));
    id
}

// An original instrumental loop, 24 kHz mono signed 16-bit little-endian PCM.
const MUSIC: &[u8] = include_bytes!("../../../../assets/cineusb/christmas-menu.pcm");
const AUDIO_INIT: u32 = 0x10; // SDL_INIT_AUDIO
const AUDIO_S16LE: u16 = 0x8010;

#[repr(C)]
struct AudioSpec {
    frequency: c_int,
    format: u16,
    channels: u8,
    silence: u8,
    samples: u16,
    padding: u16,
    size: u32,
    callback: Option<unsafe extern "C" fn(*mut c_void, *mut u8, c_int)>,
    user_data: *mut c_void,
}

type AudioInit = unsafe extern "C" fn(u32) -> c_int;
type AudioQuit = unsafe extern "C" fn(u32);
type AudioOpen = unsafe extern "C" fn(*const c_char, c_int, *const AudioSpec, *mut AudioSpec, c_int) -> u32;
type AudioPause = unsafe extern "C" fn(u32, c_int);
type AudioClose = unsafe extern "C" fn(u32);

struct AudioApi {
    init: AudioInit,
    quit: AudioQuit,
    open: AudioOpen,
    pause: AudioPause,
    close: AudioClose,
}

impl AudioApi {
    fn resolve() -> Option<Self> {
        // Older firmware can omit SDL audio. Resolve from the SDL already in use,
        // so a missing symbol leaves the decoration usable and adds no ELF dependency.
        let handle = plx_base::dynlib::Handle::self_handle();
        macro_rules! symbol {
            ($name:literal, $ty:ty) => {{
                let pointer = handle.sym($name)?;
                if pointer.is_null() { return None; }
                // SAFETY: these typedefs mirror SDL2's audio API and the library is already loaded.
                unsafe { std::mem::transmute::<*mut c_void, $ty>(pointer) }
            }};
        }
        Some(Self {
            init: symbol!("SDL_InitSubSystem", AudioInit),
            quit: symbol!("SDL_QuitSubSystem", AudioQuit),
            open: symbol!("SDL_OpenAudioDevice", AudioOpen),
            pause: symbol!("SDL_PauseAudioDevice", AudioPause),
            close: symbol!("SDL_CloseAudioDevice", AudioClose),
        })
    }
}

struct Device {
    api: AudioApi,
    id: u32,
    _cursor: Box<usize>,
}

impl Device {
    fn open() -> Option<Self> {
        let api = AudioApi::resolve()?;
        // SAFETY: this lease is released on open failure or by Device::drop.
        if unsafe { (api.init)(AUDIO_INIT) } != 0 { return None; }
        let mut cursor = Box::new(0usize);
        let spec = AudioSpec { frequency: 24000, format: AUDIO_S16LE, channels: 1, silence: 0,
            samples: 512, padding: 0, size: 0, callback: Some(fill_music),
            user_data: (&mut *cursor as *mut usize).cast() };
        // SAFETY: the callback's cursor outlives the device. No changes are allowed
        // to the PCM format expected by the callback.
        let id = unsafe { (api.open)(std::ptr::null(), 0, &spec, std::ptr::null_mut(), 0) };
        if id == 0 {
            unsafe { (api.quit)(AUDIO_INIT) };
            return None;
        }
        unsafe { (api.pause)(id, 0) };
        Some(Self { api, id, _cursor: cursor })
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // SDL joins the callback before returning, so it cannot see a freed cursor.
        unsafe {
            (self.api.close)(self.id);
            (self.api.quit)(AUDIO_INIT);
        }
    }
}

unsafe extern "C" fn fill_music(user_data: *mut c_void, stream: *mut u8, len: c_int) {
    if len <= 0 { return; }
    // SAFETY: SDL provides a writable audio buffer and serializes calls per device.
    let cursor = unsafe { &mut *user_data.cast::<usize>() };
    let output = unsafe { std::slice::from_raw_parts_mut(stream, len as usize) };
    let mut written = 0;
    while written < output.len() {
        let count = (MUSIC.len() - *cursor).min(output.len() - written);
        output[written..written + count].copy_from_slice(&MUSIC[*cursor..*cursor + count]);
        *cursor = (*cursor + count) % MUSIC.len();
        written += count;
    }
}

#[derive(Default)]
pub(super) struct Music {
    device: Option<Device>,
    attempted: bool,
}

impl Music {
    pub(super) fn sync(&mut self, requested: bool) {
        if !requested {
            self.device = None;
            self.attempted = false;
        } else if !self.attempted {
            self.attempted = true;
            self.device = Device::open();
            if self.device.is_none() { plx_base::eventlog::log("cineusb: Christmas menu audio unavailable"); }
        }
    }
}

#[cfg(test)]
mod usb_browser_tests {
    use super::*;

    #[test]
    fn christmas_audio_callback_repeats_the_local_loop() {
        let mut cursor = MUSIC.len() - 4;
        let mut output = [0u8; 8];
        // SAFETY: this supplies one live cursor and a writable buffer of exactly eight bytes.
        unsafe { fill_music((&mut cursor as *mut usize).cast(), output.as_mut_ptr(), output.len() as c_int) };
        assert_eq!(&output[..4], &MUSIC[MUSIC.len() - 4..]);
        assert_eq!(&output[4..], &MUSIC[..4]);
        assert_eq!(cursor, 4);
    }
}
