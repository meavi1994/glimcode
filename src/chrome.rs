use eframe::egui::{self, Color32, FontId, RichText, Stroke, text::LayoutJob};
use std::path::Path;

pub const EDITOR: Color32 = Color32::from_rgb(24, 29, 39);
pub const PANEL: Color32 = Color32::from_rgb(20, 25, 34);
pub const HEADER: Color32 = Color32::from_rgb(32, 40, 53);
pub const BORDER: Color32 = Color32::from_rgb(42, 51, 65);
pub const TEXT: Color32 = Color32::from_rgb(222, 225, 233);
pub const MUTED: Color32 = Color32::from_rgb(148, 154, 169);
pub const BLUE: Color32 = Color32::from_rgb(110, 168, 254);
pub const SELECTED: Color32 = Color32::from_rgb(32, 50, 73);
pub const TABS: Color32 = Color32::from_rgb(16, 21, 29);

pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.families.insert(
        egui::FontFamily::Name("heading".into()),
        fonts.families[&egui::FontFamily::Proportional].clone(),
    );
    for (name, family, candidates) in [
        (
            "heading",
            egui::FontFamily::Name("heading".into()),
            vec![
                "/Applications/IntelliJ IDEA.app/Contents/jbr/Contents/Home/lib/fonts/Inter-SemiBold.otf",
                "/Applications/IntelliJ IDEA CE.app/Contents/jbr/Contents/Home/lib/fonts/Inter-SemiBold.otf",
                "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
            ],
        ),
        (
            "ui",
            egui::FontFamily::Proportional,
            vec![
                "/Applications/IntelliJ IDEA.app/Contents/jbr/Contents/Home/lib/fonts/Inter-Regular.otf",
                "/Applications/IntelliJ IDEA CE.app/Contents/jbr/Contents/Home/lib/fonts/Inter-Regular.otf",
                "/System/Library/Fonts/SFNS.ttf",
            ],
        ),
        (
            "code",
            egui::FontFamily::Monospace,
            vec![
                "/Applications/IntelliJ IDEA.app/Contents/jbr/Contents/Home/lib/fonts/JetBrainsMono-Regular.ttf",
                "/Applications/IntelliJ IDEA CE.app/Contents/jbr/Contents/Home/lib/fonts/JetBrainsMono-Regular.ttf",
                "/System/Library/Fonts/Menlo.ttc",
            ],
        ),
    ] {
        if let Some(bytes) = candidates.iter().find_map(|path| std::fs::read(path).ok()) {
            fonts.font_data.insert(
                name.into(),
                std::sync::Arc::new(egui::FontData::from_owned(bytes)),
            );
            fonts
                .families
                .entry(family)
                .or_default()
                .insert(0, name.into());
        }
    }
    ctx.set_fonts(fonts);
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.set_visuals(egui::Visuals::dark());
    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 5.0);
        s.animation_time = 0.12;
        s.spacing.button_padding = egui::vec2(10.0, 6.0);
        s.spacing.indent = 16.0;
        s.interaction.resize_grab_radius_side = 4.0;
        s.interaction.resize_grab_radius_corner = 14.0;
        s.visuals.resize_corner_size = 16.0;
        s.spacing.interact_size.y = 28.0;
        s.visuals.panel_fill = PANEL;
        s.visuals.window_fill = PANEL;
        s.visuals.extreme_bg_color = EDITOR;
        s.visuals.faint_bg_color = HEADER;
        s.visuals.override_text_color = Some(TEXT);
        s.visuals.selection.bg_fill = SELECTED;
        s.visuals.selection.stroke = Stroke::new(1.0_f32, TEXT);
        s.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
        s.visuals.widgets.inactive.weak_bg_fill = HEADER;
        s.visuals.widgets.inactive.bg_fill = HEADER;
        s.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
        s.visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
        s.visuals.widgets.hovered.bg_fill = HEADER;
        s.visuals.widgets.active.bg_fill = SELECTED;
        s.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(7);
        s.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(7);
        s.visuals.widgets.active.corner_radius = egui::CornerRadius::same(7);
        s.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(46, 49, 59);
        s.visuals.widgets.active.weak_bg_fill = SELECTED;
        s.visuals.window_corner_radius = egui::CornerRadius::same(10);
        s.visuals.window_stroke = Stroke::new(1.0_f32, BORDER);
        s.visuals.hyperlink_color = BLUE;
        s.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, BLUE.gamma_multiply(0.5));
        s.visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, BLUE);
        s.text_styles
            .insert(egui::TextStyle::Body, FontId::proportional(13.0));
        s.text_styles
            .insert(egui::TextStyle::Button, FontId::proportional(13.0));
        s.text_styles
            .insert(egui::TextStyle::Monospace, FontId::monospace(13.0));
    });
}

pub fn file_kind(path: &Path) -> (&'static str, Color32) {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "java" => ("J", Color32::from_rgb(211, 151, 110)),
        "kt" | "kts" => ("Kt", Color32::from_rgb(179, 131, 219)),
        "go" => ("Go", Color32::from_rgb(112, 191, 210)),
        "rs" => ("Rs", Color32::from_rgb(211, 151, 110)),
        "json" | "toml" | "yaml" | "yml" => ("{}", Color32::from_rgb(179, 131, 219)),
        "md" | "markdown" => ("M↓", BLUE),
        "ts" | "tsx" => ("TS", Color32::from_rgb(85, 151, 225)),
        "js" | "jsx" => ("JS", Color32::from_rgb(219, 194, 110)),
        "html" | "xml" => ("<>", Color32::from_rgb(92, 153, 106)),
        "csv" | "tsv" => ("CSV", Color32::from_rgb(101, 177, 139)),
        "py" => ("Py", Color32::from_rgb(111, 162, 213)),
        _ => ("≡", MUTED),
    }
}

pub fn file_icon(path: &Path) -> RichText {
    let (icon, color) = file_kind(path);
    RichText::new(icon).monospace().size(12.0).color(color)
}

