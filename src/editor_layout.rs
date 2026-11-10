//! Incremental unwrapped source layout. Unchanged rows share glyphs and meshes.
use eframe::egui::{
    self,
    emath::GuiRounding as _,
    text::{LayoutJob, LayoutSection},
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

#[derive(Clone)]
struct Line {
    galley: Arc<egui::Galley>,
    continued: Vec<egui::epaint::text::PlacedRow>,
    metrics: Arc<egui::Galley>,
    painted: bool,
}

#[derive(Default)]
struct Lines {
    entries: HashMap<u64, Arc<Line>>,
    previous: HashMap<u64, Arc<Line>>,
    geometry: HashMap<u64, Arc<Line>>,
    prepared_font: Option<u64>,
    pending: Option<(mpsc::Receiver<HashMap<u64, Arc<Line>>>, Arc<AtomicBool>)>,
    deferred: Vec<egui::Event>,
    job_key: Option<u64>,
    job: Option<Arc<LayoutJob>>,
    ordered_jobs: Vec<Arc<LayoutJob>>,
    ordered_lines: Vec<Arc<Line>>,
    ordered_keys: Vec<u64>,
    ordered_geometry_keys: Vec<u64>,
    composed: Option<Arc<egui::Galley>>,
    folded_composed: Option<Arc<egui::Galley>>,
    projection_key: Option<u64>,
    visible_rows: Vec<usize>,
    visible_tops: Vec<f32>,
}

impl Drop for Lines {
    fn drop(&mut self) {
        if let Some((_, cancel)) = &self.pending {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

#[derive(Default)]
struct Computer;
impl egui::cache::ComputerMut<egui::Id, Arc<Mutex<Lines>>> for Computer {
    fn compute(&mut self, _: egui::Id) -> Arc<Mutex<Lines>> {
        Arc::default()
    }
}

fn state(ui: &egui::Ui, id: egui::Id) -> Arc<Mutex<Lines>> {
    ui.ctx().memory_mut(|memory| {
        memory
            .caches
            .cache::<egui::cache::FrameCache<Arc<Mutex<Lines>>, Computer>>()
            .get(id)
    })
}

pub fn keep_alive(ui: &egui::Ui, id: egui::Id) {
    state(ui, id);
}

/// Geometry is prepared with independent fonts. Its atlas is never painted;
/// visible rows are meshed with the UI's own atlas when they enter the viewport.
pub fn prepare(
    ui: &egui::Ui,
    id: egui::Id,
    text: &str,
    font: &egui::FontId,
    color: egui::Color32,
) -> bool {
    let state = state(ui, id);
    let mut cache = state.lock().unwrap();
    let pixels = ui.ctx().pixels_per_point();
    let font_key = egui::util::hash((font, pixels.to_bits()));
    if cache.prepared_font != Some(font_key) {
        if let Some((rx, cancel)) = cache.pending.take() {
            cancel.store(true, Ordering::Relaxed);
            std::thread::spawn(move || drop(rx));
        }
        cache.prepared_font = Some(font_key);
        let definitions = ui.fonts(|fonts| fonts.definitions().clone());
        let font = font.clone();
        let text = text.to_owned();
        let repaint = ui.ctx().clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancelled = cancel.clone();
        let (tx, rx) = mpsc::channel();
        cache.pending = Some((rx, cancel));
        std::thread::spawn(move || {
            let worker = egui::Context::default();
            worker.set_fonts(definitions);
            worker.set_pixels_per_point(pixels);
            let _ = worker.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let mut entries = HashMap::new();
                    for text in text.split('\n') {
                        if cancelled.load(Ordering::Relaxed) {
                            return;
                        }
                        let job =
                            LayoutJob::simple(text.to_owned(), font.clone(), color, f32::INFINITY);
                        let key = geometry_key(&job, pixels);
                        if entries.contains_key(&key) {
                            continue;
                        }
                        let mut line = make_line(ui, job);
                        strip_meshes(&mut line);
                        entries.insert(key, Arc::new(line));
                    }
                    let _ = tx.send(entries);
                    repaint.request_repaint();
                });
            });
        });
    }
    if let Some((rx, _)) = &cache.pending {
        match rx.try_recv() {
            Ok(geometry) => {
                let previous = std::mem::replace(&mut cache.geometry, geometry);
                std::thread::spawn(move || drop(previous));
                cache.pending = None;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                // A failed preparation may be retried without disabling editing forever.
                cache.prepared_font = None;
                cache.pending = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }
    let ready = cache.pending.is_none() && cache.prepared_font.is_some();
    if ready {
        if !cache.deferred.is_empty() {
            let events = std::mem::take(&mut cache.deferred);
            ui.ctx().input_mut(|input| {
                input.events.splice(0..0, events);
            });
        }
    }
    ready
}

pub fn defer_input(ui: &egui::Ui, id: egui::Id) {
    let state = state(ui, id);
    let mut cache = state.lock().unwrap();
    ui.ctx().input_mut(|input| {
        input.events.retain(|event| {
            if matches!(
                event,
                egui::Event::Text(_)
                    | egui::Event::Key { .. }
                    | egui::Event::Paste(_)
                    | egui::Event::Copy
                    | egui::Event::Cut
                    | egui::Event::Ime(_)
            ) {
                cache.deferred.push(event.clone());
                false
            } else {
                true
            }
        });
    });
}

#[cfg(test)]
pub fn ready(ctx: &egui::Context, id: egui::Id) -> bool {
    let state = ctx.memory_mut(|memory| {
        memory
            .caches
            .cache::<egui::cache::FrameCache<Arc<Mutex<Lines>>, Computer>>()
            .get(id)
    });
    let cache = state.lock().unwrap();
    cache.prepared_font.is_some() && cache.pending.is_none()
}

fn geometry_key(job: &LayoutJob, pixels: f32) -> u64 {
    // Code syntax styles alter meshes, not font metrics. Retain exact geometry
    // separately from colors, underline and search decorations.
    let format = job.sections.first().map(|s| &s.format);
    let same = format.is_some_and(|first| {
        job.sections.iter().all(|s| {
            s.leading_space == 0.0
                && s.format.font_id == first.font_id
                && s.format.line_height == first.line_height
                && s.format.extra_letter_spacing == first.extra_letter_spacing
                && s.format.valign == first.valign
        })
    });
    if same {
        let format = format.unwrap();
        egui::util::hash((
            &job.text,
            &format.font_id,
            format.line_height.map(f32::to_bits),
            format.extra_letter_spacing.to_bits(),
            format.valign,
            job.first_row_min_height.to_bits(),
            pixels.to_bits(),
        ))
    } else {
        egui::util::hash((job, pixels.to_bits()))
    }
}

fn strip_meshes(line: &mut Line) {
    let galley = Arc::make_mut(&mut line.galley);
    for row in &mut galley.rows {
        Arc::make_mut(&mut row.row).visuals = Default::default();
    }
    for row in &mut line.continued {
        Arc::make_mut(&mut row.row).visuals = Default::default();
    }
    galley.mesh_bounds = egui::Rect::NOTHING;
    galley.num_vertices = 0;
    galley.num_indices = 0;
    line.painted = false;
}

fn make_line(ui: &egui::Ui, job: LayoutJob) -> Line {
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    let mut continued = galley.rows.clone();
    if let Some(last) = continued.last_mut() {
        Arc::make_mut(&mut last.row).ends_with_newline = true;
    }
    let mut metrics = (*galley).clone();
    metrics.rows.clear();
    metrics.mesh_bounds = egui::Rect::NOTHING;
    metrics.num_vertices = 0;
    metrics.num_indices = 0;
    Line {
        galley,
        continued,
        metrics: Arc::new(metrics),
        painted: true,
    }
}

pub fn viewport_key(ui: &egui::Ui) -> (i32, i32) {
    let origin = ui.cursor().top();
    (
        (ui.clip_rect().top() - origin).floor() as i32,
        (ui.clip_rect().bottom() - origin).ceil() as i32,
    )
}

#[cfg(test)]
pub fn layout(ui: &egui::Ui, id: egui::Id, job: LayoutJob) -> Arc<egui::Galley> {
    layout_visible(ui, id, job, None)
}

#[cfg(test)]
pub fn layout_visible(
    ui: &egui::Ui,
    id: egui::Id,
    job: LayoutJob,
    viewport: Option<(i32, i32)>,
) -> Arc<egui::Galley> {
    let key = egui::util::hash((&job, ui.ctx().pixels_per_point().to_bits()));
    if let Some(galley) = repaint_viewport(ui, id, key, viewport) {
        return galley;
    }
    layout_visible_keyed(ui, id, job, viewport, key)
}

/// A scroll changes the viewport, not the document or its syntax. Reuse line
/// jobs and geometry and mesh only newly visible rows before recomposing.
#[cfg(test)]
pub fn repaint_viewport(
    ui: &egui::Ui,
    id: egui::Id,
    key: u64,
    viewport: Option<(i32, i32)>,
) -> Option<Arc<egui::Galley>> {
    repaint_folded_viewport(ui, id, key, viewport, &[])
}

pub fn repaint_folded_viewport(
    ui: &egui::Ui,
    id: egui::Id,
    key: u64,
    viewport: Option<(i32, i32)>,
    folds: &[&crate::folding::Region],
) -> Option<Arc<egui::Galley>> {
    let state = state(ui, id);
    let mut cache = state.lock().unwrap();
    if cache.job_key != Some(key) {
        return None;
    }
    let job = cache.job.clone()?;
    project_rows(&mut cache, folds);
    let (start, end) = viewport.map_or((0, cache.visible_rows.len()), |(top, bottom)| {
        (
            cache
                .visible_tops
                .partition_point(|y| *y < top as f32)
                .saturating_sub(2),
            (cache.visible_tops.partition_point(|y| *y <= bottom as f32) + 1)
                .min(cache.visible_rows.len()),
        )
    });
    let mut changed = false;
    for index in start..end {
        let i = cache.visible_rows[index];
        if !cache.ordered_lines[i].painted {
            let key = cache.ordered_keys[i];
            let line = cache
                .entries
                .get(&key)
                .filter(|line| line.painted)
                .cloned()
                .unwrap_or_else(|| Arc::new(make_line(ui, (*cache.ordered_jobs[i]).clone())));
            cache.ordered_lines[i] = line.clone();
            cache.entries.insert(key, line);
            changed = true;
        }
    }
    if changed || cache.composed.is_none() {
        cache.composed = Some(compose(
            job,
            &cache.ordered_lines,
            ui.ctx().pixels_per_point(),
        ));
        cache.folded_composed = None;
    }
    Some(composed_view(&mut cache, folds))
}

fn project_rows(cache: &mut Lines, folds: &[&crate::folding::Region]) {
    let key = egui::util::hash(folds.iter().map(|r| (r.first, r.last)).collect::<Vec<_>>());
    if cache.projection_key == Some(key) {
        return;
    }
    cache.folded_composed = None;
    cache.visible_rows.clear();
    cache.visible_tops.clear();
    let mut y = 0.0;
    let mut fold = 0;
    for (i, line) in cache.ordered_lines.iter().enumerate() {
        while fold < folds.len() && i > folds[fold].last {
            fold += 1;
        }
        if folds.get(fold).is_some_and(|r| i > r.first && i <= r.last) {
            continue;
        }
        cache.visible_rows.push(i);
        cache.visible_tops.push(y);
        y += line.galley.rect.height();
    }
    cache.projection_key = Some(key);
}

#[cfg(test)]
pub fn layout_visible_keyed(
    ui: &egui::Ui,
    id: egui::Id,
    job: LayoutJob,
    viewport: Option<(i32, i32)>,
    job_key: u64,
) -> Arc<egui::Galley> {
    layout_folded_visible_keyed(ui, id, job, viewport, job_key, &[])
}

pub fn layout_folded_visible_keyed(
    ui: &egui::Ui,
    id: egui::Id,
    job: LayoutJob,
    viewport: Option<(i32, i32)>,
    job_key: u64,
    folds: &[&crate::folding::Region],
) -> Arc<egui::Galley> {
    if job.text.is_empty()
        || job.wrap.max_width.is_finite()
        || job.wrap.max_rows != usize::MAX
        || !job.break_on_newline
        || job.halign != egui::Align::LEFT
        || job.justify
    {
        return ui.fonts_mut(|fonts| fonts.layout_job(job));
    }
    let state = state(ui, id);
    let mut cache = state.lock().unwrap();
    let mut next = HashMap::with_capacity(cache.entries.len());
    let mut lines = Vec::new();
    let mut jobs = Vec::new();
    let mut keys = Vec::new();
    let mut geometry_keys = Vec::new();
    let mut next_geometry = HashMap::with_capacity(cache.geometry.len());
    let mut y = 0.0;
    let pixels = ui.ctx().pixels_per_point();
    let mut start = 0;
    let mut section_index = 0;
    // Empty trailing lines are significant to cursor positioning and selection.
    let mut fold = 0;
    for (line_index, text) in job.text.split('\n').enumerate() {
        let end = start + text.len();
        while section_index < job.sections.len()
            && job.sections[section_index].byte_range.end <= start
        {
            section_index += 1;
        }
        let mut sections = Vec::new();
        for section in &job.sections[section_index..] {
            if section.byte_range.start > end {
                break;
            }
            sections.push(LayoutSection {
                leading_space: if start <= section.byte_range.start {
                    section.leading_space
                } else {
                    0.0
                },
                byte_range: section.byte_range.start.saturating_sub(start)
                    ..section.byte_range.end.min(end).saturating_sub(start),
                format: section.format.clone(),
            });
        }
        // Blank lines must retain their font height, including the final empty row.
        if sections.is_empty()
            && let Some(section) = job.sections.get(section_index.saturating_sub(1))
        {
            sections.push(LayoutSection {
                leading_space: 0.0,
                byte_range: 0..0,
                format: section.format.clone(),
            });
        }
        let reusable = cache.ordered_jobs.get(line_index).filter(|old| {
            old.text == text
                && old.sections == sections
                && old.wrap == job.wrap
                && old.first_row_min_height
                    == if start == 0 {
                        job.first_row_min_height
                    } else {
                        0.0
                    }
                && old.break_on_newline == job.break_on_newline
                && old.halign == job.halign
                && old.justify == job.justify
                && old.round_output_to_gui == job.round_output_to_gui
        });
        let (line_job, key, geometry_key) = if let Some(old) = reusable {
            (
                old.clone(),
                cache.ordered_keys[line_index],
                cache.ordered_geometry_keys[line_index],
            )
        } else {
            let line_job = Arc::new(LayoutJob {
                text: text.to_owned(),
                sections,
                first_row_min_height: if start == 0 {
                    job.first_row_min_height
                } else {
                    0.0
                },
                wrap: job.wrap.clone(),
                break_on_newline: job.break_on_newline,
                halign: job.halign,
                justify: job.justify,
                round_output_to_gui: job.round_output_to_gui,
            });
            let key = egui::util::hash((line_job.as_ref(), pixels.to_bits()));
            let geometry = geometry_key(&line_job, pixels);
            (line_job, key, geometry)
        };
        let geometry = cache.geometry.get(&geometry_key).cloned();
        let height = geometry
            .as_ref()
            .map_or(32.0, |line| line.galley.rect.height());
        while fold < folds.len() && line_index > folds[fold].last {
            fold += 1;
        }
        let hidden = folds
            .get(fold)
            .is_some_and(|r| line_index > r.first && line_index <= r.last);
        let visible = !hidden
            && viewport.is_none_or(|(top, bottom)| {
                y + height >= top as f32 - height && y <= bottom as f32 + height
            });
        let line = next
            .get(&key)
            .or_else(|| cache.entries.get(&key))
            .or_else(|| cache.previous.get(&key))
            .filter(|line| line.painted || !visible)
            .cloned()
            .unwrap_or_else(|| {
                if !visible && let Some(geometry) = geometry {
                    return Arc::new(Line {
                        painted: false,
                        ..(*geometry).clone()
                    });
                }
                Arc::new(make_line(ui, (*line_job).clone()))
            });
        if !hidden {
            y += line.galley.rect.height();
        }
        next_geometry.insert(geometry_key, line.clone());
        next.insert(key, line.clone());
        lines.push(line);
        jobs.push(line_job);
        keys.push(key);
        geometry_keys.push(geometry_key);
        start = end + 1;
    }
    // Keep the plain and syntax-colored versions across asynchronous analysis.
    cache.previous = std::mem::replace(&mut cache.entries, next);
    cache.geometry = next_geometry;
    let job = Arc::new(job);
    cache.job = Some(job.clone());
    cache.job_key = Some(job_key);
    cache.ordered_lines = lines;
    cache.ordered_jobs = jobs;
    cache.ordered_keys = keys;
    cache.ordered_geometry_keys = geometry_keys;
    cache.projection_key = None;
    project_rows(&mut cache, folds);
    cache.composed = Some(compose(job, &cache.ordered_lines, pixels));
    cache.folded_composed = None;
    composed_view(&mut cache, folds)
}

fn composed_view(cache: &mut Lines, folds: &[&crate::folding::Region]) -> Arc<egui::Galley> {
    if folds.is_empty() {
        return cache.composed.as_ref().unwrap().clone();
    }
    if let Some(galley) = &cache.folded_composed {
        return galley.clone();
    }
    let galley = crate::folding::layout(cache.composed.as_ref().unwrap().clone(), folds);
    cache.folded_composed = Some(galley.clone());
    galley
}

fn compose(job: Arc<LayoutJob>, lines: &[Arc<Line>], pixels: f32) -> Arc<egui::Galley> {
    let metrics: Vec<_> = lines.iter().map(|line| line.metrics.clone()).collect();
    let mut merged = egui::Galley::concat(job.clone(), &metrics, pixels);
    merged.rows.reserve(lines.len());
    for (i, line) in lines.iter().enumerate() {
        let offset = merged.rect.height();
        let rows = if i + 1 == lines.len() {
            &line.galley.rows
        } else {
            &line.continued
        };
        for row in rows {
            let pos = (row.pos + egui::vec2(0.0, offset)).round_to_pixels(pixels);
            merged.rect |= egui::Rect::from_min_size(pos, row.size);
            merged.mesh_bounds |= row.visuals.mesh_bounds.translate(pos.to_vec2());
            merged.num_vertices += row.visuals.mesh.vertices.len();
            merged.num_indices += row.visuals.mesh.indices.len();
            merged.rows.push(egui::epaint::text::PlacedRow {
                pos,
                row: row.row.clone(),
            });
        }
    }
    if job.round_output_to_gui {
        merged.rect = merged.rect.round_ui();
    }
    Arc::new(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn incremental_rows_match_full_layout_for_unicode_blank_lines_and_styles() {
        let ctx = egui::Context::default();
        for source in [
            "",
            "\n",
            "\n\n",
            "one\ntwo",
            "one\ntwo\n",
            "猫 e\u{301}\tλ\n\nlast\n",
            "\r\nhello\r\n",
        ] {
            let _ = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let mut job = LayoutJob::simple(
                        source.into(),
                        egui::FontId::monospace(13.0),
                        egui::Color32::WHITE,
                        f32::INFINITY,
                    );
                    if !source.is_empty() {
                        crate::chrome::mark_matches(&mut job, "o");
                    }
                    let full = ui.fonts_mut(|fonts| fonts.layout_job(job.clone()));
                    let incremental = layout(ui, egui::Id::new("test"), job);
                    assert_eq!(full.rows, incremental.rows, "rows for {source:?}");
                    assert_eq!(full.rect, incremental.rect, "bounds for {source:?}");
                    assert_eq!(full.intrinsic_size(), incremental.intrinsic_size());
                    assert_eq!(full.mesh_bounds, incremental.mesh_bounds);
                    assert_eq!(full.num_vertices, incremental.num_vertices);
                    for i in 0..=source.chars().count() {
                        assert_eq!(
                            full.pos_from_cursor(egui::text::CCursor::new(i)),
                            incremental.pos_from_cursor(egui::text::CCursor::new(i))
                        );
                    }
                });
            });
        }
    }

    #[test]
    fn prepared_geometry_meshes_only_visible_lines_and_scrolls_to_the_end() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("viewport");
        let source = (0..1500)
            .map(|i| format!("const value{i} = '猫';\n"))
            .collect::<String>();
        let font = egui::FontId::monospace(13.0);
        let job = LayoutJob::simple(
            source.clone(),
            font.clone(),
            egui::Color32::WHITE,
            f32::INFINITY,
        );
        let mut top = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while top.is_none() {
            let _ = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    if prepare(ui, id, &source, &font, egui::Color32::WHITE) {
                        top = Some(layout_visible(ui, id, job.clone(), Some((0, 400))));
                    }
                });
            });
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        let top = top.unwrap();
        assert!(!top.rows[0].visuals.mesh.vertices.is_empty());
        assert!(top.rows[1499].visuals.mesh.vertices.is_empty());
        assert_eq!(top.rows.len(), 1501);
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let end = top.rect.height() as i32;
                let bottom = layout_visible(ui, id, job.clone(), Some((end - 400, end)));
                assert!(!bottom.rows[1499].visuals.mesh.vertices.is_empty());
                let full = ui.fonts_mut(|fonts| fonts.layout_job(job.clone()));
                for index in [0, 1, 17, 1499, 1500] {
                    assert_eq!(full.rows[index].pos, bottom.rows[index].pos);
                    assert_eq!(
                        full.rows[index]
                            .glyphs
                            .iter()
                            .map(|g| (g.chr, g.pos, g.advance_width))
                            .collect::<Vec<_>>(),
                        bottom.rows[index]
                            .glyphs
                            .iter()
                            .map(|g| (g.chr, g.pos, g.advance_width))
                            .collect::<Vec<_>>()
                    );
                }
                assert_eq!(full.rect, bottom.rect);
            });
        });
    }

    #[test]
    fn folded_viewports_mesh_rows_after_the_fold_and_reuse_composed_geometry() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("folded-viewport");
        let source = (0..1500).map(|i| format!("line {i}\n")).collect::<String>();
        let font = egui::FontId::monospace(13.0);
        let fold = crate::folding::Region {
            start: 0,
            end: 1400,
            hidden_start: 1,
            first: 0,
            last: 1400,
            preview: String::new(),
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let mut done = false;
            let _ = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    if !prepare(ui, id, &source, &font, egui::Color32::WHITE) {
                        return;
                    }
                    let job = LayoutJob::simple(
                        source.clone(),
                        font.clone(),
                        egui::Color32::WHITE,
                        f32::INFINITY,
                    );
                    let top =
                        layout_folded_visible_keyed(ui, id, job, Some((0, 300)), 99, &[&fold]);
                    assert!(top.rows[500].visuals.mesh.vertices.is_empty());
                    assert!(!top.rows[1401].visuals.mesh.vertices.is_empty());
                    let next =
                        repaint_folded_viewport(ui, id, 99, Some((0, 300)), &[&fold]).unwrap();
                    assert!(
                        Arc::ptr_eq(&top, &next),
                        "unchanged viewport reuses the entire galley"
                    );
                    done = true;
                });
            });
            if done {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn idle_frames_retain_rows_and_edits_reuse_unchanged_meshes() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("incremental");
        let mut previous = None;
        for (i, source) in [
            "first\nunchanged\n",
            "first\nunchanged\n",
            "changed\nunchanged\n",
        ]
        .into_iter()
        .enumerate()
        {
            let _ = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    keep_alive(ui, id);
                    if i == 1 {
                        return;
                    }
                    let job = LayoutJob::simple(
                        source.into(),
                        egui::FontId::monospace(13.0),
                        egui::Color32::WHITE,
                        f32::INFINITY,
                    );
                    let galley = layout(ui, id, job);
                    if let Some(row) = &previous {
                        assert!(Arc::ptr_eq(row, &galley.rows[1].row));
                    }
                    previous = Some(galley.rows[1].row.clone());
                });
            });
        }
    }
}
