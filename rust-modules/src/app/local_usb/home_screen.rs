//! Layout, focus and drawing for the USB home screen.
use super::*;

const RECENT_COLUMNS: usize = 3;
const RECENT_RIGHT: f32 = HOME_DISK_X - 2.0;
const RECENT_LEFT: f32 = plx_ui::consts::MARGIN_X - 42.0;

fn advance_carousel(scroll: &mut Spring, start: usize, count: usize, dt: f32) -> bool {
    let target = start.min(count.saturating_sub(3)) as f32 * HOME_CARD_PITCH;
    let previous = scroll.pos;
    scroll.step(target, 240.0, dt);
    scroll.land(target, dt);
    previous != scroll.pos
}

impl Browser {
    pub(super) fn home_recent_offset(&self) -> usize {
        self.scan.volumes.len() + self.dashboard.items.len()
    }

    pub(super) fn home_more_index(&self) -> usize {
        self.home_recent_offset() + if self.home_all { 0 } else { self.home_recent.len() }
    }

    pub(super) fn home_recent_range(&self) -> std::ops::Range<usize> {
        self.home_recent_offset()..self.home_more_index()
    }

    fn recent_first(&self) -> usize {
        self.home_recent_start.min(self.home_recent.len().saturating_sub(RECENT_COLUMNS))
    }

    fn recent_top(&self) -> f32 {
        if self.dashboard.items.is_empty() { HOME_GRID_TOP } else { HOME_RECENT_TOP }
    }

    pub(super) fn reveal_recent_card(&mut self, position: usize) {
        self.home_recent_start = self.recent_first().clamp(position.saturating_sub(RECENT_COLUMNS - 1), position);
        let origin = plx_ui::consts::MARGIN_X + position as f32 * HOME_CARD_PITCH;
        // A fast key repeat can retarget the spring before the previous card settles.
        // Keep at least the selected card's edge inside the viewport in that case.
        let visible = self.home_recent_scroll.pos.clamp(origin - RECENT_RIGHT + 8.0,
            origin + HOME_CARD_W - RECENT_LEFT - 8.0);
        if self.home_recent_scroll.pos != visible { self.home_recent_scroll = Spring::at(visible); }
    }

    pub(super) fn advance_home_carousels(&mut self, dt: f32) -> bool {
        let resume = advance_carousel(&mut self.home_resume_scroll, self.home_resume_start, self.dashboard.items.len(), dt);
        let recent = advance_carousel(&mut self.home_recent_scroll, self.home_recent_start, self.home_recent.len(), dt);
        resume || recent
    }

    fn recent_arrow(&self, dir: Dir) -> Option<(Rect, usize)> {
        let count = self.home_recent.len();
        if !self.drive_picker() || self.home_all || count <= RECENT_COLUMNS { return None; }
        let start = self.recent_first();
        let end = start + RECENT_COLUMNS;
        let y = self.recent_top() - HOME_HEADING_PITCH + 2.0;
        let (x, index) = match dir {
            Dir::Left if start > 0 => (1114.0, start - 1),
            Dir::Right if end < count => (1378.0, end),
            _ => return None,
        };
        Some((Rect::new(x, y, 42.0, 40.0), self.home_recent_offset() + index))
    }

    pub(super) fn recent_arrow_at(&self, x: f32, y: f32) -> Option<(Dir, usize)> {
        [Dir::Left, Dir::Right].into_iter().find_map(|dir| {
            let (rect, index) = self.recent_arrow(dir)?;
            hit(rect, x, y).then_some((dir, index))
        })
    }

    pub(super) fn recent_card_accepts_pointer(&self, index: usize, x: f32) -> bool {
        !self.home_recent_range().contains(&index) || self.home_recent.len() <= RECENT_COLUMNS
            || (x >= plx_ui::consts::MARGIN_X && x < plx_ui::consts::MARGIN_X + RECENT_COLUMNS as f32 * HOME_CARD_PITCH - 18.0)
    }

    pub(super) fn home_rect(&self, index: usize) -> Rect {
        self.home_rect_measured(index, &TtfMeasure)
    }