/// Lay out `text` with every occurrence of `query` highlighted.
///
/// The match ignores case, because the file picker filters case-insensitively
/// as well: a term that keeps a row visible must also be visible in it.
pub fn highlighted(text: &str, query: &str, size: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    let normal = egui::TextFormat::simple(FontId::proportional(size), TEXT);
    let highlight = egui::TextFormat {
        background: Color32::from_rgb(186, 151, 70),
        color: Color32::BLACK,
        ..normal.clone()
    };
    if query.is_empty() {
        job.append(text, 0.0, normal);
        return job;
    }
    let needle: Vec<char> = query.chars().map(lower_char).collect();
    let haystack: Vec<char> = text.chars().collect();
    let lowered: Vec<char> = haystack
        .iter()
        .map(|character| lower_char(*character))
        .collect();
    // Byte offset of every character, so the original text can be sliced.
    let mut offsets: Vec<usize> = Vec::with_capacity(haystack.len() + 1);
    let mut offset = 0;
    for character in &haystack {
        offsets.push(offset);
        offset += character.len_utf8();
    }
    offsets.push(offset);
    let mut index = 0;
    let mut cursor = 0;
    while index + needle.len() <= haystack.len() {
        let matched = lowered[index..index + needle.len()] == needle[..];
        if matched {
            let start = offsets[index];
            let end = offsets[index + needle.len()];
            job.append(&text[cursor..start], 0.0, normal.clone());
            job.append(&text[start..end], 0.0, highlight.clone());
            cursor = end;
            index += needle.len();
        } else {
            index += 1;
        }
    }
    job.append(&text[cursor..], 0.0, normal);
    job
}

/// Drop the leading whitespace of a job, keeping the section ranges aligned
/// with the shortened text.
pub fn trim_start(job: &mut LayoutJob) {
    let start = job.text.len() - job.text.trim_start().len();
    if start == 0 {
        return;
    }
    job.text = job.text[start..].to_owned();
    job.sections
        .retain(|section| section.byte_range.end > start);
    for section in &mut job.sections {
        section.byte_range.start = section.byte_range.start.saturating_sub(start);
        section.byte_range.end = section.byte_range.end.saturating_sub(start);
    }
}

/// A lowercased search term, prepared once and matched without allocating.
///
/// The explorer and the grid ask the same question for every row, so the
/// term is lowered once instead of once per row.
#[derive(Clone, Default)]
pub struct CaseFilter {
    needle: Vec<char>,
    /// KMP failure table: `failure[i]` is the length of the longest proper
    /// prefix of `needle[..=i]` that is also a suffix of it.
    failure: Vec<usize>,
}

/// The lowercase form of a character, keeping one character per character.
fn lower_char(character: char) -> char {
    character.to_lowercase().next().unwrap_or(character)
}

impl CaseFilter {
    pub fn new(needle: &str) -> Self {
        let needle: Vec<char> = needle.chars().map(lower_char).collect();
        let mut failure = vec![0; needle.len()];
        let mut matched = 0;
        for index in 1..needle.len() {
            while matched > 0 && needle[index] != needle[matched] {
                matched = failure[matched - 1];
            }
            if needle[index] == needle[matched] {
                matched += 1;
            }
            failure[index] = matched;
        }
        Self { needle, failure }
    }

    pub fn is_empty(&self) -> bool {
        self.needle.is_empty()
    }

    /// Whether the text contains the term, ignoring case.
    ///
    /// The scan is the Knuth-Morris-Pratt algorithm: a mismatch falls back to
    /// the longest prefix that is still a suffix, so overlapping terms such
    /// as `aab` inside `aaab` are found. A plain "restart at the first
    /// character" scan misses them and hides rows that do match.
    pub fn matches(&self, text: &str) -> bool {
        let needle = &self.needle;
        if needle.is_empty() {
            return true;
        }
        let mut matched = 0;
        for character in text.chars() {
            let character = lower_char(character);
            while matched > 0 && character != needle[matched] {
                matched = self.failure[matched - 1];
            }
            if character == needle[matched] {
                matched += 1;
                if matched == needle.len() {
                    return true;
                }
            }
        }
        false
    }
}

pub fn popup_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(14)
        .inner_margin(24)
        .shadow(egui::epaint::Shadow {
            offset: [0, 16],
            blur: 48,
            spread: 1,
            color: Color32::from_black_alpha(150),
        })
}

pub fn mark_matches(job: &mut LayoutJob, query: &str) {
    if query.is_empty() {
        return;
    }
    let matches: Vec<_> = job
        .text
        .match_indices(query)
        .map(|(index, _)| index..index + query.len())
        .collect();
    let mut sections = vec![];
    let mut first_match = 0;
    for section in &job.sections {
        while first_match < matches.len() && matches[first_match].end <= section.byte_range.start {
            first_match += 1;
        }
        let mut start = section.byte_range.start;
        for range in matches[first_match..]
            .iter()
            .take_while(|r| r.start < section.byte_range.end)
        {
            let from = range.start.max(start);
            let to = range.end.min(section.byte_range.end);
            if start < from {
                let mut part = section.clone();
                part.byte_range = start..from;
                sections.push(part);
            }
            let mut part = section.clone();
            part.byte_range = from..to;
            part.format.background = Color32::from_rgb(77, 73, 39);
            part.format.underline = Stroke::new(1.0_f32, Color32::from_rgb(170, 145, 65));
            sections.push(part);
            start = to;
        }
        if start < section.byte_range.end {
            let mut part = section.clone();
            part.byte_range = start..section.byte_range.end;
            sections.push(part);
        }
    }
    job.sections = sections;
}

