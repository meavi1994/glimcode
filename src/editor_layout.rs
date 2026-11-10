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

#[cfg(test)]
static FULL_LAYOUTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
static EDIT_LAYOUTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
static VIEWPORT_PATCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
pub fn counters() -> (u64, u64, u64) {
    (
        FULL_LAYOUTS.load(Ordering::Relaxed),
        EDIT_LAYOUTS.load(Ordering::Relaxed),
        VIEWPORT_PATCHES.load(Ordering::Relaxed),
    )
}

#[derive(Clone)]
struct Line {
    galley: Arc<egui::Galley>,
    continued: Vec<egui::epaint::text::PlacedRow>,
    metrics: Arc<egui::Galley>,
    painted: bool,
}

/// A balanced implicit tree stores byte offsets and geometry summaries.
/// Structural edits replace only the affected leaves and their ancestor paths.
struct RowNode {
    left: Option<Box<RowNode>>,
    right: Option<Box<RowNode>>,
    priority: u64,
    count: usize,
    total_bytes: usize,
    total_chars: usize,
    chars: usize,
    bytes: usize,
    own: Arc<egui::Galley>,
    metrics: Arc<egui::Galley>,
}
struct RowIndex {
    root: Option<Box<RowNode>>,
    empty: Arc<LayoutJob>,
    pixels: f32,
}
impl RowIndex {
    fn new(lines: &[Arc<Line>], jobs: &[Arc<LayoutJob>], pixels: f32) -> Self {
        let mut index = Self {
            root: None,
            empty: Arc::new(LayoutJob {
                round_output_to_gui: false,
                ..Default::default()
            }),
            pixels,
        };
        let rows: Vec<_> = lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                (
                    jobs[i].text.len() + usize::from(i + 1 < lines.len()),
                    line.clone(),
                )
            })
            .collect();
        index.root = index.build(&rows, 0);
        index
    }
    fn leaf(line: &Line) -> Arc<egui::Galley> {
        let mut metrics = (*line.metrics).clone();
        metrics.mesh_bounds = line.galley.mesh_bounds;
        metrics.num_vertices = line.galley.num_vertices;
        metrics.num_indices = line.galley.num_indices;
        Arc::new(metrics)
    }
    fn build(&self, rows: &[(usize, Arc<Line>)], depth: u32) -> Option<Box<RowNode>> {
        if rows.is_empty() {
            return None;
        }
        let middle = rows.len() / 2;
        static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
        let priority = ((255 - depth.min(255)) as u64) << 56
            | (egui::util::hash(serial) & 0x00ff_ffff_ffff_ffff);
        let own = Self::leaf(&rows[middle].1);
        let mut node = Box::new(RowNode {
            left: self.build(&rows[..middle], depth + 1),
            right: self.build(&rows[middle + 1..], depth + 1),
            priority,
            count: 1,
            total_bytes: rows[middle].0,
            total_chars: 0,
            chars: rows[middle].1.galley.job.text.chars().count()
                + usize::from(rows[middle].0 > rows[middle].1.galley.job.text.len()),
            bytes: rows[middle].0,
            metrics: own.clone(),
            own,
        });
        self.merge_summary(&mut node);
        Some(node)
    }
    fn count(node: &Option<Box<RowNode>>) -> usize {
        node.as_ref().map_or(0, |n| n.count)
    }
    fn bytes(node: &Option<Box<RowNode>>) -> usize {
        node.as_ref().map_or(0, |n| n.total_bytes)
    }
    fn merge_summary(&self, node: &mut RowNode) {
        let mut parts = Vec::with_capacity(3);
        if let Some(left) = &node.left {
            parts.push(left.metrics.clone());
        }
        parts.push(node.own.clone());
        if let Some(right) = &node.right {
            parts.push(right.metrics.clone());
        }
        let mut metrics = egui::Galley::concat(self.empty.clone(), &parts, self.pixels);
        let mut y = 0.0;
        let mut width: f32 = 0.0;
        for part in parts {
            metrics.mesh_bounds |= part.mesh_bounds.translate(egui::vec2(0.0, y));
            y += part.rect.height();
            width = width.max(part.rect.width());
        }
        metrics.rect = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(width, y));
        node.count = Self::count(&node.left) + Self::count(&node.right) + 1;
        node.total_bytes = Self::bytes(&node.left) + Self::bytes(&node.right) + node.bytes;
        node.total_chars = node.left.as_ref().map_or(0, |n| n.total_chars)
            + node.right.as_ref().map_or(0, |n| n.total_chars)
            + node.chars;
        node.metrics = Arc::new(metrics);
    }
    fn join(
        &self,
        left: Option<Box<RowNode>>,
        right: Option<Box<RowNode>>,
    ) -> Option<Box<RowNode>> {
        match (left, right) {
            (None, other) | (other, None) => other,
            (Some(mut left), Some(mut right)) => {
                if left.priority >= right.priority {
                    left.right = self.join(left.right.take(), Some(right));
                    self.merge_summary(&mut left);
                    Some(left)
                } else {
                    right.left = self.join(Some(left), right.left.take());
                    self.merge_summary(&mut right);
                    Some(right)
                }
            }
        }
    }
    fn split(
        &self,
        node: Option<Box<RowNode>>,
        row: usize,
    ) -> (Option<Box<RowNode>>, Option<Box<RowNode>>) {
        let Some(mut node) = node else {
            return (None, None);
        };
        let left_count = Self::count(&node.left);
        if row <= left_count {
            let (left, right) = self.split(node.left.take(), row);
            node.left = right;
            self.merge_summary(&mut node);
            (left, Some(node))
        } else {
            let (left, right) = self.split(node.right.take(), row - left_count - 1);
            node.right = left;
            self.merge_summary(&mut node);
            (Some(node), right)
        }
    }
    fn splice(&mut self, range: std::ops::Range<usize>, rows: &[(usize, Arc<Line>)]) {
        let root = self.root.take();
        let (left, tail) = self.split(root, range.start);
        let (_, right) = self.split(tail, range.len());
        let inserted = self.build(rows, 0);
        self.root = self.join(self.join(left, inserted), right);
    }
    fn update(&mut self, row: usize, bytes: usize, line: &Line) {
        fn update(index: &RowIndex, node: &mut RowNode, row: usize, bytes: usize, line: &Line) {
            let left = RowIndex::count(&node.left);
            if row < left {
                update(index, node.left.as_mut().unwrap(), row, bytes, line);
            } else if row == left {
                node.bytes = bytes;
                node.chars = line.galley.job.text.chars().count()
                    + usize::from(bytes > line.galley.job.text.len());
                node.own = RowIndex::leaf(line);
            } else {
                update(
                    index,
                    node.right.as_mut().unwrap(),
                    row - left - 1,
                    bytes,
                    line,
                );
            }
            index.merge_summary(node);
        }
        let mut root = self.root.take().unwrap();
        update(self, &mut root, row, bytes, line);
        self.root = Some(root);
    }
    fn metric(&self) -> Arc<egui::Galley> {
        self.root.as_ref().unwrap().metrics.clone()
    }
    fn position(&self, byte: usize, rows: usize) -> (usize, usize) {
        if byte >= Self::bytes(&self.root) {
            return (rows - 1, self.prefix(rows - 1));
        }
        let mut node = self.root.as_ref().unwrap();
        let mut index = 0;
        let mut before = 0;
        loop {
            let left_bytes = Self::bytes(&node.left);
            let left_count = Self::count(&node.left);
            if byte < before + left_bytes {
                node = node.left.as_ref().unwrap();
            } else if byte < before + left_bytes + node.bytes {
                return (index + left_count, before + left_bytes);
            } else {
                before += left_bytes + node.bytes;
                index += left_count + 1;
                node = node.right.as_ref().unwrap();
            }
        }
    }
    fn prefix(&self, row: usize) -> usize {
        self.prefixes(row).0
    }
    fn prefixes(&self, row: usize) -> (usize, usize) {
        let mut node = self.root.as_ref();
        let mut remaining = row;
        let mut bytes = 0;
        let mut chars = 0;
        while let Some(current) = node {
            let count = Self::count(&current.left);
            if remaining <= count {
                node = current.left.as_ref();
            } else {
                bytes += Self::bytes(&current.left) + current.bytes;
                chars += current.left.as_ref().map_or(0, |n| n.total_chars) + current.chars;
                remaining -= count + 1;
                node = current.right.as_ref();
            }
        }
        (bytes, chars)
    }
}

