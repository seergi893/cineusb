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
        Rect::new(x + position as f32 * HOME_CARD_PITCH, top, HOME_CARD_W, HOME_CARD_H)
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
            draw_glyph(p, Glyph::Play, Rect::new(x, HOME_HEADER_TOP + 8.0, 26.0, 26.0), self.accent());
            draw_line(p, tr("Continuar viendo", "Continua mirant"),
                Rect::new(x + 41.0, HOME_HEADER_TOP, 800.0, 44.0), theme::size::HEADLINE, true);
        }
        if !self.home_all && !self.home_recent.is_empty() {
            let y = if resume.is_empty() { HOME_HEADER_TOP } else { HOME_RECENT_TOP - 56.0 };
            icons::draw(p, Icon::Trailer, Rect::new(x, y + 8.0, 27.0, 27.0), self.accent());
            draw_line(p, tr("Añadidas recientemente", "Afegides recentment"),
                Rect::new(x + 41.0, y, 850.0, 44.0), theme::size::HEADLINE, true);
        }
        if !resume.is_empty() {
            p.clip(Rect::new(x - 12.0, HOME_GRID_TOP - 15.0, 3.0 * HOME_CARD_PITCH + 16.0,
                if self.home_all { GRID_BOTTOM - HOME_GRID_TOP + 15.0 } else { HOME_CARD_H + 32.0 }));
            for index in resume.clone().filter(|index| *index != self.selected) {
                self.draw_tile(p, index, false);
            }
            if resume.contains(&self.selected) { self.draw_tile(p, self.selected, self.grid_has_focus()); }
            p.clip_clear();
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
        draw_line(p, &volume_name(volume), Rect::new(x + 43.0, card.y + 14.0, width - 43.0, 44.0), theme::size::LABEL, true);
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