/// Folder rows reserve geometry for the icon instead of padding labels with spaces.
pub fn folder_header<R>(
    ui: &mut egui::Ui,
    id: egui::Id,
    name: &str,
    default_open: bool,
    open: Option<bool>,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::Response {
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
        ui.ctx(),
        id,
        default_open,
    );
    if let Some(open) = open {
        state.set_open(open);
    }
    let mut clicked = false;
    let mut header = state.show_header(ui, |ui| {
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 23.0), egui::Sense::click());
        let icon = egui::Rect::from_min_size(rect.min, egui::vec2(23.0, 23.0));
        folder(ui.painter(), icon);
        ui.painter().text(
            egui::pos2(icon.right() + 6.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            name,
            FontId::proportional(13.0),
            TEXT,
        );
        clicked = response.clicked();
        response
    });
    if clicked {
        header.toggle();
    }
    let (_, header_response, _) = header.body(body);
    header_response.inner
}

pub fn folder(painter: &egui::Painter, rect: egui::Rect) {
    folder_colored(painter, rect, TEXT);
}

pub fn folder_colored(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let min = rect.min + egui::vec2(2.0, 5.0);
    let points = vec![
        min,
        min + egui::vec2(6.0, 0.0),
        min + egui::vec2(9.0, 3.0),
        min + egui::vec2(17.0, 3.0),
        min + egui::vec2(17.0, 14.0),
        min + egui::vec2(0.0, 14.0),
        min,
    ];
    painter.add(egui::Shape::line(points, Stroke::new(1.3_f32, color)));
}

pub fn tool_button(ui: &mut egui::Ui, tool: u8, active: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(31.0, 34.0), egui::Sense::click());
    if active || response.hovered() {
        ui.painter()
            .rect_filled(rect, 5.0, if active { SELECTED } else { HEADER });
    }
    if active {
        ui.painter().rect_filled(
            egui::Rect::from_min_size(
                rect.left_top() + egui::vec2(-3.0, 5.0),
                egui::vec2(2.0, rect.height() - 10.0),
            ),
            1,
            BLUE,
        );
    }
    let icon = egui::Rect::from_center_size(rect.center(), egui::vec2(23.0, 25.0));
    let painter = ui.painter();
    let stroke = Stroke::new(1.5_f32, TEXT);
    match tool {
        0 => folder(painter, icon),
        1 => {
            painter.circle_stroke(icon.center() - egui::vec2(2.0, 2.0), 6.0, stroke);
            painter.line_segment(
                [
                    icon.center() + egui::vec2(3.0, 3.0),
                    icon.center() + egui::vec2(9.0, 9.0),
                ],
                stroke,
            );
        }
        3 => {
            painter.rect_stroke(
                icon.shrink2(egui::vec2(1.0, 3.0)),
                2.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            painter.add(egui::Shape::line(
                vec![
                    icon.center() + egui::vec2(-6.0, -4.0),
                    icon.center() + egui::vec2(-2.0, 0.0),
                    icon.center() + egui::vec2(-6.0, 4.0),
                ],
                stroke,
            ));
            painter.line_segment(
                [
                    icon.center() + egui::vec2(1.0, 4.0),
                    icon.center() + egui::vec2(7.0, 4.0),
                ],
                stroke,
            );
        }
        _ => {
            for row in 0..3 {
                let y = icon.top() + 6.0 + row as f32 * 6.0;
                painter.circle_filled(egui::pos2(icon.left() + 3.0, y), 1.5, TEXT);
                painter.line_segment(
                    [
                        egui::pos2(icon.left() + 8.0, y),
                        egui::pos2(icon.right() - 1.0, y),
                    ],
                    stroke,
                );
            }
        }
    }
    response
}

