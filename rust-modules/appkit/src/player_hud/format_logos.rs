//! The bundled technology artwork is shared by the playback HUD and USB library.
use plx_gfx::{gfx, img};
use plx_ui::{Painter, Rect, theme};

const ART: [(&str, &[u8]); 5] = [
    ("Dolby Vision", include_bytes!("art/dolby-vision.png")),
    ("HDR10+", include_bytes!("art/hdr10-plus.png")),
    ("Dolby Digital", include_bytes!("art/dolby-digital.png")),
    ("Dolby Digital Plus", include_bytes!("art/dolby-digital-plus.png")),
    ("DTS", include_bytes!("art/dts.png")),
];

#[derive(Default)]
pub struct FormatLogos([Option<Option<FormatLogo>>; ART.len()]);

impl FormatLogos {
    pub fn prepare(&mut self, name: &str) {
        if let Some(index) = ART.iter().position(|(label, _)| *label == name) {
            self.0[index].get_or_insert_with(|| FormatLogo::load(ART[index].1));
        }
    }

    pub fn get(&self, name: &str) -> Option<&FormatLogo> {
        let index = ART.iter().position(|(label, _)| *label == name)?;
        self.0[index].as_ref()?.as_ref()
    }
}

pub struct FormatLogo {
    texture: u32,
    aspect: f32,
}

impl FormatLogo {
    fn load(data: &[u8]) -> Option<Self> {
        let (width, height, pixels) = img::img_decode_owned(data)?;
        let texture = gfx::upload_rgba(0, width as i32, height as i32, pixels.as_ptr());
        (texture != 0).then_some(Self { texture, aspect: width as f32 / height as f32 })
    }

    pub fn width(&self, height: f32) -> f32 {
        height * self.aspect
    }

    pub fn draw(&self, p: Painter, slot: Rect, alpha: f32) {
        let height = slot.h.min(slot.w / self.aspect);
        let width = self.width(height);
        p.tex(self.texture, Rect::new(slot.x, slot.y + (slot.h - height) / 2.0, width, height),
            0.0, theme::with_a(theme::TINT_WHITE, alpha));
    }
}

impl Drop for FormatLogo {
    fn drop(&mut self) {
        gfx::delete_tex(self.texture);
    }
}

#[test]
fn cineusb_format_art_decodes_with_transparency() {
    for (name, data) in ART {
        let (width, height, pixels) = img::img_decode_owned(data).expect(name);
        assert!(width > height && height >= 64, "{name}");
        assert!(pixels.chunks_exact(4).any(|pixel| pixel[3] == 0), "{name}");
        assert!(pixels.chunks_exact(4).any(|pixel| pixel[3] == 255), "{name}");
    }
}
