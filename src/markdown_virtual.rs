//! Geometry for large Markdown blocks is prepared off-thread. Only visible
//! wrapped rows are meshed with the UI atlas; links retain their hit targets.
use eframe::egui::{
    self,
    text::{LayoutJob, LayoutSection},
};
use pulldown_cmark::{Alignment, Event, Tag};
use std::{
    collections::HashMap,
    ops::Range,
    sync::{Arc, Mutex},
};
struct Link {
    bytes: Range<usize>,
    target: String,
}
pub struct Rich {
    geometry: Arc<egui::Galley>,
    ranges: Vec<Range<usize>>,
    links: Vec<Link>,
    meshes: Mutex<HashMap<usize, Arc<egui::Galley>>>,
}
impl Rich {
    fn build(ui: &egui::Ui, events: &[Event<'static>], width: f32, header: bool) -> Self {
        let dark = ui.visuals().dark_mode;
        let mut format = egui::TextFormat {
            font_id: egui::FontId::proportional(15.0),
            color: if dark {
                crate::chrome::TEXT
            } else {
                egui::Color32::BLACK
            },
            ..Default::default()
        };
        if header {
            format.color = if dark {
                egui::Color32::WHITE
            } else {
                egui::Color32::BLACK
            };
        }
        let mut stack = Vec::new();
        let mut links = Vec::new();
        let mut active_links = Vec::new();
        let mut lists: Vec<Option<u64>> = Vec::new();
        let mut job = LayoutJob::default();
        job.wrap.max_width = width.max(1.0);
        for event in events {
            match event {
                Event::Start(tag) => {
                    stack.push(format.clone());
                    match tag {
                        Tag::Emphasis => format.italics = true,
                        Tag::Strong => {
                            format.color = if dark {
                                egui::Color32::WHITE
                            } else {
                                egui::Color32::BLACK
                            }
                        }
                        Tag::Strikethrough => {
                            format.strikethrough = egui::Stroke::new(1.0_f32, format.color)
                        }
                        Tag::Link { dest_url, .. } => {
                            active_links.push((job.text.len(), dest_url.to_string()));
                            format.color = crate::chrome::BLUE;
                            format.underline = egui::Stroke::new(1.0_f32, crate::chrome::BLUE);
                        }
                        Tag::Heading { .. } => {
                            format.font_id.size = 28.0;
                            if !job.text.is_empty() {
                                job.append("\n", 0.0, format.clone());
                            }
                        }
                        Tag::Paragraph => {
                            if !job.text.is_empty() && !job.text.ends_with('\n') {
                                job.append("\n", 0.0, format.clone());
                            }
                        }
                        Tag::List(start) => lists.push(*start),
                        Tag::Item => {
                            if !job.text.is_empty() && !job.text.ends_with('\n') {
                                job.append("\n", 0.0, format.clone());
                            }
                            let indent = "  ".repeat(lists.len().saturating_sub(1));
                            let bullet = if let Some(Some(number)) = lists.last_mut() {
                                let bullet = format!("{number}. ");
                                *number += 1;
                                bullet
                            } else {
                                "• ".into()
                            };
                            job.append(&format!("{indent}{bullet}"), 0.0, format.clone());
                        }
                        Tag::CodeBlock(_) => format.font_id = egui::FontId::monospace(13.0),
                        _ => {}
                    }
                }
                Event::End(end) => {
                    if matches!(end, pulldown_cmark::TagEnd::Link)
                        && let Some((start, target)) = active_links.pop()
                    {
                        links.push(Link {
                            bytes: start..job.text.len(),
                            target,
                        });
                    }
                    if matches!(end, pulldown_cmark::TagEnd::List(_)) {
                        lists.pop();
                    }
                    if let Some(previous) = stack.pop() {
                        format = previous;
                    }
                }
                Event::Text(text) => job.append(text, 0.0, format.clone()),
                Event::Code(text) => {
                    let mut code = format.clone();
                    code.font_id = egui::FontId::monospace(13.0);
                    code.background = ui.visuals().code_bg_color;
                    job.append(text, 0.0, code);
                }
                Event::SoftBreak => job.append(" ", 0.0, format.clone()),
                Event::HardBreak => job.append("\n", 0.0, format.clone()),
                Event::TaskListMarker(checked) => {
                    job.append(if *checked { "☑ " } else { "☐ " }, 0.0, format.clone())
                }
                _ => {}
            }
        }
        let mut geometry = ui.fonts_mut(|fonts| fonts.layout_job(job));
        let mut characters = geometry.job.text.char_indices();
        let mut byte = 0;
        let mut ranges = Vec::new();
        for row in &geometry.rows {
            let start = byte;
            for _ in 0..row.glyphs.len() {
                if let Some((at, ch)) = characters.next() {
                    byte = at + ch.len_utf8();
                }
            }
            ranges.push(start..byte);
            if row.ends_with_newline {
                if let Some((at, ch)) = characters.next() {
                    byte = at + ch.len_utf8();
                }
            }
        }
        // Glyph metrics are portable between identical fonts. Worker atlas
        // meshes are discarded; painting always uses the UI's font atlas.
        for row in &mut Arc::make_mut(&mut geometry).rows {
            Arc::make_mut(&mut row.row).visuals = Default::default();
        }
        Self {
            geometry,
            ranges,
            links,
            meshes: Mutex::new(HashMap::new()),
        }
    }
    fn height(&self) -> f32 {
        self.geometry.rect.height()
    }
    fn paint(
        &self,
        ui: &mut egui::Ui,
        origin: egui::Pos2,
        clip: egui::Rect,
        align: Alignment,
        width: f32,
        base: Option<&url::Url>,
    ) {
        let clip = clip.intersect(ui.clip_rect());
        let painter = ui.painter().with_clip_rect(clip);
        let first = self
            .geometry
            .rows
            .partition_point(|row| origin.y + row.pos.y + row.size.y < clip.top());
        let mut meshes = self.meshes.lock().unwrap();
        if meshes.len() > 512 {
            meshes.clear();
        }
        for (i, row) in self
            .geometry
            .rows
            .iter()
            .enumerate()
            .skip(first)
            .take_while(|(_, row)| origin.y + row.pos.y <= clip.bottom())
        {
            let range = &self.ranges[i];
            let mesh = meshes.entry(i).or_insert_with(|| {
                let job = &self.geometry.job;
                let sections = job
                    .sections
                    .iter()
                    .filter(|s| s.byte_range.start < range.end && s.byte_range.end >= range.start)
                    .map(|section| LayoutSection {
                        byte_range: section.byte_range.start.max(range.start) - range.start
                            ..section.byte_range.end.min(range.end) - range.start,
                        ..section.clone()
                    })
                    .collect();
                let job = LayoutJob {
                    text: job.text[range.clone()].to_owned(),
                    sections,
                    first_row_min_height: row.size.y,
                    ..Default::default()
                };
                ui.fonts_mut(|fonts| fonts.layout_job(job))
            });
            let x = match align {
                Alignment::Center => (width - row.size.x) / 2.0,
                Alignment::Right => width - row.size.x,
                _ => 0.0,
            }
            .max(0.0);
            let at = origin + row.pos.to_vec2() + egui::vec2(x, 0.0);
            painter.galley(at, mesh.clone(), crate::chrome::TEXT);
            for link in self
                .links
                .iter()
                .filter(|link| link.bytes.start < range.end && link.bytes.end > range.start)
            {
                let start = self.geometry.job.text[range.start..link.bytes.start.max(range.start)]
                    .chars()
                    .count();
                let end = self.geometry.job.text[range.start..link.bytes.end.min(range.end)]
                    .chars()
                    .count();
                if let (Some(a), Some(b)) =
                    (row.glyphs.get(start), row.glyphs.get(end.saturating_sub(1)))
                {
                    let rect = egui::Rect::from_min_max(
                        at + egui::vec2(a.pos.x, 0.0),
                        at + egui::vec2(b.pos.x + b.advance_width, row.size.y),
                    )
                    .intersect(clip);
                    let response = ui
                        .interact(
                            rect,
                            ui.id().with((self as *const _ as usize, i, start)),
                            egui::Sense::click(),
                        )
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(&link.target);
                    if response.clicked() {
                        let url = url::Url::parse(&link.target)
                            .ok()
                            .or_else(|| base.and_then(|base| base.join(&link.target).ok()));
                        if let Some(url) = url.filter(|url| {
                            matches!(url.scheme(), "http" | "https" | "file" | "mailto")
                        }) {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(url.to_string()));
                        }
                    }
                }
            }
        }
    }
}
pub enum Block {
    Prose(Rich),
    Table {
        rows: Vec<Vec<Rich>>,
        positions: crate::markdown::HeightIndex,
        alignments: Vec<Alignment>,
        width: f32,
    },
}
impl Block {
    pub fn build(ui: &egui::Ui, events: &[Event<'static>], width: f32) -> Option<Self> {
        if events.len() > 512
            && let Some(Event::Start(Tag::Table(alignments))) = events.first()
        {
            let columns = alignments.len().max(1);
            let column = width / columns as f32 - 16.0;
            let mut rows = Vec::new();
            let mut cells = Vec::new();
            let mut start = None;
            for (i, event) in events.iter().enumerate() {
                match event {
                    Event::Start(Tag::TableCell) => start = Some(i + 1),
                    Event::End(pulldown_cmark::TagEnd::TableCell) => {
                        if let Some(start) = start.take() {
                            cells.push(Rich::build(ui, &events[start..i], column, rows.is_empty()));
                        }
                    }
                    Event::End(
                        pulldown_cmark::TagEnd::TableHead | pulldown_cmark::TagEnd::TableRow,
                    ) => rows.push(std::mem::take(&mut cells)),
                    _ => {}
                }
            }
            let mut positions = crate::markdown::HeightIndex::default();
            positions.reset(
                rows.iter()
                    .map(|row| row.iter().map(Rich::height).fold(20.0, f32::max) + 12.0)
                    .collect(),
            );
            return Some(Self::Table {
                rows,
                positions,
                alignments: alignments.clone(),
                width,
            });
        }
        let length: usize = events
            .iter()
            .filter_map(|event| {
                if let Event::Text(text) = event {
                    Some(text.len())
                } else {
                    None
                }
            })
            .sum();
        if length > 4096
            && !events.iter().any(|event| {
                matches!(
                    event,
                    Event::Start(Tag::Image { .. } | Tag::CodeBlock(_) | Tag::Table(_))
                        | Event::Html(_)
                        | Event::InlineHtml(_)
                )
            })
        {
            Some(Self::Prose(Rich::build(ui, events, width, false)))
        } else {
            None
        }
    }
    pub fn height(&self) -> f32 {
        match self {
            Self::Prose(rich) => rich.height(),
            Self::Table {
                rows, positions, ..
            } => positions.prefix(rows.len()),
        }
    }
    pub fn show(&self, ui: &mut egui::Ui, base: Option<&url::Url>) {
        let width = ui.available_width();
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(width, self.height()), egui::Sense::hover());
        match self {
            Self::Prose(rich) => rich.paint(ui, rect.min, rect, Alignment::Left, width, base),
            Self::Table {
                rows,
                positions,
                alignments,
                width,
            } => {
                let first = positions.row((ui.clip_rect().top() - rect.top()).max(0.0));
                let column = width / alignments.len().max(1) as f32;
                let bottom_clip = ui.clip_rect().bottom();
                for (i, row) in rows
                    .iter()
                    .enumerate()
                    .skip(first)
                    .take_while(|(i, _)| rect.top() + positions.prefix(*i) <= bottom_clip)
                {
                    let top = rect.top() + positions.prefix(i);
                    let bottom = rect.top() + positions.prefix(i + 1);
                    for (j, cell) in row.iter().enumerate() {
                        let bounds = egui::Rect::from_min_max(
                            egui::pos2(rect.left() + j as f32 * column, top),
                            egui::pos2(rect.left() + (j + 1) as f32 * column, bottom),
                        );
                        cell.paint(
                            ui,
                            bounds.min + egui::vec2(8.0, 6.0),
                            bounds,
                            alignments.get(j).copied().unwrap_or(Alignment::Left),
                            column - 16.0,
                            base,
                        );
                    }
                    ui.painter().hline(
                        rect.x_range(),
                        bottom,
                        egui::Stroke::new(1.0_f32, crate::chrome::BORDER),
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn large_prose_and_tables_mesh_only_visible_unicode_rows() {
        let ctx = egui::Context::default();
        crate::chrome::install(&ctx);
        for source in [
            format!(
                "{} [link](https://example.org)",
                "猫 word **bold** ".repeat(3000)
            ),
            format!(
                "| Name | Value |\n| --- | ---: |\n{}",
                (0..1000)
                    .map(|i| format!("| 猫 {i} | value |\n"))
                    .collect::<String>()
            ),
        ] {
            let events: Vec<_> =
                pulldown_cmark::Parser::new_ext(&source, pulldown_cmark::Options::ENABLE_TABLES)
                    .map(Event::into_static)
                    .collect();
            let mut block = None;
            let _ = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    block = Block::build(ui, &events, 400.0);
                });
            });
            let block = block.expect("large block uses virtual rows");
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(420.0, 300.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        block.show(ui, None);
                    });
                },
            );
            assert!(
                output
                    .shapes
                    .iter()
                    .filter(|shape| matches!(&shape.shape, egui::Shape::Text(_)))
                    .count()
                    < 50
            );
            assert!(block.height() > 10000.0);
            if let Block::Prose(rich) = &block {
                assert_eq!(rich.links.len(), 1);
                assert_eq!(&rich.geometry.job.text[rich.links[0].bytes.clone()], "link");
                assert!(rich.meshes.lock().unwrap().len() < 50);
            }
        }
    }
}