/// Project toolbar: reveal active file, expand folders, collapse folders.
pub fn tree_button(ui: &mut egui::Ui, action: usize) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(23.0, 23.0), egui::Sense::click());
    let c = rect.center();
    let stroke = Stroke::new(1.4_f32, if response.hovered() { BLUE } else { TEXT });
    if response.hovered() {
        ui.painter().rect_filled(rect, 3.0, HEADER);
    }
    if action == 0 {
        ui.painter().circle_stroke(c, 6.0, stroke);
        for (a, b) in [
            ((-9.0, 0.0), (-3.0, 0.0)),
            ((3.0, 0.0), (9.0, 0.0)),
            ((0.0, -9.0), (0.0, -3.0)),
            ((0.0, 3.0), (0.0, 9.0)),
        ] {
            ui.painter()
                .line_segment([c + egui::vec2(a.0, a.1), c + egui::vec2(b.0, b.1)], stroke);
        }
    } else {
        let direction = if action == 1 { -1.0 } else { 1.0 };
        for side in [-1.0, 1.0] {
            let y = side * 4.0;
            ui.painter().add(egui::Shape::line(
                vec![
                    c + egui::vec2(-4.5, y + side * direction * 2.0),
                    c + egui::vec2(0.0, y - side * direction * 2.0),
                    c + egui::vec2(4.5, y + side * direction * 2.0),
                ],
                stroke,
            ));
        }
    }
    response.on_hover_text(["Select opened file", "Expand all", "Collapse all"][action])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_case_filter_finds_overlapping_terms() {
        // A naive scan restarts at the first character on a mismatch and
        // misses this one, which hid rows that do match.
        assert!(CaseFilter::new("aab").matches("aaab"));
        assert!(CaseFilter::new("abab").matches("xxababyy"));
        assert!(CaseFilter::new("abc").matches("ABC"));
        assert!(!CaseFilter::new("abc").matches("abd"));
        assert!(!CaseFilter::new("abc").matches("ab"));
        assert!(CaseFilter::new("").matches("anything"));
    }

    #[test]
    fn the_case_filter_matches_the_text_it_filters() {
        for (needle, text, expected) in [
            ("app", "src/App.rs", true),
            ("app", "src/app.rs", true),
            ("src/app", "src/app.rs", true),
            ("zzz", "src/app.rs", false),
        ] {
            assert_eq!(
                CaseFilter::new(needle).matches(text),
                expected,
                "{needle} in {text}"
            );
        }
    }

    #[test]
    fn the_picker_highlights_the_term_it_filters_with() {
        let job = highlighted("App.rs", "app", 12.0);
        let highlighted_sections = job
            .sections
            .iter()
            .filter(|section| section.format.background != Color32::TRANSPARENT)
            .count();
        assert_eq!(highlighted_sections, 1, "the term must be marked");
        assert!(job.text.contains("App.rs"));
    }
    #[test]
    fn folder_labels_keep_a_fixed_gap_at_wide_sizes_and_display_scales() {
        for scale in [1.0, 1.5, 2.0] {
            let ctx = egui::Context::default();
            ctx.set_pixels_per_point(scale);
            let mut rect = egui::Rect::NOTHING;
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(2560.0, 1440.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        rect = folder_header(
                            ui,
                            egui::Id::new("folder"),
                            "nested folder",
                            true,
                            None,
                            |_| {},
                        )
                        .rect;
                    });
                },
            );
            let label = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == "nested folder" => Some(text),
                    _ => None,
                })
                .unwrap();
            assert!(label.pos.x >= rect.left() + 29.0);
            assert!(label.pos.x + label.galley.size().x < rect.right());
        }
    }

    #[test]
    fn selected_tab_remains_visible_when_the_tab_strip_narrows() {
        let ctx = egui::Context::default();
        install(&ctx);
        let mut time = 0.0;
        let mut frame = |width| {
            time += 0.1;
            let mut clip = egui::Rect::NOTHING;
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(950.0, 500.0),
                    )),
                    time: Some(time),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        egui::ScrollArea::horizontal()
                            .id_salt("resize-tabs-test")
                            .max_width(width)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    clip = ui.clip_rect();
                                    for index in 0..12 {
                                        workspace_tab(
                                            ui,
                                            egui::Id::new(("resize-tab", index)),
                                            &format!("source_{index}.rs"),
                                            TabKind::File(Path::new("source.rs")),
                                            index == 11,
                                            false,
                                            "Source",
                                        );
                                    }
                                });
                            });
                    });
                },
            );
            clip
        };
        for width in [500.0, 250.0] {
            let mut clip = egui::Rect::NOTHING;
            for _ in 0..20 {
                clip = frame(width);
            }
            let body = ctx
                .read_response(egui::Id::new(("resize-tab", 11)))
                .unwrap()
                .rect;
            let close = ctx
                .read_response(egui::Id::new(("resize-tab", 11)).with("close"))
                .unwrap()
                .rect;
            assert!(
                body.left() >= clip.left() - 1.0 && close.right() <= clip.right() + 1.0,
                "active tab and its close button must remain visible at {width}: {body:?}, {close:?}, {clip:?}"
            );
        }
    }

    #[test]
    fn tab_close_is_independent_from_activation_and_supports_keyboard_and_middle_click() {
        let ctx = egui::Context::default();
        install(&ctx);
        let id = egui::Id::new("test-tab");
        let frame = |events| {
            let mut result = None;
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 500.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        result = Some(workspace_tab(
                            ui,
                            id,
                            "Long Unicode 文件 name that needs truncation.rs",
                            TabKind::File(Path::new("file.rs")),
                            false,
                            true,
                            "File path",
                        ));
                    });
                },
            );
            result.unwrap()
        };
        frame(vec![]);
        frame(vec![]);
        let body = ctx.read_response(id).unwrap().rect;
        let close = ctx.read_response(id.with("close")).unwrap().rect;
        assert!(!body.intersects(close));
        for (point, button, closes) in [
            (close.center(), egui::PointerButton::Primary, true),
            (
                body.left_center() + egui::vec2(6.0, 0.0),
                egui::PointerButton::Primary,
                false,
            ),
            (body.center(), egui::PointerButton::Middle, true),
        ] {
            frame(vec![
                egui::Event::PointerMoved(point),
                egui::Event::PointerButton {
                    pos: point,
                    button,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
            let result = frame(vec![egui::Event::PointerButton {
                pos: point,
                button,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }]);
            assert_eq!(result.close, closes);
            assert_eq!(result.activate, !closes);
        }
        ctx.memory_mut(|m| m.request_focus(id.with("close")));
        let result = frame(vec![egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        assert!(result.close);
        assert!(!result.activate);
    }

    #[test]
    fn unicode_matches_preserve_syntax_and_full_text() {
        let mut job = LayoutJob::default();
        job.append(
            "猫",
            0.0,
            egui::TextFormat::simple(FontId::monospace(13.0), BLUE),
        );
        job.append(
            "x 猫x",
            0.0,
            egui::TextFormat::simple(FontId::monospace(13.0), TEXT),
        );
        mark_matches(&mut job, "猫x");
        assert_eq!(
            job.sections
                .iter()
                .map(|s| &job.text[s.byte_range.clone()])
                .collect::<String>(),
            "猫x 猫x"
        );
        assert_eq!(job.sections[0].format.color, BLUE);
        assert_eq!(
            job.sections
                .iter()
                .filter(|s| s.format.background != Color32::TRANSPARENT)
                .count(),
            3
        );
    }
}

/// One final cursor decision for every splitter and floating-window edge.
/// Native hover flags can be obscured by content drawn later in the frame;
/// use the actual resize hit rectangles, with foreground layers taking priority.
pub fn finish_resize_borders(
    ctx: &egui::Context,
    panels: &[(&str, bool)],
    windows: &[egui::Id],
    modal: bool,
) {
    use egui::CursorIcon as C;
    let pointer = ctx.pointer_hover_pos();
    let top = pointer.and_then(|pos| ctx.layer_id_at(pos));
    let mut handles = Vec::new();
    for &(panel, horizontal) in panels {
        let Some(response) = ctx.read_response(egui::Id::new(panel).with("__resize")) else {
            continue;
        };
        let hovered = !modal
            && response.enabled()
            && pointer.is_some_and(|pos| response.rect.contains(pos))
            && top == Some(response.layer_id);
        let active = !modal && response.dragged();
        let rect = response.rect;
        let points = if horizontal {
            [
                egui::pos2(rect.center().x, rect.top()),
                egui::pos2(rect.center().x, rect.bottom()),
            ]
        } else {
            [
                egui::pos2(rect.left(), rect.center().y),
                egui::pos2(rect.right(), rect.center().y),
            ]
        };
        ctx.layer_painter(response.layer_id).line_segment(
            points,
            Stroke::new(
                if active || hovered { 2.0_f32 } else { 1.0_f32 },
                if active {
                    BLUE
                } else if hovered {
                    Color32::from_rgb(112, 124, 149)
                } else {
                    BORDER
                },
            ),
        );
        handles.push((
            response,
            if horizontal {
                C::ResizeHorizontal
            } else {
                C::ResizeVertical
            },
        ));
    }
    for &id in windows {
        let layer = egui::LayerId::new(egui::Order::Middle, id);
        // These are egui Window's edge interaction ids. Use its hit rectangles
        // so the cursor and the area that actually starts a resize always agree.
        let base = egui::Id::new(layer).with("edge_drag");
        for (edge, cursor) in [
            ("left", C::ResizeHorizontal),
            ("right", C::ResizeHorizontal),
            ("top", C::ResizeVertical),
            ("bottom", C::ResizeVertical),
            ("left_top", C::ResizeNwSe),
            ("right_bottom", C::ResizeNwSe),
            ("right_top", C::ResizeNeSw),
            ("left_bottom", C::ResizeNeSw),
        ] {
            if let Some(response) = ctx.read_response(base.with(edge)) {
                handles.push((response, cursor));
            }
        }
    }
    if !modal {
        // Keep the dragged edge's cursor even outside its original hit rectangle.
        let dragged = handles
            .iter()
            .find(|(response, _)| response.enabled() && response.dragged());
        let hovered = handles.iter().rev().find(|(response, _)| {
            response.enabled()
                && pointer.is_some_and(|pos| response.rect.contains(pos))
                && top.is_none_or(|top| {
                    top == response.layer_id || top.order < response.layer_id.order
                })
        });
        if let Some((_, cursor)) = dragged.or(hovered) {
            ctx.set_cursor_icon(*cursor);
        }
    }
}

/// Reset a background edge before drawing modal controls, so controls such as
/// font-size drag values can still choose their own cursor on the modal layer.
pub fn clear_resize_cursor(ctx: &egui::Context) {
    ctx.output_mut(|output| {
        if is_resize_cursor(output.cursor_icon) {
            output.cursor_icon = egui::CursorIcon::Default;
        }
    });
}

pub fn is_resize_cursor(cursor: egui::CursorIcon) -> bool {
    use egui::CursorIcon::*;
    matches!(
        cursor,
        ResizeHorizontal
            | ResizeVertical
            | ResizeNeSw
            | ResizeNwSe
            | ResizeEast
            | ResizeWest
            | ResizeNorth
            | ResizeSouth
            | ResizeNorthEast
            | ResizeNorthWest
            | ResizeSouthEast
            | ResizeSouthWest
            | ResizeColumn
            | ResizeRow
    )
}

pub const DANGER: Color32 = Color32::from_rgb(220, 100, 112);
pub const WARNING: Color32 = Color32::from_rgb(225, 178, 103);

pub fn dialog_typography(ui: &mut egui::Ui) {
    let size = crate::preferences::dialog_font_size(ui.ctx());
    for style in [
        egui::TextStyle::Body,
        egui::TextStyle::Button,
        egui::TextStyle::Small,
    ] {
        ui.style_mut()
            .text_styles
            .insert(style, FontId::proportional(size));
    }
    ui.style_mut()
        .text_styles
        .insert(egui::TextStyle::Monospace, FontId::monospace(size));
    ui.style_mut()
        .text_styles
        .insert(egui::TextStyle::Heading, FontId::proportional(size + 8.0));
}

pub fn dialog_style(ui: &mut egui::Ui, width: f32) {
    dialog_typography(ui);
    ui.set_width(width.min((ui.ctx().content_rect().width() - 80.0).max(260.0)));
    ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
    ui.spacing_mut().button_padding = egui::vec2(14.0, 8.0);
    ui.spacing_mut().interact_size.y =
        (crate::preferences::dialog_font_size(ui.ctx()) + 20.0).max(34.0);
    ui.visuals_mut().widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    ui.visuals_mut().widgets.inactive.corner_radius = egui::CornerRadius::same(7);
    ui.visuals_mut().widgets.hovered.corner_radius = egui::CornerRadius::same(7);
    ui.visuals_mut().widgets.active.corner_radius = egui::CornerRadius::same(7);
    ui.visuals_mut().selection.bg_fill = SELECTED;
}

pub fn modal_style(ui: &mut egui::Ui, width: f32) {
    let scale = (crate::preferences::dialog_font_size(ui.ctx()) / 14.0).max(1.0);
    dialog_style(ui, width * scale);
}

pub fn dialog_header(
    ui: &mut egui::Ui,
    symbol: &str,
    title: &str,
    subtitle: &str,
    accent: Color32,
) -> bool {
    let mut close = false;
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(42.0, 42.0), egui::Sense::hover());
        ui.painter()
            .rect_filled(rect, 12, accent.gamma_multiply(0.13));
        if symbol == "⌕" {
            let center = rect.center() - egui::vec2(2.0, 2.0);
            ui.painter()
                .circle_stroke(center, 7.0, Stroke::new(2.0_f32, accent));
            ui.painter().line_segment(
                [
                    center + egui::vec2(5.0, 5.0),
                    center + egui::vec2(11.0, 11.0),
                ],
                Stroke::new(2.0_f32, accent),
            );
        } else if symbol == "⑂" {
            let c = rect.center();
            ui.painter().line_segment(
                [c + egui::vec2(-5.0, -9.0), c + egui::vec2(-5.0, 9.0)],
                Stroke::new(2.0_f32, accent),
            );
            ui.painter().line_segment(
                [c + egui::vec2(-5.0, 3.0), c + egui::vec2(7.0, -7.0)],
                Stroke::new(2.0_f32, accent),
            );
            for offset in [
                egui::vec2(-5.0, -9.0),
                egui::vec2(-5.0, 9.0),
                egui::vec2(7.0, -7.0),
            ] {
                ui.painter().circle_filled(c + offset, 3.0, accent);
            }
        } else {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                symbol,
                FontId::proportional(23.0),
                accent,
            );
        }
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.set_max_width((ui.available_width() - 44.0).max(100.0));
            ui.label(
                RichText::new(title)
                    .size(crate::preferences::dialog_font_size(ui.ctx()) + 8.0)
                    .strong()
                    .color(Color32::from_rgb(235, 237, 243)),
            );
            if !subtitle.is_empty() {
                ui.label(
                    RichText::new(subtitle)
                        .size(crate::preferences::dialog_font_size(ui.ctx()))
                        .color(MUTED),
                );
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (id, rect) = ui.allocate_space(egui::vec2(32.0, 32.0));
            close = icon_at(ui, rect, id, Icon::Close, "Close dialog (Esc)").clicked();
        });
    });
    ui.add_space(10.0);
    close
}