    pub(super) fn home_rect_measured(&self, index: usize, measure: &dyn Measure) -> Rect {
        let drives = self.scan.volumes.len();
        let x = plx_ui::consts::MARGIN_X;
        let hidden = Rect::new(-5000.0, 0.0, 0.0, 0.0);
        if index < drives {
            let shown = drives.min(2);
            let start = self.home_disk_start.min(drives.saturating_sub(shown));
            let Some(slot) = index.checked_sub(start).filter(|&slot| slot < shown) else { return hidden };
            let top = HOME_HEADER_TOP;
            let height = (HOME_DISK_BOTTOM - top - (shown - 1) as f32 * 14.0) / shown as f32;
            return Rect::new(HOME_DISK_X, top + slot as f32 * (height + 14.0), 374.0, height);
        }
        if index == self.home_more_index() && self.dashboard.items.len() > 3 {
            return Rect::new(x + 3.0 * HOME_CARD_PITCH - 232.0, HOME_HEADER_TOP, 214.0, 44.0);
        }
        if index < self.home_recent_offset() {
            let position = index - drives;
            let slot = if self.home_all {
                let page = self.home_resume_start / 9 * 9;
                let Some(slot) = position.checked_sub(page).filter(|&slot| slot < 9) else { return hidden };
                slot
            } else {
                if self.dashboard.items.len() <= 3 {
                    return home_row_card(x, HOME_GRID_TOP, &self.dashboard.items, position, |item| {
                        let title = self.home_media_title(item, item.episode.is_some() || item.show);
                        media_card::home_title_width(&title, item.favorite, false, i18n::current().language(), measure)
                    });
                }
                let offset = position as f32 * HOME_CARD_PITCH - self.home_resume_scroll.pos;
                let width = 3.0 * HOME_CARD_PITCH - 18.0;
                if offset >= width || offset + HOME_CARD_W <= 0.0 { return hidden; }
                return Rect::new(x + offset, HOME_GRID_TOP, HOME_CARD_W, HOME_CARD_H);
            };
            let height = if self.home_all { 194.0 } else { HOME_CARD_H };
            return Rect::new(x + (slot % 3) as f32 * HOME_CARD_PITCH,
                HOME_GRID_TOP + (slot / 3) as f32 * 209.0, HOME_CARD_W, height);
        }
        let position = index - self.home_recent_offset();
        let top = self.recent_top();
        if !self.home_recent_range().contains(&index) { return hidden; }
        if self.home_recent.len() > RECENT_COLUMNS {
            let left = x + position as f32 * HOME_CARD_PITCH - self.home_recent_scroll.pos;
            if left >= RECENT_RIGHT || left + HOME_CARD_W <= RECENT_LEFT { return hidden; }
            return Rect::new(left, top, HOME_CARD_W, HOME_CARD_H);
        }
        home_row_card(x, top, &self.home_recent, position, |recent| {
            let title = self.home_media_title(&recent.item, recent.folder.is_some());
            media_card::home_title_width(&title, false, true, i18n::current().language(), measure)
        })
    }

    pub(super) fn home_neighbor(&self, dir: Dir) -> Option<usize> {
        let drives = self.scan.volumes.len();
        let count = self.dashboard.items.len();
        let recent = if self.home_all { 0 } else { self.home_recent.len() };
        let recent_offset = drives + count;
        let recent_start = self.recent_first();
        let more = (count > 3).then_some(self.home_more_index());
        let selected = self.selected;
        let disk = self.home_disk_start.min(drives.saturating_sub(1));
        if selected < drives {
            return match dir {
                Dir::Left if count > 0 => Some(drives + self.home_resume_start.min(count - 1)),
                Dir::Left if recent > 0 => Some(recent_offset + (recent_start + RECENT_COLUMNS - 1).min(recent - 1)),
                Dir::Up => selected.checked_sub(1),
                Dir::Down if selected + 1 < drives => Some(selected + 1),
                _ => None,
            };
        }
        if Some(selected) == more {
            return match dir {
                Dir::Down | Dir::Left => Some(drives + self.home_resume_start.min(count - 1)),
                Dir::Right if drives > 0 => Some(disk),
                _ => None,
            };
        }
        if selected < recent_offset {
            let index = selected - drives;
            return match dir {
                Dir::Left if index > 0 => Some(selected - 1),
                Dir::Right if index + 1 < count => Some(selected + 1),
                Dir::Right if drives > 0 => Some(disk),
                Dir::Up if self.home_all && index >= 3 => Some(selected - 3),
                Dir::Up => more,
                Dir::Down if self.home_all && index + 3 < count => Some(selected + 3),
                Dir::Down if recent > 0 => Some(recent_offset + (recent_start + index.saturating_sub(self.home_resume_start)).min(recent - 1)),
                _ => None,
            };
        }
        let index = selected - recent_offset;
        match dir {
            Dir::Left if index > 0 => Some(selected - 1),
            Dir::Right if index + 1 < recent => Some(selected + 1),
            Dir::Right if drives > 0 => Some(disk),
            Dir::Up if count > 0 => Some(drives + (self.home_resume_start + index.saturating_sub(recent_start)).min(count - 1)),
            _ => None,
        }
    }

