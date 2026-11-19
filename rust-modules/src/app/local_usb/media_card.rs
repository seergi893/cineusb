//! One composition for USB media, with a wider arrangement for direct playback from Home.
use super::{MovieQuality, tr, unpack_progress, video_duration, WATCHED};
use plx_appkit::player_hud::format_logos::{FormatLogo, FormatLogos};
use plx_gfx::{gfx, svg, text::TtfMeasure};
use plx_machine::machine::Measure;
use plx_play_feed::{LocalVideoDetails, VideoRange};
use plx_ui::{Painter, Rect, hstack::{Flow, Item}, icons::{self, Icon}, label::Label, text_view::TextView, theme};
use std::cell::RefCell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Category { Movie, Series, Documentary }

impl Category {
    pub(super) fn for_path(path: &std::path::Path, episodic: bool) -> Self {
        if path.iter().any(|part| super::media_folder_score(&part.to_string_lossy(), super::DOCUMENTALS_TAB) > 0) {
            Self::Documentary
        } else if episodic {
            Self::Series
        } else {
            Self::Movie
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Movie => tr("Película", "Pel·lícula"),
            Self::Series => tr("Serie", "Sèrie"),
            Self::Documentary => "Documental",
        }
    }

    fn palette(self) -> [[f32; 4]; 3] {
        match self {
            Self::Movie => theme::USB_MOVIE_CARD,
            Self::Series => theme::USB_SERIES_CARD,
            Self::Documentary => theme::USB_DOCUMENTARY_CARD,
        }
    }

    fn illustration(self) -> &'static str {
        match self {
            Self::Movie => include_str!("../../../../assets/cineusb/card-movie.svg"),
            Self::Series => include_str!("../../../../assets/cineusb/card-series.svg"),
            Self::Documentary => include_str!("../../../../assets/cineusb/card-documentary.svg"),
        }
    }

    fn symbol(self) -> icons::Svg<'static> {
        icons::Svg::new(match self {
            Self::Movie => r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="none" stroke="white" stroke-width="2" stroke-linejoin="round" d="M3 11h18v9H3zm0 0L2 6l17-4 1 5z"/></svg>"#,
            Self::Series => r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="none" stroke="white" stroke-width="2" stroke-linejoin="round" d="m8 2 4 4 4-4M3 6h18v13H3zm5 16h8"/></svg>"#,
            Self::Documentary => r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="none" stroke="white" stroke-width="2" d="M4 18C2 10 8 4 21 3c0 11-5 17-14 16M3 22 15 10"/></svg>"#,
        })
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct Formats {
    pub resolution: Option<&'static str>,
    pub video: Option<&'static str>,
    pub audio: Option<&'static str>,
    pub channels: Option<u8>,
    pub mixed: bool,
}

impl Formats {
    pub(super) fn file(details: &LocalVideoDetails, spanish: bool) -> Self {
        let audio = details.audio[usize::from(spanish)];
        Self {
            resolution: details.resolution.and_then(|(w, h)| MovieQuality::of(w, h)).map(MovieQuality::label),
            video: details.range.map(|range| match range {
                VideoRange::DolbyVision | VideoRange::DolbyVisionHdr10 => "Dolby Vision",
                VideoRange::Hdr10Plus => "HDR10+",
                VideoRange::Hdr10 => "HDR10",
                VideoRange::Hdr => "HDR",
                VideoRange::Sdr => "SDR",
            }),
            audio: audio.map(|format| format.label),
            channels: audio.and_then(|format| format.channels),
            mixed: false,
        }
    }
}

pub(super) fn file_progress(record: u64, seconds: Option<u64>) -> (f32, Option<u64>) {
    if record == WATCHED { return (1.0, None); }
    let (position, duration) = unpack_progress(record);
    let duration = if duration > 0 { duration } else { seconds.unwrap_or_default().saturating_mul(1_000_000_000) };
    if position == 0 || duration == 0 { return (0.0, None); }
    let fraction = (position as f64 / duration as f64).clamp(0.0, 1.0) as f32;
    (fraction, Some(duration.saturating_sub(position).div_ceil(1_000_000_000)))
}

pub(super) enum Readout {
    File(Option<u64>),
    Series(super::series::Counts),
    Resume { episode: Option<(u32, u32)>, remaining: Option<u64> },
    Episode { number: Option<(u32, u32)>, duration: Option<u64> },
}