pub fn dialog_section(ui: &mut egui::Ui, title: &str, description: &str) {
    ui.label(
        RichText::new(title)
            .size(crate::preferences::dialog_font_size(ui.ctx()) + 1.0)
            .strong()
            .color(TEXT),
    );
    if !description.is_empty() {
        ui.label(
            RichText::new(description)
                .size(crate::preferences::dialog_font_size(ui.ctx()))
                .color(MUTED),
        );
    }
}

pub fn dialog_card<R>(ui: &mut egui::Ui, contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(EDITOR)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(9)
        .inner_margin(14)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            contents(ui)
        })
        .inner
}

pub fn dialog_path(ui: &mut egui::Ui, path: &Path) {
    dialog_card(ui, |ui| {
        ui.horizontal(|ui| {
            if path.is_dir() {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::hover());
                folder(ui.painter(), rect);
            } else {
                ui.label(file_icon(path));
            }
            ui.label(
                RichText::new(path.file_name().unwrap_or_default().to_string_lossy())
                    .strong()
                    .color(TEXT),
            );
        });
        ui.add(
            egui::Label::new(
                RichText::new(path.display().to_string())
                    .size(crate::preferences::dialog_font_size(ui.ctx()))
                    .color(MUTED),
            )
            .wrap(),
        );
    });
}