    pub(super) fn home_focus(&self) -> Option<HomeFocus> {
        if !self.drive_picker() { return None; }
        if let Some(volume) = self.scan.volumes.get(self.selected) { return Some(HomeFocus::Drive(volume.clone())); }
        if let Some(item) = self.home_item() { return Some(HomeFocus::Resume(item.path.clone())); }
        if self.selected == self.home_more_index() && self.dashboard.items.len() > 3 { return Some(HomeFocus::More); }
        self.selected.checked_sub(self.home_recent_offset()).and_then(|index| self.home_recent.get(index))
            .map(|recent| HomeFocus::Recent(recent.key.clone()))
    }

    pub(super) fn restore_home_selection(&mut self, focus: Option<HomeFocus>) {
        if !self.drive_picker() { return; }
        if self.home_recent_start != self.recent_first() {
            self.home_recent_start = self.recent_first();
            self.home_recent_scroll = Spring::at(self.home_recent_start as f32 * HOME_CARD_PITCH);
        }
        if self.dashboard.items.len() <= 3 {
            self.home_all = false;
            self.home_resume_start = 0;
            self.home_resume_scroll = Spring::at(0.0);
        }
        let selected = match focus {
            Some(HomeFocus::Drive(volume)) => self.scan.volumes.iter().position(|drive| *drive == volume),
            Some(HomeFocus::Resume(path)) => self.dashboard.items.iter().position(|item| item.path == path)
                .map(|index| self.scan.volumes.len() + index),
            Some(HomeFocus::Recent(key)) if !self.home_all => self.home_recent.iter().position(|recent| recent.key == key)
                .map(|index| self.home_recent_offset() + index),
            Some(HomeFocus::More) if self.dashboard.items.len() > 3 => Some(self.home_more_index()),
            _ => None,
        };
        let index = selected.unwrap_or(self.selected).min(self.len().saturating_sub(1));
        let out_of_window = self.home_recent_range().contains(&index)
            && !(self.recent_first()..self.recent_first() + RECENT_COLUMNS).contains(&(index - self.home_recent_offset()));
        if index != self.selected || out_of_window || self.home_rect(index).w == 0.0 {
            self.select(index, true);
        }
    }

    pub(super) fn draw_home_tiles(&self, p: Painter, x: f32) {
        let drives = self.scan.volumes.len();
        let resume = drives..self.home_recent_offset();
        let recent = self.home_recent_range();
        if !resume.is_empty() {
            draw_home_heading(p, x, HOME_HEADER_TOP, Icon::Trailer, theme::USB_SERIES_CARD[2], self.accent(),
                tr("Continuar viendo", "Continua mirant"));
        }
        if !recent.is_empty() {
            let y = self.recent_top() - HOME_HEADING_PITCH;
            draw_home_heading(p, x, y, Icon::Clock, theme::USB_MOVIE_CARD[2], self.accent(),
                tr("Añadidas recientemente", "Afegides recentment"));
            self.draw_recent_carousel(p, recent.clone(), y);
        }
        if !resume.is_empty() {
            let carousel = resume.len() > 3;
            if carousel {
                p.clip(Rect::new(x - 12.0, HOME_GRID_TOP - 15.0, 3.0 * HOME_CARD_PITCH + 16.0,
                    if self.home_all { GRID_BOTTOM - HOME_GRID_TOP + 15.0 } else { HOME_CARD_H + 32.0 }));
            }
            for index in resume.clone().filter(|index| *index != self.selected) {
                self.draw_tile(p, index, false);
            }
            if resume.contains(&self.selected) { self.draw_tile(p, self.selected, self.grid_has_focus()); }
            if carousel { p.clip_clear(); }
        }
        for index in (0..self.len()).filter(|index| !resume.contains(index) && !recent.contains(index)) {
            self.draw_tile(p, index, self.selected == index && self.grid_has_focus());
        }
    }