impl Readout {
    fn labels(&self, category: Category) -> (String, String) {
        let kind = category.label();
        match *self {
            Self::File(duration) => match duration {
                Some(seconds) => (format!("{kind} · {}", video_duration(seconds)),
                    format!("{kind} · {}", super::card_duration(seconds, 0.0))),
                None => (kind.into(), kind.into()),
            },
            Self::Series(counts) => {
                let season = if counts.seasons == 1 { "temporada" } else { tr("temporadas", "temporades") };
                let episode = if counts.episodes == 1 { tr("capítulo", "capítol") } else { tr("capítulos", "capítols") };
                (format!("{kind} · {} {season} · {} {episode}", counts.seasons, counts.episodes),
                    format!("{kind}·{}T/{}c", counts.seasons, counts.episodes))
            }
            Self::Resume { episode, remaining } => {
                let mut text = kind.to_owned();
                if let Some((s, e)) = episode { text.push_str(&format!(" · {s}×{e:02}")); }
                if let Some(seconds) = remaining {
                    text.push_str(&format!(" · {} {}", tr("Quedan", "Queden"), video_duration(seconds)));
                }
                (text.clone(), text)
            }
            Self::Episode { number, duration } => {
                let label = number.map_or_else(|| tr("Episodio", "Episodi").into(), |(s, e)| format!("{s}×{e:02}"));
                match duration {
                    Some(seconds) => (format!("{label} · {}", video_duration(seconds)),
                        format!("{label} · {}", super::card_duration(seconds, 0.0))),
                    None => (label.clone(), label),
                }
            }
        }
    }
}

pub(super) struct Card<'a> {
    pub title: &'a str,
    pub category: Category,
    pub formats: Formats,
    pub readout: Readout,
    pub fraction: f32,
    pub watched: bool,
    pub favorite: bool,
}

#[derive(Default)]
pub(super) struct Art {
    logos: RefCell<FormatLogos>,
    illustrations: RefCell<[Option<u32>; 3]>,
}