pub fn dialog_divider(ui: &mut egui::Ui) {
    ui.add_space(10.0);
    ui.separator();
    ui.add_space(4.0);
}

pub fn dialog_button(ui: &mut egui::Ui, label: &str, accent: Option<Color32>) -> egui::Response {
    let mut button = egui::Button::new(
        RichText::new(label)
            .size(crate::preferences::dialog_font_size(ui.ctx()))
            .color(if accent.is_some() {
                Color32::WHITE
            } else {
                TEXT
            }),
    )
    .corner_radius(7)
    .min_size(egui::vec2(94.0, 35.0));
    if let Some(accent) = accent {
        button = button.fill(accent).stroke(Stroke::NONE);
    }
    ui.add(button)
}

pub const GREEN: Color32 = Color32::from_rgb(112, 190, 148);
pub const PURPLE: Color32 = Color32::from_rgb(178, 153, 225);
pub const ACTIVE_TAB: Color32 = Color32::from_rgb(28, 36, 49);

#[derive(Clone, Copy, PartialEq)]
pub enum Icon {
    Close,
    HideLeft,
    HideRight,
    HideDown,
    Plus,
    Search,
    History,
    Check,
    Refresh,
    Terminal,
    Table,
    Structure,
    Branch,
    Dock,
    External,
    Maximize,
    Restore,
}

