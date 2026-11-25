//! One composition for USB media, with a wider arrangement for direct playback from Home.
use super::{MovieQuality, unpack_progress, video_duration, WATCHED};
use plx_gfx::{gfx, svg, text::TtfMeasure};
use plx_machine::machine::Measure;
use plx_play_feed::{LocalVideoDetails, VideoRange};
use plx_ui::{Painter, Rect, hstack::{Flow, Item}, icons::{self, Icon}, label::Label, text_view::TextView, theme};
use std::cell::RefCell;
use plx_platform::i18n::{self, Language};

fn local(language: Language, es: &'static str, ca: &'static str, en: &'static str) -> &'static str {
    match language {
        Language::Es => es,
        Language::Ca => ca,
        _ => en,
    }
}

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

    fn label(self, language: Language) -> &'static str {
        match self {
            Self::Movie => local(language, "Película", "Pel·lícula", "Movie"),
            Self::Series => local(language, "Serie", "Sèrie", "Series"),
            Self::Documentary => local(language, "Documental", "Documental", "Documentary"),
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

    pub(super) fn symbol(self) -> icons::Svg<'static> {
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
    Recent(Option<u64>),
    Series(super::series::Counts),
    Resume { episode: Option<(u32, u32)>, remaining: Option<u64> },
    Episode { number: Option<(u32, u32)>, duration: Option<u64> },
}

impl Readout {
    fn labels(&self, category: Category, language: Language, show_category: bool) -> (String, String) {
        let kind = category.label(language);
        let with_kind = |text: String| if show_category { format!("{kind} · {text}") } else { text };
        match *self {
            Self::File(duration) | Self::Recent(duration) => match duration {
                Some(seconds) => (with_kind(video_duration(seconds)),
                    with_kind(super::card_duration(seconds, 0.0))),
                None if show_category => (kind.into(), kind.into()),
                None => (String::new(), String::new()),
            },
            Self::Series(counts) => {
                let season = if counts.seasons == 1 {
                    local(language, "temporada", "temporada", "season")
                } else { local(language, "temporadas", "temporades", "seasons") };
                let episode = if counts.episodes == 1 {
                    local(language, "capítulo", "capítol", "episode")
                } else { local(language, "capítulos", "capítols", "episodes") };
                let text = with_kind(format!("{} {season} · {} {episode}", counts.seasons, counts.episodes));
                (text.clone(), text)
            }
            Self::Resume { remaining, .. } => {
                let text = remaining.map_or_else(
                    || if show_category { kind.to_owned() } else { String::new() },
                    |seconds| with_kind(format!("{} {}", local(language, "Quedan", "Queden", "Remaining"), video_duration(seconds))),
                );
                (text.clone(), text)
            }
            Self::Episode { duration, .. } => {
                let label = local(language, "Episodio", "Episodi", "Episode").to_owned();
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
    pub show_category: bool,
    pub formats: Formats,
    pub readout: Readout,
    pub fraction: f32,
    pub watched: bool,
    pub favorite: bool,
}

#[derive(Default)]
pub(super) struct Art {
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

const HOME_INSET: f32 = 22.0;
const FAVORITE_SPACE: f32 = 23.0;

fn new_badge(language: Language, measure: &dyn Measure) -> (&'static str, f32) {
    let text = local(language, "Nuevo", "Nou", "New");
    (text, measure.width_str(text, theme::size::USB_CARD_META, true) + 18.0)
}

pub(super) fn home_title_width(title: &str, favorite: bool, recent: bool, language: Language, measure: &dyn Measure) -> f32 {
    let reserved = if recent { new_badge(language, measure).1 + 5.0 } else { 0.0 };
    let favorite = if favorite { FAVORITE_SPACE } else { 0.0 };
    (measure.width_str(title, theme::size::HEADLINE, true) + HOME_INSET * 2.0 + favorite + reserved).ceil()
}

fn episode_header(mut title: Rect, title_w: f32, title_h: f32, badge_w: f32, badge_h: f32) -> (Rect, Rect) {
    let badge = if title_w + 8.0 + badge_w <= title.w {
        title.w -= badge_w + 8.0;
        Rect::new(title.x + title_w.min(title.w) + 8.0, title.y, badge_w, badge_h)
    } else {
        Rect::new(title.x, title.y + title_h + 3.0, badge_w, badge_h)
    };
    (title, badge)
}

impl Layout {
    fn art_below_formats(&self, formats_bottom: f32) -> Rect {
        if formats_bottom + 5.0 <= self.art.y { return self.art; }
        let top = formats_bottom + 5.0;
        let available_h = (self.progress.y - 6.0 - top).max(0.0);
        let height = self.art.h.min(available_h);
        let width = height * 200.0 / 172.0;
        Rect::new(self.art.x + (self.art.w - width) / 2.0,
            top + (available_h - height) / 2.0, width, height)
    }

    fn new(rect: Rect, wide: bool, episode: bool, favorite: bool, series: bool, extra_footer: f32) -> Self {
        let compact = !wide && rect.w < 195.0;
        let inset = if wide { HOME_INSET } else { (rect.w * 0.057).round() };
        let x = rect.x + inset;
        let width = rect.w - inset * 2.0;
        let footer_h = (if series { 54.0 } else { 28.0 }) + extra_footer;
        let footer_y = rect.y + rect.h - footer_h - if wide { 20.0 } else { 15.0 };
        let title_size = if wide { theme::size::HEADLINE } else if compact { theme::size::MICRO } else { theme::size::LABEL };
        let leading = title_size as f32 * 1.08;
        let title_y = rect.y + inset;
        let art_top = if wide { title_y + leading + 5.0 } else {
            title_y + leading * 2.0 + if episode { 38.0 } else { 53.0 }
        };
        let art_bottom = footer_y - 10.0;
        let art_width = if wide { rect.w * 0.33 } else { width };
        let picture_width = art_width.min((art_bottom - art_top) * 200.0 / 172.0);
        let picture_h = picture_width * 172.0 / 200.0;
        let art_x = if wide { rect.x + rect.w - inset - picture_width } else { x + (width - picture_width) / 2.0 };
        let formats_w = if wide { art_x - x - 12.0 } else { width };
        let title_w = width - if favorite { FAVORITE_SPACE } else { 0.0 };
        Self {
            title: Rect::new(x, title_y, title_w, if wide { leading } else { 2.0 * leading }),
            title_size, leading, formats_w, compact,
            art: Rect::new(art_x, (art_top + art_bottom - picture_h) / 2.0, picture_width, picture_h),
            progress: Rect::new(x, footer_y - 4.0, width, if wide { 5.0 } else { 4.0 }),
            footer: Rect::new(x, footer_y + 6.0, width, footer_h),
        }
    }
}

impl Art {
    pub(super) fn draw(&self, p: Painter, frame: Frame, card: Card<'_>) {
        let Frame { rect, radius, lift, accent } = frame;
        let [top, bottom, color] = card.category.palette();
        let recent = matches!(card.readout, Readout::Recent(_));
        let wide = recent || matches!(card.readout, Readout::Resume { .. });
        let episode = matches!(card.readout, Readout::Episode { .. });
        let series = matches!(card.readout, Readout::Series(_));
        let mut layout = Layout::new(rect, wide, episode, card.favorite, series, 0.0);
        let language = i18n::current().language();
        let extra_footer = if card.watched && series {
            30.0
        } else if card.watched && watched_seconds(&card.readout).is_some_and(|seconds| {
            let badge = watched_badge_width(watched_label(&card, language), &TtfMeasure);
            watched_duration(&TtfMeasure, &video_duration(seconds), &super::card_duration(seconds, 0.0),
                layout.footer.w - badge - 8.0).is_none()
        }) {
            26.0
        } else { 0.0 };
        if extra_footer > 0.0 {
            layout = Layout::new(rect, wide, episode, card.favorite, series, extra_footer);
        }
        let number = match card.readout {
            Readout::Episode { number, .. } | Readout::Resume { episode: number, .. } => number,
            _ => None,
        }.map(|(s, e)| format!("{s}×{e:02}"));

        p.shadow_outside(rect, radius, 16.0, 6.0, theme::CARD_SHADOW);
        if lift > 0.0 {
            p.shadow_outside(rect, radius, 23.0, 4.0, theme::with_a(color, lift * 0.46));
        }
        p.rect_sheened(rect, radius, top, bottom);
        p.rring(rect, radius, 1.0, theme::with_a(color, 0.30));

        let title = TextView::new(card.title, layout.title_size, theme::TEXT_PRIMARY).with_measure(&TtfMeasure)
            .bold().break_long_words().max_lines(2).leading(layout.leading);
        let mut title_rect = layout.title;
        let new_badge = recent.then(|| new_badge(language, &TtfMeasure));
        if let Some((_, width)) = new_badge { title_rect.w -= width + 5.0; }
        let mut header_h = if wide { title_rect.h } else { title.measure_h(title_rect.w).min(title_rect.h) };
        if let Some(number) = number.as_deref() {
            let size = theme::size::MICRO;
            let badge_w = TtfMeasure.width_str(number, size, true) + 14.0;
            let title_w = TtfMeasure.width_str(card.title, layout.title_size, true);
            let (text, badge) = episode_header(title_rect, title_w, header_h, badge_w,
                layout.leading.max(26.0));
            title_rect = text;
            Self::capsule(p, badge, color);
            let run = TtfMeasure.fit_line(number, badge_w - 12.0, size, true);
            Label::new(run.as_ptr(), size, theme::TEXT_PRIMARY).bold().h(plx_ui::label::HAlign::Center).draw(p, badge);
            header_h = (if wide { title_rect.h } else { title.measure_h(title_rect.w).min(title_rect.h) })
                .max(badge.y + badge.h - title_rect.y);
        }
        if wide {
            let run = TtfMeasure.fit_line(card.title, title_rect.w, layout.title_size, true);
            Label::new(run.as_ptr(), layout.title_size, theme::TEXT_PRIMARY).bold().draw(p, title_rect);
        } else {
            title.draw(p, title_rect);
        }
        if card.favorite {
            super::draw_glyph(p, super::Glyph::Star(true),
                Rect::new(layout.title.x + layout.title.w + 4.0, layout.title.y + 1.0, 18.0, 18.0), theme::USB_FAVORITES);
        }
        let mut art = layout.art;
        if !episode {
            let y = layout.title.y + header_h + 5.0;
            let format_bottom = Self::draw_formats(p, Rect::new(layout.title.x, y, layout.formats_w, if wide { 25.0 } else { 23.0 }),
                card.formats, layout.compact, wide, color);
            if !wide { art = layout.art_below_formats(format_bottom); }
        }
        if art.h > 0.0 { self.draw_illustration(p, card.category, art); }
        if let Some((text, width)) = new_badge {
            let size = theme::size::USB_CARD_META;
            let badge = Rect::new(rect.x + rect.w - width - 15.0, rect.y + 12.0, width, 25.0);
            Self::capsule(p, badge, accent);
            let run = TtfMeasure.fit_line(text, width - 12.0, size, true);
            Label::new(run.as_ptr(), size, theme::TEXT_PRIMARY).bold().h(plx_ui::label::HAlign::Center).draw(p, badge);
        }

        let bar = layout.progress;
        if !recent && card.watched {
            p.rrect(Rect::new(bar.x, bar.y + bar.h * 0.5, bar.w, 1.0), 0.5, 0.5, theme::with_a(color, 0.38));
        } else if !recent {
            p.rrect(bar, bar.h * 0.5, bar.h * 0.5, theme::with_a(theme::TEXT_PRIMARY, 0.19));
        }
        if !recent && !card.watched && card.fraction > 0.0 {
            let fill = Rect::new(bar.x, bar.y, bar.w * card.fraction, bar.h);
            p.rrect(fill, bar.h * 0.5, bar.h * 0.5, color);
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

    fn capsule(p: Painter, rect: Rect, color: [f32; 4]) {
        p.rrect(rect, 7.0, 7.0, theme::with_a(color, 0.11));
        p.rring(rect, 7.0, 1.0, theme::with_a(color, 0.40));
    }

    fn draw_formats(p: Painter, rect: Rect, formats: Formats, compact: bool, wide: bool, color: [f32; 4]) -> f32 {
        let mixed = formats.mixed.then(|| local(i18n::current().language(), "Mixtos", "Mixts", "Mixed"));
        let labels = [formats.resolution, formats.video, formats.audio, mixed];
        let gap = if compact { 5.0 } else { 7.0 };
        let size = if compact { theme::size::USB_CARD_META } else { theme::size::DIAGNOSTIC };
        let (labels, widths, size) = format_labels(labels, rect.w, gap, size, if wide { 1 } else { 2 }, &TtfMeasure);
        let mut row = Flow::new(rect.x, gap);
        let mut y = rect.y;
        for (label, mark) in labels.into_iter().zip(widths) {
            let Some(label) = label else { continue };
            let mark = mark.min(rect.w);
            if row.next_x() > rect.x && row.next_x() + mark > rect.x + rect.w {
                row = Flow::new(rect.x, gap);
                y += rect.h + 2.0;
            }
            let Some(span) = row.push(Item::new(mark)) else { continue; };
            let slot = Rect::new(span.x, y, span.w, rect.h);
            Self::capsule(p, slot, color);
            let slot = Rect::new(slot.x + 5.0, slot.y, slot.w - 10.0, slot.h);
            let run = TtfMeasure.fit_line(label, slot.w, size, true);
            Label::new(run.as_ptr(), size, theme::TEXT_PRIMARY).bold().draw(p, slot);
        }
        if row.next_x() > rect.x { y + rect.h } else { rect.y }
    }

    fn draw_footer(p: Painter, footer: Rect, card: &Card<'_>, color: [f32; 4], wide: bool) {
        let language = i18n::current().language();
        let series = matches!(card.readout, Readout::Series(_));
        let (full, short) = card.readout.labels(card.category, language, card.show_category && !series);
        if series {
            let counts = full.splitn(2, " · ");
            for (index, line) in counts.enumerate() {
                let size = count_size(line, &TtfMeasure, footer.w);
                let run = TtfMeasure.fit_line(line, footer.w, size, true);
                Label::new(run.as_ptr(), size, theme::TEXT_PRIMARY).bold()
                    .draw(p, Rect::new(footer.x, footer.y + 26.0 * index as f32, footer.w, 26.0));
            }
        }
        if card.watched {
            let badge_y = footer.y + if series { 54.0 } else { 0.0 };
            let badge = draw_watched_badge(p, Rect::new(footer.x, badge_y, footer.w, 26.0),
                watched_label(card, language));
            if let Some(seconds) = watched_seconds(&card.readout) {
                let width = footer.w - badge - 8.0;
                let full = video_duration(seconds);
                let short = super::card_duration(seconds, 0.0);
                let (text, size, slot) = if let Some((text, size)) = watched_duration(&TtfMeasure, &full, &short, width) {
                    (text, size, Rect::new(footer.x + badge + 8.0, badge_y, width, 26.0))
                } else {
                    let (text, size) = watched_duration(&TtfMeasure, &full, &short, footer.w)
                        .unwrap_or((&short, theme::size::USB_CARD_META_TIGHT));
                    (text, size, Rect::new(footer.x, badge_y + 26.0, footer.w, 26.0))
                };
                let run = TtfMeasure.fit_line(text, slot.w, size, false);
                Label::new(run.as_ptr(), size, theme::TEXT_PRIMARY).h(plx_ui::label::HAlign::Right)
                    .draw(p, slot);
            }
            return;
        }
        if series { return; }
        let smallest = theme::size::USB_CARD_COUNT_TIGHT;
        let icon_w = if short.is_empty() || TtfMeasure.width_str(&short, smallest, false) > footer.w - 23.0 { 0.0 } else { 23.0 };
        let symbol_x = if icon_w > 0.0 { footer.x } else { footer.x + footer.w - 17.0 };
        let symbol_y = if icon_w > 0.0 { footer.y + 4.0 } else { footer.y - 32.0 };
        let symbol = Rect::new(symbol_x, symbol_y, 17.0, 17.0);
        if icon_w > 0.0 {
            icons::draw_svg(p, card.category.symbol(), symbol, color);
        }
        if full.is_empty() { return; }
        let width = footer.w - icon_w;
        let (label, size) = footer_text(&TtfMeasure, &full, &short, width, wide);
        let run = TtfMeasure.fit_line(label, width, size, false);
        Label::new(run.as_ptr(), size, theme::TEXT_PRIMARY)
            .draw(p, Rect::new(footer.x + icon_w, footer.y, width, footer.h));
    }
}

fn count_size(text: &str, measure: &dyn Measure, width: f32) -> i32 {
    [theme::size::DIAGNOSTIC, theme::size::USB_CARD_META].into_iter()
        .find(|size| measure.width_str(text, *size, true) <= width)
        .unwrap_or(theme::size::USB_CARD_META_TIGHT)
}

fn watched_label(card: &Card<'_>, language: Language) -> &'static str {
    if matches!(card.readout, Readout::Episode { .. }) || card.category == Category::Documentary {
        local(language, "Visto", "Vist", "Watched")
    } else {
        local(language, "Vista", "Vista", "Watched")
    }
}

fn watched_badge_width(text: &str, measure: &dyn Measure) -> f32 {
    33.0 + measure.width_str(text, theme::size::USB_CARD_META, true)
}

fn draw_watched_badge(p: Painter, slot: Rect, text: &str) -> f32 {
    let size = theme::size::USB_CARD_META;
    let width = watched_badge_width(text, &TtfMeasure);
    let badge = Rect::new(slot.x, slot.y, width, slot.h);
    p.rrect(badge, 13.0, 13.0, theme::USB_WATCHED_TOP);
    icons::draw(p, Icon::Check, Rect::new(slot.x + 7.0, slot.y + 5.0, 16.0, 16.0), theme::USB_WATCHED_EDGE);
    let run = TtfMeasure.fit_line(text, width - 33.0, size, true);
    Label::new(run.as_ptr(), size, theme::TEXT_PRIMARY).bold()
        .draw(p, Rect::new(slot.x + 25.0, slot.y, width - 33.0, slot.h));
    width
}

fn watched_seconds(readout: &Readout) -> Option<u64> {
    match *readout {
        Readout::File(duration) | Readout::Episode { duration, .. } => duration,
        Readout::Resume { remaining, .. } => remaining,
        Readout::Series(_) | Readout::Recent(_) => None,
    }
}

fn watched_duration<'a>(measure: &dyn Measure, full: &'a str, short: &'a str, width: f32) -> Option<(&'a str, i32)> {
    [(full, theme::size::DIAGNOSTIC), (short, theme::size::DIAGNOSTIC), (short, theme::size::USB_CARD_META)]
        .into_iter().find(|(text, size)| measure.width_str(text, *size, false) <= width)
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

fn format_rows(widths: impl Iterator<Item = f32>, gap: f32, available: f32) -> usize {
    let mut rows = 0;
    let mut x = 0.0;
    for width in widths.filter(|&width| width > 0.0) {
        if rows == 0 || x + gap + width > available {
            rows += 1;
            x = width.min(available);
        } else {
            x += gap + width;
        }
    }
    rows
}

fn format_labels(full: [Option<&'static str>; 4], available: f32, gap: f32, size: i32, preferred_rows: usize,
    measure: &dyn Measure) -> ([Option<&'static str>; 4], [f32; 4], i32) {
    let widths = |labels: [Option<&str>; 4], size| {
        labels.map(|label| label.map_or(0.0, |text| measure.width_str(text, size, true).ceil() + 10.0))
    };
    let fits = |widths: [f32; 4]| widths.iter().all(|&width| width <= available)
        && format_rows(widths.into_iter(), gap, available) <= preferred_rows;
    let full_widths = widths(full, size);
    if fits(full_widths) {
        return (full, full_widths, size);
    }
    let short = full.map(|label| label.map(|text| match text {
        "Dolby Vision" => "DV",
        "Dolby Digital Plus" => "DD+",
        "Dolby Digital" => "DD",
        "Dolby Atmos" => "Atmos",
        "Dolby TrueHD" => "TrueHD",
        "DTS-HD MA" => "DTS MA",
        "DTS-HD HRA" => "DTS HRA",
        _ => text,
    }));
    let short_widths = widths(short, size);
    for index in [2, 1] {
        if full[index] == short[index] { continue; }
        let mut labels = full;
        let mut widths = full_widths;
        labels[index] = short[index];
        widths[index] = short_widths[index];
        if fits(widths) { return (labels, widths, size); }
    }
    if size > theme::size::USB_CARD_META && !fits(short_widths) {
        let smaller = theme::size::USB_CARD_META;
        return (short, widths(short, smaller), smaller);
    }
    (short, short_widths, size)
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
                let layout = Layout::new(rect, false, false, true, false, 0.0);
                assert!(layout.title.y + layout.title.h + 52.9 <= layout.art.y);
                assert!(layout.art.y + layout.art.h < layout.progress.y);
                assert!(layout.progress.y + layout.progress.h < layout.footer.y);
                assert!(layout.footer.y + layout.footer.h < rect.y + rect.h);
                let three_rows = layout.title.y + layout.title.h + 5.0 + 3.0 * 23.0 + 4.0;
                let art = layout.art_below_formats(three_rows);
                assert!(art.y >= three_rows + 5.0 && art.y + art.h < layout.progress.y);
            }
        }
        let wide = Layout::new(Rect::new(80.0, 600.0, 564.0, 210.0), true, false, true, false, 0.0);
        assert!(wide.title.x + wide.formats_w < wide.art.x);
        assert!(wide.art.y + wide.art.h < wide.progress.y);
    }

    #[test]
    fn footer_counts_and_watched_badge_fit_all_card_sizes() {
        let measure = plx_base::fontcov::advances::ShippedMeasure;
        let counts = super::super::series::Counts { seasons: 12, episodes: 302 };
        for preset in 0..3 {
            let grid = super::super::grid_geometry(12, preset, true);
            let rect = Rect::new(0.0, 0.0, grid.w, grid.h);
            for watched in [false, true] {
                let layout = Layout::new(rect, false, false, false, true, if watched { 30.0 } else { 0.0 });
                for (language, expected) in [
                    (Language::Es, "12 temporadas · 302 capítulos"),
                    (Language::Ca, "12 temporades · 302 capítols"),
                    (Language::En, "12 seasons · 302 episodes"),
                ] {
                    let (label, _) = Readout::Series(counts).labels(Category::Series, language, false);
                    assert_eq!(label, expected);
                    assert_eq!(label.split(" · ").count(), 2);
                    for line in label.split(" · ") {
                        let size = count_size(line, &measure, layout.footer.w);
                        assert!(measure.width_str(line, size, true) <= layout.footer.w);
                    }
                }
                assert!(layout.footer.h >= if watched { 80.0 } else { 52.0 });
                assert!(layout.art.y + layout.art.h < layout.progress.y);
            }
            let movie = Layout::new(rect, false, false, false, false, 0.0);
            for badge in ["Vista", "Vist", "Watched"] {
                let width = movie.footer.w - watched_badge_width(badge, &measure) - 8.0;
                if let Some((duration, size)) = watched_duration(&measure, "2 h 52 min", "2h52", width) {
                    assert!(measure.width_str(duration, size, false) <= width);
                } else {
                    let stacked = Layout::new(rect, false, false, false, false, 26.0);
                    let (duration, size) = watched_duration(&measure, "2 h 52 min", "2h52", stacked.footer.w).unwrap();
                    assert!(measure.width_str(duration, size, false) <= stacked.footer.w);
                    assert!(stacked.footer.h >= 52.0);
                }
            }
        }
    }

    #[test]
    fn compact_format_capsules_pack_without_overlapping_the_art() {
        assert_eq!(format_rows([65.0, 84.0, 108.0].into_iter(), 5.0, 139.0), 3);
        assert_eq!(format_rows([56.0, 71.0, 96.0].into_iter(), 5.0, 139.0), 2);
    }

    #[test]
    fn hdr_and_dolby_badges_fit_without_miniature_logos() {
        let measure = plx_base::fontcov::advances::ShippedMeasure;
        let size = theme::size::DIAGNOSTIC;
        let only_audio_short = ["4K UHD", "Dolby Vision", "DD+"];
        let width = only_audio_short.into_iter().map(|text| measure.width_str(text, size, true).ceil() + 10.0)
            .sum::<f32>() + 14.0;
        let (names, _, _) = format_labels([Some("4K UHD"), Some("Dolby Vision"), Some("Dolby Digital Plus"), None],
            width, 7.0, size, 1, &measure);
        assert_eq!(names, [Some("4K UHD"), Some("Dolby Vision"), Some("DD+"), None]);
        for video in ["HDR", "HDR10+", "Dolby Vision"] {
            let labels = [Some("4K UHD"), Some(video), Some("Dolby Digital Plus"), None];
            let (wide_names, _, _) = format_labels(labels, 475.0, 7.0, theme::size::DIAGNOSTIC, 1, &measure);
            assert_eq!(wide_names, labels);
            let (names, widths, _) = format_labels(labels, 220.0, 7.0, theme::size::DIAGNOSTIC, 1, &measure);
            assert_eq!(names[1], Some(if video == "Dolby Vision" { "DV" } else { video }));
            assert_eq!(names[2], Some("DD+"));
            assert_eq!(format_rows(widths.into_iter(), 7.0, 220.0), 1);
            for preset in 0..3 {
                let grid = super::super::grid_geometry(12, preset, true);
                let layout = Layout::new(Rect::new(0.0, 0.0, grid.w, grid.h), false, false, false, false, 0.0);
                let gap = if layout.compact { 5.0 } else { 7.0 };
                let size = if layout.compact { theme::size::USB_CARD_META } else { theme::size::DIAGNOSTIC };
                let (names, widths, size) = format_labels(labels, layout.formats_w, gap, size, 2, &measure);
                assert_eq!(names.iter().flatten().count(), 3);
                assert!(widths.iter().all(|&width| width <= layout.formats_w));
                assert!(size >= theme::size::USB_CARD_META);
                assert!(format_rows(widths.into_iter(), gap, layout.formats_w) <= 2);
            }
        }
    }

    #[test]
    fn episode_capsule_stays_in_the_header_with_short_and_long_titles() {
        for preset in 0..3 {
            let grid = super::super::grid_geometry(12, preset, true);
            let layout = Layout::new(Rect::new(0.0, 0.0, grid.w, grid.h), false, true, false, false, 0.0);
            let (short, next) = episode_header(layout.title, 50.0, layout.leading, 63.0, 27.0);
            assert!(next.x > short.x + 50.0 && next.y == short.y);
            let (long, below) = episode_header(layout.title, 400.0, layout.title.h, 63.0, 27.0);
            assert!(below.y > long.y + long.h && below.y + below.h <= layout.art.y);
        }
        let layout = Layout::new(Rect::new(0.0, 0.0, 564.0, 190.0), true, false, true, false, 0.0);
        let (text, badge) = episode_header(layout.title, 600.0, layout.title.h, 63.0, 30.0);
        assert_eq!(text.w, layout.title.w);
        assert!(badge.y >= text.y + text.h && badge.x + badge.w < layout.art.x);
        for episode in 1..=3 {
            let (footer, _) = Readout::Episode { number: Some((1, episode)), duration: Some(3120) }
                .labels(Category::Documentary, Language::En, false);
            assert_eq!(footer, "Episode · 52 min");
        }
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