impl Drop for Art {
    fn drop(&mut self) {
        for texture in self.illustrations.get_mut().iter().flatten().copied().filter(|id| *id != 0) {
            gfx::delete_tex(texture);
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Frame {
    pub rect: Rect,
    pub radius: f32,
    pub lift: f32,
    pub accent: [f32; 4],
}

struct Layout {
    title: Rect,
    title_size: i32,
    leading: f32,
    formats_w: f32,
    art: Rect,
    progress: Rect,
    footer: Rect,
    compact: bool,
}

impl Layout {
    fn new(rect: Rect, wide: bool, episode: bool, favorite: bool) -> Self {
        let compact = !wide && rect.w < 195.0;
        let inset = if wide { 22.0 } else { (rect.w * 0.057).round() };
        let x = rect.x + inset;
        let width = rect.w - inset * 2.0;
        let footer_y = rect.y + rect.h - if wide { 48.0 } else { 42.0 };
        let title_size = if wide { theme::size::HEADLINE } else if compact { theme::size::MICRO } else { theme::size::LABEL };
        let leading = title_size as f32 * 1.08;
        let title_y = rect.y + inset;
        let art_top = if wide { rect.y + 14.0 } else {
            title_y + leading * 2.0 + if episode { 5.0 } else { 47.0 }
        };
        let art_bottom = footer_y - 10.0;
        let art_width = if wide { rect.w * 0.33 } else { width };
        let picture_width = art_width.min((art_bottom - art_top) * 200.0 / 172.0);
        let picture_h = picture_width * 172.0 / 200.0;
        let art_x = if wide { rect.x + rect.w - inset - picture_width } else { x + (width - picture_width) / 2.0 };
        let formats_w = if wide { art_x - x - 12.0 } else { width };
        let title_w = formats_w - if favorite { 23.0 } else { 0.0 };
        Self {
            title: Rect::new(x, title_y, title_w, 2.0 * leading),
            title_size, leading, formats_w, compact,
            art: Rect::new(art_x, (art_top + art_bottom - picture_h) / 2.0, picture_width, picture_h),
            progress: Rect::new(x, footer_y - 4.0, width, if wide { 5.0 } else { 4.0 }),
            footer: Rect::new(x, footer_y + 6.0, width, 27.0),
        }
    }
}

impl Art {
    pub(super) fn draw(&self, p: Painter, frame: Frame, card: Card<'_>) {
        let Frame { rect, radius, lift, accent } = frame;
        let [top, bottom, color] = card.category.palette();
        let wide = matches!(card.readout, Readout::Resume { .. });
        let episode = matches!(card.readout, Readout::Episode { .. });
        let layout = Layout::new(rect, wide, episode, card.favorite);

        p.shadow_outside(rect, radius, 16.0, 6.0, theme::CARD_SHADOW);
        if lift > 0.0 {
            p.shadow_outside(rect, radius, 23.0, 4.0, theme::with_a(color, lift * 0.46));
        }
        p.rect_sheened(rect, radius, top, bottom);
        p.rring(rect, radius, 1.0, theme::with_a(color, 0.30));

        self.draw_illustration(p, card.category, layout.art);
        let title = TextView::new(card.title, layout.title_size, theme::TEXT_PRIMARY).with_measure(&TtfMeasure)
            .bold().break_long_words().max_lines(2).leading(layout.leading);
        title.draw(p, layout.title);
        if card.favorite {
            super::draw_glyph(p, super::Glyph::Star(true),
                Rect::new(layout.title.x + layout.title.w + 4.0, layout.title.y + 1.0, 18.0, 18.0), theme::USB_FAVORITES);
        }
        if !episode {
            let y = layout.title.y + title.measure_h(layout.title.w).min(layout.title.h) + 6.0;
            self.draw_formats(p, Rect::new(layout.title.x, y, layout.formats_w, if wide { 26.0 } else { 22.0 }),
                card.formats, layout.compact);
        }

        let bar = layout.progress;
        p.rrect(bar, bar.h * 0.5, bar.h * 0.5, theme::with_a(theme::TEXT_PRIMARY, 0.19));
        if card.fraction > 0.0 {
            let fill = Rect::new(bar.x, bar.y, bar.w * card.fraction, bar.h);
            p.rrect(fill, bar.h * 0.5, bar.h * 0.5, if card.watched { theme::USB_WATCHED_EDGE } else { color });
        }
        Self::draw_footer(p, layout.footer, &card, color, wide);
        if lift > 0.0 {
            p.rring(rect, radius, theme::CONTROL_FOCUS_RING_W, theme::with_a(accent, lift));
        }
    }

    fn draw_illustration(&self, p: Painter, category: Category, rect: Rect) {
        if p.is_recording() { return; }
        let texture = *self.illustrations.borrow_mut()[category as usize].get_or_insert_with(|| {
            svg::rasterize(category.illustration(), 300, 258)
                .map_or(0, |pixels| gfx::upload_rgba(0, 300, 258, pixels.as_ptr()))
        });
        if texture != 0 { p.tex(texture, rect, 0.0, theme::TINT_WHITE); }
    }

    fn draw_formats(&self, p: Painter, rect: Rect, formats: Formats, compact: bool) {
        let mixed = formats.mixed.then(|| tr("Mixtos", "Mixtos"));
        let channels = formats.channels.map(|count| format!("{count} ch"));
        let labels = [formats.resolution, formats.video, formats.audio, mixed, channels.as_deref()];
        let mut logos = self.logos.borrow_mut();
        if !p.is_recording() {
            for label in [formats.video, formats.audio].into_iter().flatten() { logos.prepare(label); }
        }
        let logos = labels.map(|label| label.and_then(|label| logos.get(label)));
        let gap = if compact { 5.0 } else { 7.0 };
        let size = if compact { theme::size::USB_CARD_META } else { theme::size::DIAGNOSTIC };
        let height = if compact { 13.0 } else { 17.0 };
        let width = |index: usize, size, height| labels[index].map_or(0.0, |label| mark_width(label, logos[index], size, height));
        let total = |count: usize, size, height| {
            let widths: [f32; 5] = std::array::from_fn(|index| width(index, size, height));
            format_row_width(&widths[..count], gap)
        };
        let mandatory = labels.len() - 1;
        let (size, height) = if total(mandatory, size, height) > rect.w {
            (theme::size::USB_CARD_META, 11.0)
        } else { (size, height) };
        let mut row = Flow::new(rect.x, gap);
        let mut y = rect.y;
        for (index, (label, logo)) in labels.iter().zip(&logos).enumerate() {
            let Some(label) = *label else { continue };
            let available = rect.x + rect.w - row.next_x();
            let mut font = size;
            let mut mark = width(index, size, height);
            if mark > available {
                // Channel count is optional. Keep primary formats legible by
                // wrapping whole marks when the card cannot fit them in one band.
                if index == mandatory { break; }
                row = Flow::new(rect.x, gap);
                y += 19.0;
            }
            if mark > rect.w && logo.is_none() {
                font = theme::size::USB_CARD_META_TIGHT;
                mark = width(index, font, height);
            }
            let Some(span) = row.push(Item::new(mark.min(rect.w))) else { continue; };
            let slot = Rect::new(span.x, y, span.w, rect.h);
            if let Some(logo) = logo {
                logo.draw(p, Rect::new(slot.x, slot.y + (slot.h - height) / 2.0, slot.w, height), 1.0);
            } else {
                let run = TtfMeasure.fit_line(label, slot.w, font, true);
                Label::new(run.as_ptr(), font, theme::TEXT_SECONDARY).bold().draw(p, slot);
            }
        }
    }

    fn draw_footer(p: Painter, footer: Rect, card: &Card<'_>, color: [f32; 4], wide: bool) {
        let (full, short) = card.readout.labels(card.category);
        let ink = if card.watched { theme::USB_WATCHED_EDGE } else { color };
        let smallest = theme::size::USB_CARD_COUNT_TIGHT;
        let icon_w = if TtfMeasure.width_str(&short, smallest, false) > footer.w - 23.0 { 0.0 } else { 23.0 };
        let symbol_x = if icon_w > 0.0 { footer.x } else { footer.x + footer.w - 17.0 };
        let symbol_y = if icon_w > 0.0 { footer.y + 4.0 } else { footer.y - 32.0 };
        let symbol = Rect::new(symbol_x, symbol_y, 17.0, 17.0);
        if card.watched {
            icons::draw(p, Icon::Check, symbol, ink);
        } else if icon_w > 0.0 {
            icons::draw_svg(p, card.category.symbol(), symbol, ink);
        }
        let width = footer.w - icon_w;
        let (label, size) = footer_text(&TtfMeasure, &full, &short, width, wide);
        let run = TtfMeasure.fit_line(label, width, size, false);
        Label::new(run.as_ptr(), size, theme::TEXT_PRIMARY)
            .draw(p, Rect::new(footer.x + icon_w, footer.y, width, footer.h));
    }
}

fn footer_text<'a>(measure: &dyn Measure, full: &'a str, short: &'a str, width: f32, wide: bool) -> (&'a str, i32) {
    let tight = theme::size::USB_CARD_META_TIGHT;
    let smallest = theme::size::USB_CARD_COUNT_TIGHT;
    let candidates = [(full, theme::size::CAPTION), (full, theme::size::DIAGNOSTIC),
        (full, theme::size::USB_CARD_META), (short, theme::size::USB_CARD_META), (short, tight), (short, smallest)];
    candidates.into_iter().skip(usize::from(!wide))
        .find(|(text, size)| measure.width_str(text, *size, false) <= width)
        .unwrap_or((short, smallest))
}

fn format_row_width(widths: &[f32], gap: f32) -> f32 {
    let present = widths.iter().filter(|&&width| width > 0.0);
    present.clone().sum::<f32>() + present.count().saturating_sub(1) as f32 * gap
}

fn mark_width(label: &str, logo: Option<&FormatLogo>, size: i32, logo_height: f32) -> f32 {
    logo.map_or_else(|| TtfMeasure.width_str(label, size, true), |logo| logo.width(logo_height))
}

#[cfg(test)]
mod usb_browser_tests {
    use super::*;

    #[test]
    fn layout_keeps_a_two_line_title_clear_of_formats_art_and_footer() {
        for preset in 0..3 {
            let grid = super::super::grid_geometry(12, preset, true);
            for scale in [1.0, 1.045] {
                let rect = Rect::new(80.0, 204.0, grid.w * scale, grid.h * scale);
                let layout = Layout::new(rect, false, false, true);
                assert!(layout.title.y + layout.title.h + 46.9 <= layout.art.y);
                assert!(layout.art.y + layout.art.h < layout.progress.y);
                assert!(layout.progress.y + layout.progress.h < layout.footer.y);
                assert!(layout.footer.y + layout.footer.h < rect.y + rect.h);
            }
        }
        let wide = Layout::new(Rect::new(80.0, 600.0, 564.0, 210.0), true, false, true);
        assert!(wide.title.x + wide.formats_w < wide.art.x);
        assert!(wide.art.y + wide.art.h < wide.progress.y);
    }

    #[test]
    fn compact_footer_keeps_type_and_counts_in_one_line() {
        let grid = super::super::grid_geometry(12, 1, true);
        let layout = Layout::new(Rect::new(0.0, 0.0, grid.w, grid.h), false, false, false);
        let measure = plx_base::fontcov::advances::ShippedMeasure;
        let counts = super::super::series::Counts { seasons: 12, episodes: 302 };
        let (full, short) = Readout::Series(counts).labels(Category::Documentary);
        let (text, size) = footer_text(&measure, &full, &short, layout.footer.w, false);
        assert!(text.starts_with("Documental") && text.contains("12") && text.contains("302"));
        assert!(measure.width_str(text, size, false) <= layout.footer.w);
    }

    #[test]
    fn progress_recovers_duration_for_legacy_records_and_keeps_watched_complete() {
        assert_eq!(file_progress(0, Some(120)), (0.0, None));
        assert_eq!(file_progress(30, None), (0.0, None));
        assert_eq!(file_progress(30, Some(120)), (0.25, Some(90)));
        assert_eq!(file_progress(WATCHED, None), (1.0, None));
        let saved = super::super::pack_progress(60_000_000_000, Some(120_000_000_000));
        assert_eq!(file_progress(saved, Some(100)), (0.5, Some(60)));
    }
}