    fn draw_recent_carousel(&self, p: Painter, cards: std::ops::Range<usize>, heading_y: f32) {
        let count = cards.len();
        let y = self.recent_top();
        if count > RECENT_COLUMNS {
            p.clip(Rect::new(RECENT_LEFT, y - 12.0, RECENT_RIGHT - RECENT_LEFT, HOME_CARD_H + 26.0));
        }
        for index in cards.clone().filter(|&index| index != self.selected) {
            self.draw_tile(p, index, false);
        }
        if cards.contains(&self.selected) { self.draw_tile(p, self.selected, self.grid_has_focus()); }
        if count <= RECENT_COLUMNS { return; }
        p.clip_clear();
        let end = self.recent_first() + RECENT_COLUMNS;
        for (dir, icon) in [(Dir::Left, Icon::ChevronLeft), (Dir::Right, Icon::Chevron)] {
            let Some((button, _)) = self.recent_arrow(dir) else { continue; };
            let hovered = self.home_recent_arrow_hover == Some(dir);
            let fill = if hovered { self.accent() } else { theme::CONTROL_IDLE_FILL };
            let ink = if hovered { theme::ACCENT_INK } else { theme::TEXT_PRIMARY };
            widgets::draw_control_face(p, button, fill, hovered, widgets::ControlGround::Keyed);
            icons::draw(p, icon, Rect::new(button.x + 9.0, button.y + 8.0, 24.0, 24.0), ink);
            let (edge, width) = if dir == Dir::Left { (RECENT_LEFT, 30.0) } else { (RECENT_RIGHT - 22.0, 22.0) };
            let dense = theme::with_a(theme::USB_HOME_BOTTOM, 0.88);
            let clear = theme::with_a(theme::USB_HOME_BOTTOM, 0.0);
            let corners = if dir == Dir::Left { [dense, clear, clear, dense] } else { [clear, dense, dense, clear] };
            p.grad4(Rect::new(edge, y, width, HOME_CARD_H), corners);
        }
        let pages = count.div_ceil(RECENT_COLUMNS);
        let current = (end - 1) / RECENT_COLUMNS;
        for page in 0..pages {
            let dot = Rect::new(1170.0 + page as f32 * 16.0, heading_y + 18.0, 8.0, 8.0);
            let color = if page == current { self.accent() } else { theme::with_a(theme::TEXT_PRIMARY, 0.4) };
            p.rrect(dot, 4.0, 4.0, color);
        }
        draw_line(p, &format!("{end} de {count}"),
            Rect::new(1264.0, heading_y, 106.0, 44.0), theme::size::MICRO, false);
    }