pub fn paint_icon(painter: &egui::Painter, center: egui::Pos2, icon: Icon, color: Color32) {
    let stroke = Stroke::new(1.5_f32, color);
    let p = |x, y| center + egui::vec2(x, y);
    let line = |a: (f32, f32), b: (f32, f32)| {
        painter.line_segment([p(a.0, a.1), p(b.0, b.1)], stroke);
    };
    match icon {
        Icon::Maximize | Icon::Restore => {
            if icon == Icon::Restore {
                painter.rect_stroke(
                    egui::Rect::from_min_max(p(-2.0, -5.0), p(5.0, 2.0)),
                    1.0,
                    stroke,
                    egui::StrokeKind::Inside,
                );
            }
            painter.rect_stroke(
                egui::Rect::from_min_max(p(-5.0, -2.0), p(2.0, 5.0)),
                1.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            if icon == Icon::Maximize {
                line((-5.0, -5.0), (5.0, -5.0));
                line((5.0, -5.0), (5.0, 5.0));
            }
        }
        Icon::Close => {
            line((-4.0, -4.0), (4.0, 4.0));
            line((-4.0, 4.0), (4.0, -4.0));
        }
        Icon::Plus => {
            line((-5.0, 0.0), (5.0, 0.0));
            line((0.0, -5.0), (0.0, 5.0));
        }
        Icon::HideLeft | Icon::HideRight => {
            let d = if icon == Icon::HideLeft { 1.0 } else { -1.0 };
            line((-5.0 * d, -6.0), (-5.0 * d, 6.0));
            painter.add(egui::Shape::line(
                vec![p(3.0 * d, -4.0), p(-d, 0.0), p(3.0 * d, 4.0)],
                stroke,
            ));
        }
        Icon::HideDown => {
            painter.add(egui::Shape::line(
                vec![p(-4.0, -3.0), p(0.0, 1.0), p(4.0, -3.0)],
                stroke,
            ));
            line((-6.0, 6.0), (6.0, 6.0));
        }
        Icon::Search => {
            painter.circle_stroke(p(-2.0, -2.0), 5.0, stroke);
            line((2.0, 2.0), (6.5, 6.5));
        }
        Icon::History => {
            painter.circle_stroke(center, 6.0, stroke);
            line((0.0, -3.5), (0.0, 0.5));
            line((0.0, 0.5), (3.0, 2.0));
        }
        Icon::Check => {
            painter.add(egui::Shape::line(
                vec![p(-4.5, 0.0), p(-1.0, 3.5), p(5.0, -3.0)],
                stroke,
            ));
        }
        Icon::Refresh => {
            let points = (0..=20)
                .map(|n| {
                    let angle = -0.65 + n as f32 * 5.1 / 20.0;
                    p(angle.cos() * 6.0, angle.sin() * 6.0)
                })
                .collect();
            painter.add(egui::Shape::line(points, stroke));
            line((5.0, -7.0), (5.0, -2.0));
            line((5.0, -2.0), (0.0, -2.0));
        }
        Icon::Terminal | Icon::Dock => {
            painter.rect_stroke(
                egui::Rect::from_center_size(center, egui::vec2(16.0, 13.0)),
                2,
                stroke,
                egui::StrokeKind::Inside,
            );
            if icon == Icon::Terminal {
                painter.add(egui::Shape::line(
                    vec![p(-5.0, -3.0), p(-2.0, 0.0), p(-5.0, 3.0)],
                    stroke,
                ));
                line((0.0, 3.0), (4.0, 3.0));
            } else {
                line((-7.0, 2.0), (7.0, 2.0));
            }
        }
        Icon::Table => {
            painter.rect_stroke(
                egui::Rect::from_center_size(center, egui::vec2(16.0, 14.0)),
                2,
                stroke,
                egui::StrokeKind::Inside,
            );
            line((-7.0, -2.0), (7.0, -2.0));
            line((-7.0, 3.0), (7.0, 3.0));
            line((-2.0, -6.0), (-2.0, 6.0));
            line((3.0, -6.0), (3.0, 6.0));
        }
        Icon::Structure => {
            for (y, x) in [(-5.0, -5.0), (0.0, -2.0), (5.0, -2.0)] {
                painter.circle_filled(p(x, y), 1.5, color);
                line((x + 4.0, y), (7.0, y));
            }
        }
        Icon::Branch => {
            line((-4.0, -6.0), (-4.0, 6.0));
            painter.add(egui::Shape::line(
                vec![p(-4.0, 2.0), p(4.0, -1.0), p(4.0, -6.0)],
                stroke,
            ));
            for (x, y) in [(-4.0, -6.0), (-4.0, 6.0), (4.0, -6.0)] {
                painter.circle_filled(p(x, y), 2.2, color);
            }
        }
        Icon::External => {
            painter.add(egui::Shape::line(
                vec![p(-1.0, -6.0), p(6.0, -6.0), p(6.0, 1.0)],
                stroke,
            ));
            line((-2.0, 2.0), (6.0, -6.0));
            painter.add(egui::Shape::line(
                vec![
                    p(-3.0, -5.0),
                    p(-6.0, -5.0),
                    p(-6.0, 6.0),
                    p(5.0, 6.0),
                    p(5.0, 3.0),
                ],
                stroke,
            ));
        }
    }
}

fn icon_at(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    id: egui::Id,
    icon: Icon,
    tooltip: &str,
) -> egui::Response {
    let response = ui.interact(rect, id, egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), tooltip)
    });
    let hovered = response.hovered() || response.has_focus();
    let close = icon == Icon::Close;
    let fill = if hovered && close {
        Color32::from_rgb(82, 45, 55)
    } else if hovered {
        HEADER
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect.shrink(1.0), 6, fill);
    ui.painter().rect_stroke(
        rect.shrink(1.0),
        6,
        Stroke::new(
            1.0_f32,
            if response.has_focus() {
                BLUE
            } else if hovered {
                BORDER
            } else {
                Color32::TRANSPARENT
            },
        ),
        egui::StrokeKind::Inside,
    );
    paint_icon(
        ui.painter(),
        rect.center(),
        icon,
        if !ui.is_enabled() {
            MUTED.gamma_multiply(0.4)
        } else if hovered && close {
            Color32::from_rgb(255, 159, 169)
        } else if hovered {
            Color32::WHITE
        } else {
            Color32::from_rgb(163, 174, 192)
        },
    );
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(tooltip)
}

pub fn icon_button(ui: &mut egui::Ui, icon: Icon, tooltip: &str) -> egui::Response {
    let (id, rect) = ui.allocate_space(egui::vec2(28.0, 28.0));
    icon_at(ui, rect, id, icon, tooltip)
}

pub fn navigation_button(
    ui: &mut egui::Ui,
    icon: Icon,
    active: bool,
    tooltip: &str,
) -> egui::Response {
    let (id, rect) = ui.allocate_space(egui::vec2(31.0, 34.0));
    let response = icon_at(ui, rect, id, icon, tooltip);
    if active {
        ui.painter().rect_filled(rect.shrink(1.0), 6, SELECTED);
        ui.painter().rect_filled(
            egui::Rect::from_min_size(rect.min + egui::vec2(-3.0, 7.0), egui::vec2(2.0, 20.0)),
            1,
            BLUE,
        );
        paint_icon(ui.painter(), rect.center(), icon, BLUE);
    }
    response
}

pub fn heading_icon(ui: &mut egui::Ui, icon: Icon, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 24.0), egui::Sense::hover());
    paint_icon(ui.painter(), rect.center(), icon, color);
}

pub fn status_pill(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(status_width(ui, text), 28.0),
        egui::Sense::hover(),
    );
    let center = egui::pos2(rect.left() + 6.0, rect.center().y);
    if color == GREEN {
        paint_icon(ui.painter(), center, Icon::Check, color);
    } else {
        ui.painter().circle_filled(center, 3.0, color);
    }
    if !text.is_empty() {
        ui.painter().text(
            egui::pos2(rect.left() + 17.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            text,
            FontId::proportional(11.5),
            color,
        );
    }
    response
}

pub enum TabKind<'a> {
    File(&'a Path),
    Terminal { exited: bool },
}
pub struct TabResponse {
    pub activate: bool,
    pub close: bool,
}

