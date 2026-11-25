//! Layout, focus and drawing for the USB home screen.
use super::*;

impl Browser {
    pub(super) fn home_recent_offset(&self) -> usize {
        self.scan.volumes.len() + self.dashboard.items.len()
    }

    pub(super) fn home_more_index(&self) -> usize {
        self.home_recent_offset() + if self.home_all { 0 } else { self.home_recent.len() }
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
        let top = if self.dashboard.items.is_empty() { HOME_GRID_TOP } else { HOME_RECENT_TOP };
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
        let more = (count > 3).then_some(self.home_more_index());
        let selected = self.selected;
        let disk = self.home_disk_start.min(drives.saturating_sub(1));
        if selected < drives {
            return match dir {
                Dir::Left if count > 0 => Some(drives + self.home_resume_start.min(count - 1)),
                Dir::Left if recent > 0 => Some(recent_offset + recent - 1),
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
                Dir::Down if recent > 0 => Some(recent_offset + (index.saturating_sub(self.home_resume_start)).min(recent - 1)),
                _ => None,
            };
        }
        let index = selected - recent_offset;
        match dir {
            Dir::Left if index > 0 => Some(selected - 1),
            Dir::Right if index + 1 < recent => Some(selected + 1),
            Dir::Right if drives > 0 => Some(disk),
            Dir::Up if count > 0 => Some(drives + (self.home_resume_start + index).min(count - 1)),
            _ => None,
        }
    }

    pub(super) fn home_focus(&self) -> Option<HomeFocus> {
        if !self.drive_picker() { return None; }
        if let Some(volume) = self.scan.volumes.get(self.selected) { return Some(HomeFocus::Drive(volume.clone())); }
        if let Some(item) = self.home_item() { return Some(HomeFocus::Resume(item.path.clone())); }
        if self.selected == self.home_more_index() && self.dashboard.items.len() > 3 { return Some(HomeFocus::More); }
        self.selected.checked_sub(self.home_recent_offset()).and_then(|index| self.home_recent.get(index))
            .map(|recent| HomeFocus::Recent(recent.item.path.clone()))
    }

    pub(super) fn restore_home_selection(&mut self, focus: Option<HomeFocus>) {
        if !self.drive_picker() { return; }
        if self.dashboard.items.len() <= 3 {
            self.home_all = false;
            self.home_resume_start = 0;
            self.home_resume_scroll = Spring::at(0.0);
        }
        let selected = match focus {
            Some(HomeFocus::Drive(volume)) => self.scan.volumes.iter().position(|drive| *drive == volume),
            Some(HomeFocus::Resume(path)) => self.dashboard.items.iter().position(|item| item.path == path)
                .map(|index| self.scan.volumes.len() + index),
            Some(HomeFocus::Recent(path)) if !self.home_all => self.home_recent.iter().position(|recent| recent.item.path == path)
                .map(|index| self.home_recent_offset() + index),
            Some(HomeFocus::More) if self.dashboard.items.len() > 3 => Some(self.home_more_index()),
            _ => None,
        };
        let index = selected.unwrap_or(self.selected).min(self.len().saturating_sub(1));
        if index != self.selected || self.home_rect(index).w == 0.0 {
            self.select(index, true);
        }
    }

    pub(super) fn draw_home_tiles(&self, p: Painter, x: f32) {
        let drives = self.scan.volumes.len();
        let resume = drives..self.home_recent_offset();
        if !resume.is_empty() {
            draw_home_heading(p, x, HOME_HEADER_TOP, Icon::Trailer, theme::USB_SERIES_CARD[2], self.accent(),
                tr("Continuar viendo", "Continua mirant"));
        }
        if !self.home_all && !self.home_recent.is_empty() {
            let y = if resume.is_empty() { HOME_HEADER_TOP } else { HOME_RECENT_TOP - HOME_HEADING_PITCH };
            draw_home_heading(p, x, y, Icon::Clock, theme::USB_MOVIE_CARD[2], self.accent(),
                tr("Añadidas recientemente", "Afegides recentment"));
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
        for index in (0..self.len()).filter(|index| !resume.contains(index)) {
            self.draw_tile(p, index, self.selected == index && self.grid_has_focus());
        }
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