    pub(super) fn draw_home_card(&self, p: Painter, index: usize, card: Rect, radius: f32, lift: f32) {
        let Some(volume) = self.scan.volumes.get(index) else { return };
        let drive = self.dashboard.drives.iter().find(|drive| &drive.volume == volume);
        p.shadow_outside(card, radius, 16.0, 6.0, theme::CARD_SHADOW);
        p.rect_sheened(card, radius, theme::USB_DRIVE_TOP, theme::USB_DRIVE_BOTTOM);
        p.rring(card, radius, if lift > 0.0 { theme::CONTROL_FOCUS_RING_W } else { 1.0 },
            if lift > 0.0 { self.accent() } else { theme::HAIRLINE });
        let x = card.x + 20.0;
        let width = card.w - 40.0;
        let compact = card.h < 400.0;
        draw_glyph(p, Glyph::Drive, Rect::new(x, card.y + 19.0, 32.0, 32.0), self.accent());
        draw_line(p, &home_drive_name(volume, index, self.scan.volumes.len()),
            Rect::new(x + 43.0, card.y + 14.0, width - 43.0, 44.0), theme::size::LABEL, true);
        if let Some(stats) = drive.and_then(|drive| drive.stats) {
            for (index, (count, label, category)) in [
                (stats.movies, tr("películas", "pel·lícules"), Category::Movie),
                (stats.series, tr("series", "sèries"), Category::Series),
                (stats.documentaries, tr("documentales", "documentals"), Category::Documentary),
            ].into_iter().enumerate() {
                let y = card.y + 63.0 + index as f32 * if compact { 23.0 } else { 38.0 };
                icons::draw_svg(p, category.symbol(), Rect::new(x, y + 3.0, 19.0, 19.0), theme::TEXT_SECONDARY);
                draw_line(p, &format!("{count} {label}"), Rect::new(x + 28.0, y, width - 28.0, 25.0),
                    if compact { theme::size::MICRO } else { theme::size::CAPTION }, false);
            }
        } else {
            draw_line(p, tr("Leyendo la biblioteca…", "Llegint la biblioteca…"),
                Rect::new(x, card.y + 62.0, width, 32.0), theme::size::CAPTION, false);
        }
        let capacity = drive.and_then(|drive| drive.facts.0).or_else(|| self.facts.as_ref()
            .filter(|(current, _)| current == volume).and_then(|(_, facts)| facts.0));
        if let Some((total, used, free, _)) = capacity {
            let fraction = if total == 0 { 0.0 } else { (used as f64 / total as f64).clamp(0.0, 1.0) as f32 };
            let center_x = card.x + card.w * 0.5;
            let center_y = card.y + if compact { 168.0 } else { 294.0 };
            let radius = if compact { 31.0 } else { 91.0 };
            let step = if compact { 3.0 } else { 8.0 };
            let segments = 64;
            for index in 0..segments {
                let angle = index as f32 * std::f32::consts::TAU / segments as f32;
                let point = Rect::new(center_x + angle.sin() * radius - step * 0.5,
                    center_y - angle.cos() * radius - step * 0.5, step, step);
                p.rrect(point, step * 0.5, step * 0.5,
                    if index < (fraction * segments as f32).round() as usize { self.accent() } else { theme::RAIL_TRACK });
            }
            let percentage = format!("{:.0} %", 100.0 * fraction);
            let text_width = radius * 2.0;
            let size = if compact { theme::size::MICRO } else { theme::size::HEADLINE };
            let run = TtfMeasure.fit_line(&percentage, text_width, size, true);
            Label::new(run.as_ptr(), size, theme::TEXT_PRIMARY).bold().h(HAlign::Center)
                .draw(p, Rect::new(center_x - radius, center_y - 20.0, text_width, 40.0));
            for (index, (bytes, label)) in [
                (used, tr("Utilizado", "Utilitzat")), (free, tr("Libre", "Lliure")), (total, "Total"),
            ].into_iter().enumerate() {
                let pitch = if compact { 20.0 } else { 38.0 };
                let y = card.y + card.h - 3.0 * pitch - 10.0 + index as f32 * pitch;
                draw_line(p, &format!("{label} · {}", video_size(bytes)), Rect::new(x, y, width, pitch),
                    if compact { theme::size::MICRO } else { theme::size::CAPTION }, false);
            }
        }
    }

}

fn home_row_card<T>(x: f32, y: f32, items: &[T], index: usize, title_width: impl Fn(&T) -> f32) -> Rect {
    if index >= items.len() { return Rect::new(-5000.0, 0.0, 0.0, 0.0); }
    if items.len() >= 3 { return Rect::new(x + index as f32 * HOME_CARD_PITCH, y, HOME_CARD_W, HOME_CARD_H); }
    let spare = (3 - items.len()) as f32 * HOME_CARD_PITCH;
    let mut extra = [0.0; 2];
    for (each, item) in extra.iter_mut().zip(items) {
        *each = (title_width(item) - HOME_CARD_W).clamp(0.0, spare);
    }
    let scale = spare / extra.iter().sum::<f32>().max(spare);
    let left = x + index as f32 * HOME_CARD_PITCH + extra[..index].iter().sum::<f32>() * scale;
    Rect::new(left, y, HOME_CARD_W + extra[index] * scale, HOME_CARD_H)
}

fn draw_home_heading(p: Painter, x: f32, y: f32, icon: Icon, color: [f32; 4], accent: [f32; 4], title: &str) {
    let badge = Rect::new(x, y, 54.0, 44.0);
    p.rrect(badge, 10.0, 10.0, theme::with_a(color, 0.20));
    p.rring(badge, 10.0, 1.5, theme::with_a(accent, 0.65));
    icons::draw(p, icon, Rect::new(x + 7.0, y + 2.0, 40.0, 40.0), color);
    draw_line(p, title, Rect::new(x + 69.0, y, 800.0, 44.0), theme::size::HEADLINE, true);
}