pub fn workspace_tab(
    ui: &mut egui::Ui,
    id: egui::Id,
    title: &str,
    kind: TabKind<'_>,
    selected: bool,
    dirty: bool,
    tooltip: &str,
) -> TabResponse {
    let font = FontId::proportional(13.0);
    let mut job = LayoutJob::simple_singleline(
        title.to_owned(),
        font,
        if selected {
            Color32::from_rgb(239, 243, 250)
        } else {
            TEXT
        },
    );
    job.wrap.max_width = 190.0;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    let height = if matches!(kind, TabKind::File(_)) {
        36.0
    } else {
        32.0
    };
    let width = (galley.size().x + 76.0 + if dirty { 10.0 } else { 0.0 }).max(118.0);
    let (_, rect) = ui.allocate_space(egui::vec2(width, height));
    let close_rect = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 17.0, rect.center().y),
        egui::vec2(24.0, 24.0),
    );
    let body =
        egui::Rect::from_min_max(rect.min, egui::pos2(close_rect.left() - 2.0, rect.bottom()));
    let response = ui.interact(body, id, egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            selected,
            title,
        )
    });
    let fill = if selected {
        ACTIVE_TAB
    } else if response.hovered() {
        HEADER
    } else {
        Color32::TRANSPARENT
    };
    let corners = egui::CornerRadius {
        nw: 7,
        ne: 7,
        sw: 0,
        se: 0,
    };
    ui.painter().rect_filled(rect, corners, fill);
    ui.painter().rect_stroke(
        rect,
        corners,
        Stroke::new(
            1.0_f32,
            if selected {
                BORDER
            } else {
                Color32::TRANSPARENT
            },
        ),
        egui::StrokeKind::Inside,
    );
    if selected {
        ui.painter().line_segment(
            [
                rect.left_bottom() + egui::vec2(1.0, -1.0),
                rect.right_bottom() + egui::vec2(-1.0, -1.0),
            ],
            Stroke::new(2.0_f32, BLUE),
        );
    }
    match kind {
        TabKind::File(path) => {
            let (label, color) = file_kind(path);
            ui.painter().text(
                rect.left_center() + egui::vec2(15.0, 0.0),
                egui::Align2::CENTER_CENTER,
                label,
                FontId::monospace(10.0),
                color,
            );
        }
        TabKind::Terminal { exited } => {
            paint_icon(
                ui.painter(),
                rect.left_center() + egui::vec2(17.0, 0.0),
                Icon::Terminal,
                if exited { MUTED } else { GREEN },
            );
        }
    }
    ui.painter().galley(
        egui::pos2(rect.left() + 32.0, rect.center().y - galley.size().y / 2.0),
        galley,
        TEXT,
    );
    if dirty {
        ui.painter().circle_filled(
            egui::pos2(close_rect.left() - 8.0, rect.center().y),
            3.0,
            WARNING,
        );
    }
    if selected {
        let key = ui.id().with("visible-active-tab");
        let clip = ui.clip_rect();
        let changed = ui.ctx().data_mut(|data| {
            let changed = data
                .get_temp::<(egui::Id, f32)>(key)
                .is_none_or(|(previous, width)| {
                    previous != id || (width - clip.width()).abs() > 1.0
                });
            data.insert_temp(key, (id, clip.width()));
            changed
        });
        if changed {
            let delta = if rect.right() > clip.right() {
                clip.right() - rect.right()
            } else if rect.left() < clip.left() {
                clip.left() - rect.left()
            } else {
                0.0
            };
            ui.scroll_with_delta(egui::vec2(delta, 0.0));
        }
    }
    let close = icon_at(ui, close_rect, id.with("close"), Icon::Close, "Close tab").clicked()
        || response.clicked_by(egui::PointerButton::Middle);
    TabResponse {
        activate: response.on_hover_text(tooltip).clicked(),
        close,
    }
}

pub fn action_chip_width(ui: &egui::Ui, label: &str) -> f32 {
    if label.is_empty() {
        28.0
    } else {
        ui.fonts_mut(|fonts| {
            fonts
                .layout_no_wrap(label.to_owned(), FontId::proportional(12.0), TEXT)
                .size()
                .x
        }) + 36.0
    }
}

pub fn action_chip(
    ui: &mut egui::Ui,
    icon: Icon,
    label: &str,
    selected: bool,
    tooltip: &str,
) -> egui::Response {
    let width = action_chip_width(ui, label);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 28.0), egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, ui.is_enabled(), selected, tooltip)
    });
    let hovered = response.hovered() || response.has_focus();
    if selected || hovered {
        ui.painter().rect_filled(
            rect,
            5,
            if selected {
                BLUE.gamma_multiply(0.15)
            } else {
                HEADER
            },
        );
        if selected || response.has_focus() {
            ui.painter().rect_stroke(
                rect.shrink(0.5),
                5,
                Stroke::new(1.0_f32, BLUE.gamma_multiply(0.45)),
                egui::StrokeKind::Inside,
            );
        }
    }
    let color = if !ui.is_enabled() {
        MUTED.gamma_multiply(0.4)
    } else if selected {
        Color32::from_rgb(139, 182, 255)
    } else if hovered {
        Color32::WHITE
    } else {
        TEXT
    };
    paint_icon(
        ui.painter(),
        egui::pos2(rect.left() + 14.0, rect.center().y),
        icon,
        color,
    );
    if !label.is_empty() {
        ui.painter().text(
            egui::pos2(rect.left() + 28.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            FontId::proportional(12.0),
            color,
        );
    }
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(tooltip)
        .on_disabled_hover_text(tooltip)
}

pub fn status_width(ui: &egui::Ui, label: &str) -> f32 {
    if label.is_empty() {
        14.0
    } else {
        ui.fonts_mut(|fonts| {
            fonts
                .layout_no_wrap(label.to_owned(), FontId::proportional(11.5), TEXT)
                .size()
                .x
        }) + 18.0
    }
}