enum Preparation {
    Lines(Arc<Vec<usize>>),
    Geometry(HashMap<u64, Arc<Line>>),
}

#[derive(Default)]
struct Lines {
    entries: HashMap<u64, Arc<Line>>,
    previous: HashMap<u64, Arc<Line>>,
    geometry: HashMap<u64, Arc<Line>>,
    prepared_font: Option<u64>,
    pending: Option<(mpsc::Receiver<Preparation>, Arc<AtomicBool>)>,
    deferred: Vec<egui::Event>,
    preview_lines: Option<Arc<Vec<usize>>>,
    job_key: Option<u64>,
    job: Option<Arc<LayoutJob>>,
    ordered_jobs: Vec<Arc<LayoutJob>>,
    ordered_lines: Vec<Arc<Line>>,
    ordered_keys: Vec<u64>,
    ordered_geometry_keys: Vec<u64>,
    style_generation: u64,
    ordered_styles: Vec<u64>,
    composed: Option<Arc<egui::Galley>>,
    folded_composed: Option<Arc<egui::Galley>>,
    projection_key: Option<u64>,
    visible_rows: Vec<usize>,
    visible_tops: Vec<f32>,
    rows: Option<RowIndex>,
    uniform_metrics: bool,
    uses: HashMap<u64, usize>,
    geometry_uses: HashMap<u64, usize>,
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
        cache.preview_lines = None;
        let definitions = ui.fonts(|fonts| fonts.definitions().clone());
        let font = font.clone();
        let text = text.to_owned();
        let repaint = ui.ctx().clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancelled = cancel.clone();
        let (tx, rx) = mpsc::channel();
        cache.pending = Some((rx, cancel));
        std::thread::spawn(move || {
            let starts = Arc::new(
                std::iter::once(0)
                    .chain(text.match_indices('\n').map(|(i, _)| i + 1))
                    .collect(),
            );
            let _ = tx.send(Preparation::Lines(starts));
            repaint.request_repaint();
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
                    let _ = tx.send(Preparation::Geometry(entries));
                    repaint.request_repaint();
                });
            });
        });
    }
    loop {
        let Some((rx, _)) = &cache.pending else {
            break;
        };
        match rx.try_recv() {
            Ok(Preparation::Lines(starts)) => cache.preview_lines = Some(starts),
            Ok(Preparation::Geometry(geometry)) => {
                let previous = std::mem::replace(&mut cache.geometry, geometry);
                std::thread::spawn(move || drop(previous));
                cache.pending = None;
                cache.preview_lines = None;
                break;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                cache.prepared_font = None;
                cache.pending = None;
                break;
            }
            Err(mpsc::TryRecvError::Empty) => break,
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

pub fn row_offsets(ui: &egui::Ui, id: egui::Id, row: usize) -> Option<(usize, usize)> {
    state(ui, id)
        .lock()
        .unwrap()
        .rows
        .as_ref()
        .map(|rows| rows.prefixes(row))
}

pub fn preview_lines(ui: &egui::Ui, id: egui::Id) -> Option<Arc<Vec<usize>>> {
    state(ui, id).lock().unwrap().preview_lines.clone()
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
    Some(paint_viewport(ui, &mut cache, viewport, folds))
}

fn paint_viewport(
    ui: &egui::Ui,
    cache: &mut Lines,
    viewport: Option<(i32, i32)>,
    folds: &[&crate::folding::Region],
) -> Arc<egui::Galley> {
    let job = cache.job.clone().unwrap();
    project_rows(cache, folds);
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
    let mut changed = Vec::new();
    for index in start..end {
        let i = cache.visible_rows[index];
        if cache.ordered_styles[i] != cache.style_generation {
            let start = cache.rows.as_ref().unwrap().prefix(i);
            let end = start + cache.ordered_jobs[i].text.len();
            let first = job.sections.partition_point(|s| s.byte_range.end <= start);
            let mut sections = Vec::new();
            for section in &job.sections[first..] {
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
            if sections.is_empty()
                && let Some(section) = job.sections.get(first.saturating_sub(1))
            {
                sections.push(LayoutSection {
                    leading_space: 0.0,
                    byte_range: 0..0,
                    format: section.format.clone(),
                });
            }
            let mut line_job = (*cache.ordered_jobs[i]).clone();
            line_job.sections = sections;
            let key = egui::util::hash((&line_job, ui.ctx().pixels_per_point().to_bits()));
            let old = cache.ordered_keys[i];
            if let Some(count) = cache.uses.get_mut(&old) {
                *count -= 1;
                if *count == 0 {
                    cache.uses.remove(&old);
                    cache.entries.remove(&old);
                }
            }
            *cache.uses.entry(key).or_default() += 1;
            cache.ordered_keys[i] = key;
            cache.ordered_jobs[i] = Arc::new(line_job);
            cache.ordered_styles[i] = cache.style_generation;
            // Geometry is unchanged; only this visible row needs a fresh mesh.
            let line = cache
                .entries
                .get(&key)
                .filter(|line| line.painted)
                .cloned()
                .unwrap_or_else(|| Arc::new(make_line(ui, (*cache.ordered_jobs[i]).clone())));
            cache.ordered_lines[i] = line.clone();
            cache.entries.insert(key, line.clone());
            let bytes =
                cache.ordered_jobs[i].text.len() + usize::from(i + 1 < cache.ordered_jobs.len());
            cache.rows.as_mut().unwrap().update(i, bytes, &line);
            changed.push(i);
        }
        if !cache.ordered_lines[i].painted {
            let key = cache.ordered_keys[i];
            let line = cache
                .entries
                .get(&key)
                .filter(|line| line.painted)
                .cloned()
                .unwrap_or_else(|| Arc::new(make_line(ui, (*cache.ordered_jobs[i]).clone())));
            cache.ordered_lines[i] = line.clone();
            cache.entries.insert(key, line.clone());
            let bytes =
                cache.ordered_jobs[i].text.len() + usize::from(i + 1 < cache.ordered_jobs.len());
            if let Some(rows) = &mut cache.rows {
                rows.update(i, bytes, &line);
            }
            changed.push(i);
        }
    }
    if cache.composed.is_none() {
        cache.composed = Some(compose(
            job,
            &cache.ordered_lines,
            ui.ctx().pixels_per_point(),
        ));
        cache.folded_composed = None;
    } else if !changed.is_empty() {
        #[cfg(test)]
        VIEWPORT_PATCHES.fetch_add(1, Ordering::Relaxed);
        let lines = &cache.ordered_lines;
        let replacements: Vec<_> = changed.iter().map(|i| (*i, lines[*i].clone())).collect();
        let count = cache.ordered_lines.len();
        let Lines {
            composed,
            folded_composed,
            ..
        } = cache;
        for target in [composed, folded_composed].into_iter().flatten() {
            let galley = Arc::make_mut(target);
            for (i, line) in &replacements {
                let replacement = if *i + 1 == count {
                    &line.galley.rows[0]
                } else {
                    &line.continued[0]
                };
                let row = &mut galley.rows[*i];
                if row.size.y == 0.0 {
                    continue;
                }
                galley.num_vertices = galley.num_vertices - row.visuals.mesh.vertices.len()
                    + replacement.visuals.mesh.vertices.len();
                galley.num_indices = galley.num_indices - row.visuals.mesh.indices.len()
                    + replacement.visuals.mesh.indices.len();
                row.row = replacement.row.clone();
                galley.mesh_bounds |= row.visuals.mesh_bounds.translate(row.pos.to_vec2());
            }
        }
    }
    composed_view(cache, folds)
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
    mut job: LayoutJob,
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
    if folds.is_empty() && update_plain_rows(ui, &mut cache, &mut job, job_key, viewport) {
        #[cfg(test)]
        EDIT_LAYOUTS.fetch_add(1, Ordering::Relaxed);
        return cache.composed.as_ref().unwrap().clone();
    }
    // Syntax results change colors without changing row geometry. Install the
    // new source job once and color rows only as they enter the viewport.
    if cache.uniform_metrics
        && cache.rows.is_some()
        && cache.composed.is_some()
        && let Some(previous) = &cache.job
        && job.text == previous.text
        && job.wrap == previous.wrap
        && job.first_row_min_height == previous.first_row_min_height
        && job.round_output_to_gui == previous.round_output_to_gui
        && cache.composed.as_ref().unwrap().pixels_per_point == ui.ctx().pixels_per_point()
        && let Some(first) = previous.sections.first()
        && job
            .sections
            .iter()
            .all(|s| s.leading_space == 0.0 && same_metrics(&s.format, &first.format))
        && previous.sections.iter().all(|s| s.leading_space == 0.0)
    {
        let job = Arc::new(job);
        cache.job = Some(job.clone());
        cache.job_key = Some(job_key);
        cache.style_generation = cache.style_generation.wrapping_add(1);
        let Lines {
            composed,
            folded_composed,
            ..
        } = &mut *cache;
        for target in [composed, folded_composed].into_iter().flatten() {
            Arc::make_mut(target).job = job.clone();
        }
        return paint_viewport(ui, &mut cache, viewport, folds);
    }
    #[cfg(test)]
    FULL_LAYOUTS.fetch_add(1, Ordering::Relaxed);
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
    cache.uniform_metrics = job.sections.first().is_some_and(|first| {
        job.sections
            .iter()
            .all(|section| same_metrics(&section.format, &first.format))
    });
    cache.job = Some(job.clone());
    cache.job_key = Some(job_key);
    cache.ordered_styles = vec![cache.style_generation; lines.len()];
    cache.ordered_lines = lines;
    cache.ordered_jobs = jobs;
    cache.ordered_keys = keys;
    cache.ordered_geometry_keys = geometry_keys;
    cache.uses.clear();
    cache.geometry_uses.clear();
    let keys = cache.ordered_keys.clone();
    let geometry = cache.ordered_geometry_keys.clone();
    for key in keys {
        *cache.uses.entry(key).or_default() += 1;
    }
    for key in geometry {
        *cache.geometry_uses.entry(key).or_default() += 1;
    }
    cache.rows = Some(RowIndex::new(
        &cache.ordered_lines,
        &cache.ordered_jobs,
        pixels,
    ));
    cache.projection_key = None;
    project_rows(&mut cache, folds);
    cache.composed = Some(compose(job, &cache.ordered_lines, pixels));
    cache.folded_composed = None;
    composed_view(&mut cache, folds)
}

fn same_metrics(a: &egui::TextFormat, b: &egui::TextFormat) -> bool {
    a.font_id == b.font_id
        && a.line_height == b.line_height
        && a.extra_letter_spacing == b.extra_letter_spacing
        && a.valign == b.valign
}

/// Typing and structural edits split only the affected rows. Egui's flat row
/// positions shift after inserted/deleted lines, but jobs, hashes, summaries and
/// font geometry of unchanged lines are retained.
fn update_plain_rows(
    ui: &egui::Ui,
    cache: &mut Lines,
    job: &mut LayoutJob,
    key: u64,
    viewport: Option<(i32, i32)>,
) -> bool {
    let Some(previous) = &cache.job else {
        return false;
    };
    if cache.composed.is_none()
        || cache.rows.is_none()
        || job.sections.len() != 1
        || !cache.uniform_metrics
        || previous.sections.is_empty()
        || !same_metrics(&job.sections[0].format, &previous.sections[0].format)
        || job.wrap != previous.wrap
        || job.first_row_min_height != previous.first_row_min_height
        || previous.wrap.max_width.is_finite()
        || job.round_output_to_gui != previous.round_output_to_gui
        || cache.composed.as_ref().unwrap().pixels_per_point != ui.ctx().pixels_per_point()
        || job.sections[0].leading_space != previous.sections[0].leading_space
    {
        return false;
    }
    if job.text == previous.text {
        if previous.sections.len() != 1 || job.sections[0].format != previous.sections[0].format {
            return false;
        }
        cache.job_key = Some(key);
        return true;
    }
    let old = &previous.text;
    let new = &job.text;
    let mut prefix = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(prefix) || !new.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let mut suffix = old[prefix..]
        .bytes()
        .rev()
        .zip(new[prefix..].bytes().rev())
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix) {
        suffix -= 1;
    }
    let rows = cache.rows.as_ref().unwrap();
    let (first, start) = rows.position(prefix, cache.ordered_lines.len());
    let (last, _) = rows.position(old.len() - suffix, cache.ordered_lines.len());
    let end = rows.prefix(last + 1).min(old.len());
    let Some(new_end) = end.checked_add_signed(new.len() as isize - old.len() as isize) else {
        return false;
    };
    if new_end > new.len() || !new.is_char_boundary(new_end) {
        return false;
    }
    let mut texts: Vec<_> = new[start..new_end]
        .split_inclusive('\n')
        .map(|line| line.strip_suffix('\n').unwrap_or(line))
        .collect();
    if last + 1 == cache.ordered_lines.len()
        && new_end == new.len()
        && (new.is_empty() || new.ends_with('\n'))
    {
        texts.push("");
    }
    let total = cache.ordered_lines.len() - (last + 1 - first) + texts.len();
    let pixels = ui.ctx().pixels_per_point();
    let mut replacements = Vec::new();
    let mut paint_y = cache.composed.as_ref().unwrap().rows[first].pos.y;
    let existing: HashMap<_, _> = (first..last + 1)
        .map(|i| (cache.ordered_jobs[i].text.as_str(), i))
        .collect();
    for (offset, text) in texts.into_iter().enumerate() {
        let i = first + offset;
        if let Some(old) = existing.get(text).copied()
            && cache.ordered_jobs[old].first_row_min_height
                == if i == 0 {
                    job.first_row_min_height
                } else {
                    0.0
                }
        {
            let mut line = cache.ordered_lines[old].clone();
            let height = line.galley.rect.height();
            if !line.painted
                && viewport.is_none_or(|(top, bottom)| {
                    paint_y + height >= top as f32 - height && paint_y <= bottom as f32 + height
                })
            {
                line = Arc::new(make_line(ui, (*cache.ordered_jobs[old]).clone()));
            }
            paint_y += height;
            replacements.push((
                i,
                cache.ordered_jobs[old].clone(),
                cache.ordered_keys[old],
                cache.ordered_geometry_keys[old],
                line,
            ));
            continue;
        }
        let line_job = Arc::new(LayoutJob {
            text: text.into(),
            sections: vec![LayoutSection {
                leading_space: if i == 0 {
                    job.sections[0].leading_space
                } else {
                    0.0
                },
                byte_range: 0..text.len(),
                format: job.sections[0].format.clone(),
            }],
            first_row_min_height: if i == 0 {
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
        let style = egui::util::hash((line_job.as_ref(), pixels.to_bits()));
        let geometry = geometry_key(&line_job, pixels);
        let mut line = cache
            .entries
            .get(&style)
            .cloned()
            .unwrap_or_else(|| Arc::new(make_line(ui, (*line_job).clone())));
        let height = line.galley.rect.height();
        if !line.painted
            && viewport.is_none_or(|(top, bottom)| {
                paint_y + height >= top as f32 - height && paint_y <= bottom as f32 + height
            })
        {
            line = Arc::new(make_line(ui, (*line_job).clone()));
        }
        paint_y += height;
        if line.galley.rows.len() != 1 {
            return false;
        }
        replacements.push((i, line_job, style, geometry, line));
    }
    let old_height: f32 = cache.ordered_lines[first..last + 1]
        .iter()
        .map(|line| line.galley.rect.height())
        .sum();
    for i in first..last + 1 {
        let style = cache.ordered_keys[i];
        let geometry = cache.ordered_geometry_keys[i];
        if let Some(count) = cache.uses.get_mut(&style) {
            *count -= 1;
            if *count == 0 {
                cache.entries.remove(&style);
                cache.uses.remove(&style);
            }
        }
        if let Some(count) = cache.geometry_uses.get_mut(&geometry) {
            *count -= 1;
            if *count == 0 {
                cache.geometry.remove(&geometry);
                cache.geometry_uses.remove(&geometry);
            }
        }
    }
    for (_, _, style, geometry, line) in &replacements {
        *cache.uses.entry(*style).or_default() += 1;
        *cache.geometry_uses.entry(*geometry).or_default() += 1;
        cache.entries.insert(*style, line.clone());
        cache.geometry.insert(*geometry, line.clone());
    }
    let rows: Vec<_> = replacements
        .iter()
        .map(|(i, job, _, _, line)| (job.text.len() + usize::from(*i + 1 < total), line.clone()))
        .collect();
    cache.rows.as_mut().unwrap().splice(first..last + 1, &rows);
    cache
        .ordered_keys
        .splice(first..last + 1, replacements.iter().map(|r| r.2));
    cache
        .ordered_geometry_keys
        .splice(first..last + 1, replacements.iter().map(|r| r.3));
    cache
        .ordered_jobs
        .splice(first..last + 1, replacements.iter().map(|r| r.1.clone()));
    cache
        .ordered_lines
        .splice(first..last + 1, replacements.iter().map(|r| r.4.clone()));
    cache.ordered_styles.splice(
        first..last + 1,
        std::iter::repeat_n(cache.style_generation, replacements.len()),
    );
    let job = Arc::new(std::mem::take(job));
    let metrics = cache.rows.as_ref().unwrap().metric();
    let mut old = cache.composed.take().unwrap();
    let old = Arc::make_mut(&mut old);
    let mut galley = egui::Galley::concat(job.clone(), &[metrics.clone()], pixels);
    galley.rect = metrics.rect;
    galley.mesh_bounds = metrics.mesh_bounds;
    galley.rows = std::mem::take(&mut old.rows);
    let mut y = galley.rows[first].pos.y;
    let mut placed = Vec::new();
    let mut new_height = 0.0;
    for (i, _, _, _, line) in &replacements {
        let mut row = if *i + 1 == total {
            line.galley.rows[0].clone()
        } else {
            line.continued[0].clone()
        };
        row.pos.y = y;
        y += row.size.y;
        new_height += row.size.y;
        placed.push(row);
    }
    let count = placed.len();
    galley.rows.splice(first..last + 1, placed);
    let shift = new_height - old_height;
    if shift != 0.0 {
        for row in &mut galley.rows[first + count..] {
            row.pos.y += shift;
        }
    }
    if job.round_output_to_gui {
        galley.rect = galley.rect.round_ui();
    }
    cache.job = Some(job);
    cache.job_key = Some(key);
    cache.composed = Some(Arc::new(galley));
    cache.folded_composed = None;
    if count != last + 1 - first || shift != 0.0 {
        cache.projection_key = None;
        project_rows(cache, &[]);
    }
    true
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
    fn structural_edits_match_native_layout_for_unicode_newlines_and_width_changes() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("structural-edits");
        let mut source = "first\nunchanged 猫\nlast".to_owned();
        let mut random = 123u64;
        for step in 0..120 {
            let _ = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let job = LayoutJob::simple(
                        source.clone(),
                        egui::FontId::monospace(13.0),
                        egui::Color32::WHITE,
                        f32::INFINITY,
                    );
                    let full = ui.fonts_mut(|fonts| fonts.layout_job(job.clone()));
                    let incremental = layout(ui, id, job);
                    assert_eq!(
                        full.rows, incremental.rows,
                        "rows after edit {step}: {source:?}"
                    );
                    assert_eq!(full.rect, incremental.rect, "bounds after edit {step}");
                    assert_eq!(full.intrinsic_size(), incremental.intrinsic_size());
                    assert_eq!(full.num_vertices, incremental.num_vertices);
                    assert_eq!(full.mesh_bounds, incremental.mesh_bounds);
                    let mut byte = 0;
                    let mut chars = 0;
                    for (row, line) in source.split('\n').enumerate() {
                        assert_eq!(
                            row_offsets(ui, id, row),
                            Some((byte, chars)),
                            "indexed row offsets after edit {step}"
                        );
                        byte += line.len() + 1;
                        chars += line.chars().count() + 1;
                    }
                    for cursor in 0..=source.chars().count() {
                        assert_eq!(
                            full.pos_from_cursor(egui::text::CCursor::new(cursor)),
                            incremental.pos_from_cursor(egui::text::CCursor::new(cursor))
                        );
                    }
                });
            });
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let boundaries: Vec<_> = source
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(source.len()))
                .collect();
            let from = random as usize % boundaries.len();
            if step % 3 == 0 && from + 1 < boundaries.len() {
                source.replace_range(
                    boundaries[from]..boundaries[(from + 2).min(boundaries.len() - 1)],
                    "",
                );
            } else {
                source.insert_str(
                    boundaries[from],
                    ["猫", "e\u{301}", "\n", "foo\nbar", "\t", "λ"][step % 6],
                );
            }
        }
    }

    #[test]
    fn syntax_updates_color_only_visible_rows_and_refresh_on_scroll() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("lazy-syntax");
        let source = (0..1000).map(|i| format!("line {i}\n")).collect::<String>();
        let mut pointer = 0;
        for step in 0..3 {
            let _ = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let mut job = LayoutJob::simple(
                        source.clone(),
                        egui::FontId::monospace(13.0),
                        if step == 0 {
                            egui::Color32::WHITE
                        } else {
                            egui::Color32::RED
                        },
                        f32::INFINITY,
                    );
                    // Exercise multiple syntax sections with unchanged geometry.
                    if step > 0 {
                        crate::chrome::mark_matches(&mut job, "line");
                    }
                    let viewport = if step == 2 {
                        Some((9000, 9300))
                    } else {
                        Some((0, 300))
                    };
                    let galley = if step == 2 {
                        repaint_viewport(ui, id, 2, viewport).unwrap()
                    } else {
                        layout_visible_keyed(ui, id, job, viewport, step + 1)
                    };
                    if step == 0 {
                        pointer = galley.rows.as_ptr() as usize;
                    } else {
                        assert_eq!(pointer, galley.rows.as_ptr() as usize);
                    }
                    let cache = state(ui, id);
                    let cache = cache.lock().unwrap();
                    let colored = cache
                        .ordered_styles
                        .iter()
                        .filter(|g| **g == cache.style_generation)
                        .count();
                    if step == 1 {
                        assert!(colored > 0 && colored < 40, "{colored}");
                        assert_ne!(cache.ordered_styles[600], cache.style_generation);
                    } else if step == 2 {
                        assert_eq!(cache.ordered_styles[600], cache.style_generation);
                        let expected = make_line(ui, (*cache.ordered_jobs[600]).clone());
                        assert_eq!(
                            galley.rows[600].visuals.mesh,
                            expected.galley.rows[0].visuals.mesh
                        );
                    }
                });
            });
        }
    }

    #[test]
    fn typing_preserves_unmodified_syntax_rows_until_new_analysis_arrives() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("retain-syntax");
        let mut unchanged = None;
        for (step, source) in [
            "first\nkeep\nlast",
            "first!\nkeep\nlast",
            "first!\nnew\nkeep\nlast",
        ]
        .iter()
        .enumerate()
        {
            let _ = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let mut job = LayoutJob::simple(
                        (*source).into(),
                        egui::FontId::monospace(13.0),
                        egui::Color32::WHITE,
                        f32::INFINITY,
                    );
                    if step == 0 {
                        crate::chrome::mark_matches(&mut job, "keep");
                    }
                    let galley = layout(ui, id, job);
                    let row = if step == 2 { 2 } else { 1 };
                    if let Some(old) = &unchanged {
                        assert!(Arc::ptr_eq(old, &galley.rows[row].row));
                    }
                    unchanged = Some(galley.rows[row].row.clone());
                    assert_eq!(galley.job.text, *source);
                });
            });
        }
    }

    #[test]
    fn scrolling_patches_the_existing_row_vector_when_no_snapshot_is_retained() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("patch-rows");
        let source = (0..2000).map(|i| format!("row {i}\n")).collect::<String>();
        let font = egui::FontId::monospace(13.0);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut pointer = None;
        while pointer.is_none() {
            let _ = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    if prepare(ui, id, &source, &font, egui::Color32::WHITE) {
                        let galley = layout_visible(
                            ui,
                            id,
                            LayoutJob::simple(
                                source.clone(),
                                font.clone(),
                                egui::Color32::WHITE,
                                f32::INFINITY,
                            ),
                            Some((0, 300)),
                        );
                        pointer = Some(galley.rows.as_ptr() as usize);
                    }
                });
            });
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let galley = layout_visible(
                    ui,
                    id,
                    LayoutJob::simple(
                        source.clone(),
                        font.clone(),
                        egui::Color32::WHITE,
                        f32::INFINITY,
                    ),
                    Some((10000, 10300)),
                );
                assert_eq!(
                    Some(galley.rows.as_ptr() as usize),
                    pointer,
                    "scrolling must not recompose every row"
                );
                assert_eq!(
                    galley.num_vertices,
                    galley
                        .rows
                        .iter()
                        .map(|row| row.visuals.mesh.vertices.len())
                        .sum::<usize>()
                );
            });
        });
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