fn home_drive_name(volume: &Volume, index: usize, count: usize) -> String {
    let label = volume_name(volume);
    let linux_device = label.strip_prefix("sd").is_some_and(|name| {
        let letters = name.bytes().take_while(u8::is_ascii_lowercase).count();
        letters > 0 && (letters == 1 || letters < name.len()) && name[letters..].bytes().all(|byte| byte.is_ascii_digit())
    });
    if !label.trim().is_empty() && !linux_device { return label; }
    let name = tr("Disco duro", "Disc dur");
    if count == 1 { name.into() } else { format!("{name} {}", index + 1) }
}

#[cfg(test)]
mod usb_browser_tests {
    use super::*;

    fn recent_browser(count: usize) -> Browser {
        let mut browser = Browser::new();
        let volume = Volume { path: "/tmp/usb/sda1".into(), label: None, mount_id: None, system: false };
        browser.scan.volumes.push(volume.clone());
        browser.home_recent = (0..count).map(|i| {
            let path = volume.path.join(format!("Film {i}.mkv"));
            let item = home::Item { volume: volume.clone(), path: path.clone(), title: format!("Film {i}"),
                episode: None, season_remaining: None, show: false, favorite: false, progress: 0,
                detail_key: (path.clone(), None, None) };
            home::Recent { item, folder: None, key: home::RecentKey::File(path) }
        }).collect();
        browser
    }

    fn settle_carousels(browser: &mut Browser) {
        for _ in 0..150 { browser.advance_home_carousels(1.0 / 60.0); }
    }

    #[test]
    fn recent_carousel_shows_three_full_cards_and_only_the_next_edge() {
        let measure = plx_base::fontcov::advances::ShippedMeasure;
        for count in [0, 1, 2, 3, 4, 15] {
            let browser = recent_browser(count);
            let offset = browser.home_recent_offset();
            assert_eq!(browser.home_recent_range().len(), count);
            assert_eq!(browser.recent_arrow(Dir::Right).is_some(), count > 3);
            assert!(browser.recent_arrow(Dir::Left).is_none());
            for index in browser.home_recent_range().take(3) {
                let card = browser.home_rect_measured(index, &measure);
                assert_eq!((card.w, card.h), (HOME_CARD_W, HOME_CARD_H));
                assert!(card.x + card.w < RECENT_RIGHT);
            }
            if count > 3 {
                let peek = browser.home_rect(offset + 3);
                assert!(peek.x < RECENT_RIGHT && peek.x + peek.w > RECENT_RIGHT);
                assert!(!browser.recent_card_accepts_pointer(offset + 3, peek.x));
                assert_eq!(browser.home_rect(offset + 4).w, 0.0);
            }
            assert!(RECENT_RIGHT < browser.home_rect(0).x);
        }
    }

    #[test]
    fn recent_carousel_scrolls_with_remote_and_pointer_and_keeps_the_disk_in_place() {
        let mut browser = recent_browser(15);
        let offset = browser.home_recent_offset();
        let disk = browser.home_rect(0);
        browser.select(offset + 2, true);
        browser.key(Key::Right { alt: false }, false);
        assert_eq!(browser.selected, offset + 3);
        assert_eq!(browser.home_recent_start, 1);
        assert!(browser.home_recent_scroll.pos < HOME_CARD_PITCH);
        assert!(browser.advance_home_carousels(1.0 / 60.0));
        assert!(browser.home_recent_scroll.pos > 0.0 && browser.home_recent_scroll.pos < HOME_CARD_PITCH);
        settle_carousels(&mut browser);
        assert_eq!(browser.home_recent_scroll.pos, HOME_CARD_PITCH);
        let (left, _) = browser.recent_arrow(Dir::Left).unwrap();
        let (x, y) = (left.x + left.w / 2.0, left.y + left.h / 2.0);
        assert!(browser.pointer(x, y));
        assert_eq!(browser.home_recent_arrow_hover, Some(Dir::Left));
        assert_eq!(browser.selected, offset + 3);
        browser.click(x, y);
        assert_eq!(browser.selected, offset);
        assert!(browser.dialog.is_none());
        settle_carousels(&mut browser);
        for _ in 0..14 { browser.key(Key::Right { alt: false }, false); }
        assert_eq!(browser.selected, offset + 14);
        assert!(browser.home_rect(browser.selected).w > 0.0);
        assert!(browser.recent_arrow(Dir::Right).is_none());
        settle_carousels(&mut browser);
        assert_eq!(browser.home_recent_scroll.pos, 12.0 * HOME_CARD_PITCH);
        assert_eq!(browser.home_rect(0), disk);
        browser.key(Key::Right { alt: false }, false);
        assert_eq!(browser.selected, 0);
        browser.key(Key::Left { alt: false }, false);
        assert_eq!(browser.selected, offset + 14);
    }

    #[test]
    fn recent_carousel_preserves_the_visible_column_and_the_series_focus() {
        let mut browser = recent_browser(8);
        browser.dashboard.items = browser.home_recent.iter().take(4).map(|recent| recent.item.clone()).collect();
        let offset = browser.home_recent_offset();
        let show = browser.scan.volumes[0].path.join("Series/Show");
        browser.home_recent[6].key = home::RecentKey::Series(show.clone(), None);
        browser.select(offset + 6, true);
        assert_eq!(browser.home_recent_start, 4);
        browser.key(Key::Up, false);
        assert_eq!(browser.selected, 3, "third visible column in Continue Watching");
        browser.key(Key::Down, false);
        assert_eq!(browser.selected, offset + 6);
        let focus = browser.home_focus();
        browser.home_recent[6].item.path = show.join("Show S01E02.mkv");
        let updated = browser.home_recent.remove(6);
        browser.home_recent.insert(0, updated);
        browser.restore_home_selection(focus);
        assert_eq!(browser.selected, offset);
        assert_eq!(browser.home_recent_start, 0);
        assert_eq!(browser.home_recent_scroll.pos, 0.0);
    }

    #[test]
    fn home_leaves_space_between_intro_rows_and_remote_hints() {
        let mut browser = Browser::new();
        let volume = Volume { path: "/tmp/usb/sdb1".into(), label: None, mount_id: None, system: false };
        browser.scan.volumes.push(volume);
        let disk = browser.home_rect(0);
        assert!(disk.y - HOME_INTRO_BOTTOM >= 36.0);
        let focused_resume_bottom = HOME_GRID_TOP + HOME_CARD_H * (1.0 + (USB_STYLE.focus_scale - 1.0) / 2.0);
        assert!(HOME_RECENT_TOP - HOME_HEADING_PITCH - focused_resume_bottom >= 22.0);
        assert_eq!(disk.y + disk.h, HOME_RECENT_TOP + HOME_CARD_H);
        assert!(disk.scaled(USB_STYLE.focus_scale).y + disk.scaled(USB_STYLE.focus_scale).h < 956.0);
    }

    #[test]
    fn longer_home_titles_use_free_slots_without_crossing_the_usb_panel() {
        let measure = plx_base::fontcov::advances::ShippedMeasure;
        let width = |title: &&str| media_card::home_title_width(title, true, false, Language::Ca, &measure);
        let x = plx_ui::consts::MARGIN_X;
        let titles = ["Les aventures extraordinàries del detectiu de Barcelona", "Llegendes"];
        let first = home_row_card(x, HOME_GRID_TOP, &titles, 0, width);
        let second = home_row_card(x, HOME_GRID_TOP, &titles, 1, width);
        assert!(first.w > HOME_CARD_W);
        assert_eq!(second.w, HOME_CARD_W);
        assert_eq!(second.x - first.x - first.w, HOME_CARD_PITCH - HOME_CARD_W);
        assert!(second.x + second.w < HOME_DISK_X);
        let widest = home_row_card(x, HOME_GRID_TOP, &["Sèrie"], 0, |_| 2000.0).scaled(USB_STYLE.focus_scale);
        assert!(widest.x + widest.w < HOME_DISK_X);
        let full = home_row_card(x, HOME_GRID_TOP, &[titles[0]; 3], 2, width);
        assert_eq!(full.w, HOME_CARD_W);
        assert!(full.x + full.w < HOME_DISK_X);
    }

    #[test]
    fn home_disk_replaces_device_ids_and_keeps_readable_labels() {
        let mut volume = Volume { path: "/tmp/usb/sdb1".into(), label: None, mount_id: None, system: false };
        assert!([ "Disco duro", "Disc dur" ].contains(&home_drive_name(&volume, 0, 1).as_str()));
        volume.label = Some("sdb1".into());
        assert!(home_drive_name(&volume, 1, 2).ends_with(" 2"));
        volume.label = Some("Biblioteca".into());
        assert_eq!(home_drive_name(&volume, 0, 2), "Biblioteca");
    }
}
