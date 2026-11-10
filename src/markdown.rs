//! Comrak owns Markdown parsing. The event types below are only a compatibility
//! boundary to egui_commonmark's native widgets; they are never reparsed.
use comrak::{
    Arena, Options,
    arena_tree::NodeEdge,
    nodes::{AstNode, ListType, NodeValue, TableAlignment},
    parse_document,
};
use egui_commonmark::CommonMarkCache;
use std::{
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct HeightIndex {
    values: Vec<f32>,
    tree: Vec<f32>,
}
impl HeightIndex {
    pub fn reset(&mut self, values: Vec<f32>) {
        self.tree = vec![0.0; values.len() + 1];
        self.values = values;
        for i in 0..self.values.len() {
            let mut node = i + 1;
            while node < self.tree.len() {
                self.tree[node] += self.values[i];
                node += node & node.wrapping_neg();
            }
        }
    }
    pub fn set(&mut self, index: usize, height: f32) {
        let difference = height - self.values[index];
        self.values[index] = height;
        let mut node = index + 1;
        while node < self.tree.len() {
            self.tree[node] += difference;
            node += node & node.wrapping_neg();
        }
    }
    pub fn prefix(&self, count: usize) -> f32 {
        let mut node = count;
        let mut value = 0.0;
        while node > 0 {
            value += self.tree[node];
            node &= node - 1;
        }
        value
    }
    pub fn row(&self, y: f32) -> usize {
        if self.values.is_empty() {
            return 0;
        }
        let mut index = 0;
        let mut sum = 0.0;
        let mut step = self.values.len().next_power_of_two();
        while step > 0 {
            let next = index + step;
            if next < self.tree.len() && sum + self.tree[next] <= y {
                sum += self.tree[next];
                index = next;
            }
            step /= 2;
        }
        index.min(self.values.len() - 1)
    }
}
struct Parsed {
    large: std::collections::HashMap<usize, crate::markdown_virtual::Block>,
    events: Vec<Event<'static>>,
    blocks: Vec<std::ops::Range<usize>>,
    units: Vec<(usize, usize)>,
    code: std::collections::HashMap<usize, Arc<crate::preview::SourcePreview>>,
}

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, LinkType, Tag};

#[derive(Default)]
pub struct MarkdownCache {
    pub native: CommonMarkCache,
    /// Fingerprint of the parsed source. The source itself is not kept, so
    /// the document is not held twice.
    fingerprint: u64,
    pub events: Vec<Event<'static>>,
    pub blocks: Vec<std::ops::Range<usize>>,
    pub heights: Vec<Option<f32>>,
    pub layout_key: Option<u64>,
    pub positions: HeightIndex,
    units: Vec<(usize, usize)>,
    pub code: std::collections::HashMap<usize, Arc<crate::preview::SourcePreview>>,
    pub large: std::collections::HashMap<usize, crate::markdown_virtual::Block>,
    pending: Option<(u64, mpsc::Receiver<Parsed>)>,
    desired: Option<(u64, Instant)>,
}

impl MarkdownCache {
    #[cfg(test)]
    pub fn prepare(&mut self, source: &str) {
        self.prepare_keyed(source, eframe::egui::util::hash(source));
    }

    #[cfg(test)]
    pub fn prepare_keyed(&mut self, source: &str, fingerprint: u64) {
        if fingerprint != self.fingerprint {
            self.install(fingerprint, prepare(source, false));
        }
    }
    fn install(&mut self, fingerprint: u64, parsed: Parsed) {
        let old = std::mem::replace(&mut self.events, parsed.events);
        if !old.is_empty() {
            std::thread::spawn(move || drop(old));
        }
        self.blocks = parsed.blocks;
        self.units = parsed.units;
        self.code = parsed.code;
        self.large = parsed.large;
        self.heights = vec![None; self.blocks.len()];
        self.layout_key = None;
        self.fingerprint = fingerprint;
    }
    pub fn request(
        &mut self,
        source: &str,
        fingerprint: u64,
        width: f32,
        ctx: &eframe::egui::Context,
    ) -> bool {
        let fingerprint = eframe::egui::util::hash((
            fingerprint,
            width.to_bits(),
            ctx.pixels_per_point().to_bits(),
            ctx.style().visuals.dark_mode,
        ));
        if let Some((version, rx)) = &self.pending {
            match rx.try_recv() {
                Ok(parsed) => {
                    let version = *version;
                    self.pending = None;
                    if version == fingerprint {
                        self.install(version, parsed);
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => self.pending = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.fingerprint == fingerprint {
            return true;
        }
        if self.desired.is_none_or(|(old, _)| old != fingerprint) {
            self.desired = Some((fingerprint, Instant::now()));
        }
        let delay = if self.events.is_empty() {
            Duration::ZERO
        } else {
            Duration::from_millis(200)
        };
        let elapsed = self.desired.unwrap().1.elapsed();
        if self.pending.is_none() && elapsed >= delay {
            let definitions = ctx.fonts(|fonts| fonts.definitions().clone());
            let pixels = ctx.pixels_per_point();
            let source = source.to_owned();
            let dark = ctx.style().visuals.dark_mode;
            let ctx = ctx.clone();
            let (tx, rx) = mpsc::channel();
            self.pending = Some((fingerprint, rx));
            std::thread::spawn(move || {
                let mut parsed = prepare(&source, dark);
                let worker = eframe::egui::Context::default();
                worker.set_fonts(definitions);
                worker.set_pixels_per_point(pixels);
                let _ = worker.run(Default::default(), |ctx| {
                    eframe::egui::CentralPanel::default().show(ctx, |ui| {
                        for (i, range) in parsed.blocks.iter().enumerate() {
                            if let Some(block) = crate::markdown_virtual::Block::build(
                                ui,
                                &parsed.events[range.clone()],
                                width,
                            ) {
                                parsed.large.insert(i, block);
                            }
                        }
                    });
                });
                let _ = tx.send(parsed);
                ctx.request_repaint();
            });
        } else if self.pending.is_none() {
            ctx.request_repaint_after(delay.saturating_sub(elapsed));
        }
        false
    }
    pub fn resize(&mut self, key: u64, width: f32) {
        if self.layout_key == Some(key) {
            return;
        }
        self.heights.fill(None);
        self.layout_key = Some(key);
        let columns = (width / 7.0).max(1.0);
        self.positions.reset(
            self.units
                .iter()
                .enumerate()
                .map(|(i, (chars, lines))| {
                    if let Some(block) = self.large.get(&i) {
                        return block.height() + 12.0;
                    }
                    self.code.get(&i).map_or_else(
                        || {
                            ((*chars as f32 / columns).ceil() + *lines as f32).max(1.0) * 20.0
                                + 12.0
                        },
                        |code| code.len() as f32 * 18.0 + 20.0,
                    )
                })
                .collect(),
        );
    }
}

fn ranges(events: &[Event<'static>]) -> Vec<std::ops::Range<usize>> {
    let mut result = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, event) in events.iter().enumerate() {
        match event {
            Event::Start(_) => depth += 1,
            Event::End(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 {
            result.push(start..i + 1);
            start = i + 1;
        }
    }
    if start < events.len() {
        result.push(start..events.len());
    }
    result
}
fn prepare(source: &str, dark: bool) -> Parsed {
    let source_events = parse(source);
    let mut events = Vec::new();
    // A long list is paged at item boundaries; reopened ordered lists retain
    // the correct ordinal. Nested content remains balanced inside each item.
    for range in ranges(&source_events) {
        let slice = &source_events[range];
        if let Some(Event::Start(Tag::List(number))) = slice.first()
            && slice.len() > 256
        {
            let mut depth = 0;
            let mut starts = Vec::new();
            for (i, event) in slice.iter().enumerate() {
                if depth == 1 && matches!(event, Event::Start(Tag::Item)) {
                    starts.push(i);
                }
                match event {
                    Event::Start(_) => depth += 1,
                    Event::End(_) => depth -= 1,
                    _ => {}
                }
            }
            starts.push(slice.len() - 1);
            for (page, chunk) in starts[..starts.len() - 1].chunks(8).enumerate() {
                events.push(Event::Start(Tag::List(number.map(|n| n + page as u64 * 8))));
                let end = starts[(page * 8 + chunk.len()).min(starts.len() - 1)];
                events.extend_from_slice(&slice[chunk[0]..end]);
                events.push(Event::End(Tag::List(None).to_end()));
            }
        } else {
            events.extend_from_slice(slice);
        }
    }
    let blocks = ranges(&events);
    let mut units = Vec::new();
    let mut code = std::collections::HashMap::new();
    for (i, range) in blocks.iter().enumerate() {
        let mut chars = 0;
        let mut lines = 0;
        for event in &events[range.clone()] {
            match event {
                Event::Text(t) | Event::Code(t) => {
                    chars += t.chars().count();
                    lines += t.bytes().filter(|b| *b == b'\n').count();
                }
                Event::SoftBreak | Event::HardBreak => lines += 1,
                _ => {}
            }
        }
        units.push((chars, lines));
        if lines > 128
            && let Event::Start(Tag::CodeBlock(kind)) = &events[range.start]
        {
            let language = match kind {
                CodeBlockKind::Fenced(language) => {
                    language.split_whitespace().next().unwrap_or("txt")
                }
                _ => "txt",
            };
            let extension = match language {
                "rust" => "rs",
                "python" => "py",
                "javascript" => "js",
                "typescript" => "ts",
                other => other,
            };
            let text: String = events[range.clone()]
                .iter()
                .filter_map(|event| {
                    if let Event::Text(t) = event {
                        Some(t.as_ref())
                    } else {
                        None
                    }
                })
                .collect();
            if let Some(preview) = crate::preview::SourcePreview::build(
                std::path::PathBuf::from(format!("markdown.{extension}")),
                text,
                dark,
                &|| false,
            ) {
                code.insert(i, Arc::new(preview));
            }
        }
    }
    Parsed {
        large: Default::default(),
        events,
        blocks,
        units,
        code,
    }
}

fn options() -> Options<'static> {
    let mut options = Options::default();
    options.extension.strikethrough = true;
    options.extension.table = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;
    options.extension.footnotes = true;
    options.extension.description_lists = true;
    // Keep alert markers as blockquote content for the native alert widget.
    options
}

fn parse(source: &str) -> Vec<Event<'static>> {
    let arena = Arena::new();
    let root = parse_document(&arena, source, &options());
    let mut events = Vec::new();
    let mut skip = None;
    // Iterative traversal also handles deeply nested documents without recursing.
    for edge in root.traverse() {
        let (node, entering) = match edge {
            NodeEdge::Start(node) => (node, true),
            NodeEdge::End(node) => (node, false),
        };
        if let Some(skipped) = skip {
            if !entering && std::ptr::eq(node, skipped) {
                skip = None;
            }
            continue;
        }
        let data = node.data();
        if entering
            && matches!(data.value, NodeValue::Paragraph)
            && !node
                .parent()
                .is_some_and(|parent| matches!(parent.data().value, NodeValue::TaskItem(_)))
            && let Some(tags) = image_row(node)
        {
            events.push(Event::Start(Tag::HtmlBlock));
            events.push(Event::Html(
                format!("<div data-coder-images>\n{tags}\n</div>\n").into(),
            ));
            events.push(Event::End(Tag::HtmlBlock.to_end()));
            skip = Some(node);
            continue;
        }
        let tag = match &data.value {
            NodeValue::Paragraph if !tight_paragraph(node) => Some(Tag::Paragraph),
            NodeValue::Heading(heading) => Some(Tag::Heading {
                level: match heading.level {
                    1 => HeadingLevel::H1,
                    2 => HeadingLevel::H2,
                    3 => HeadingLevel::H3,
                    4 => HeadingLevel::H4,
                    5 => HeadingLevel::H5,
                    _ => HeadingLevel::H6,
                },
                id: None,
                classes: vec![],
                attrs: vec![],
            }),
            NodeValue::BlockQuote => Some(Tag::BlockQuote(None)),
            NodeValue::List(list) => Some(Tag::List(
                (list.list_type == ListType::Ordered).then_some(list.start as u64),
            )),
            NodeValue::Item(_) | NodeValue::TaskItem(_) => Some(Tag::Item),
            NodeValue::CodeBlock(code) => Some(Tag::CodeBlock(if code.fenced {
                CodeBlockKind::Fenced(code.info.clone().into())
            } else {
                CodeBlockKind::Indented
            })),
            NodeValue::HtmlBlock(_) => Some(Tag::HtmlBlock),
            NodeValue::Emph => Some(Tag::Emphasis),
            NodeValue::Strong => Some(Tag::Strong),
            NodeValue::Strikethrough => Some(Tag::Strikethrough),
            NodeValue::Link(link) => Some(Tag::Link {
                link_type: LinkType::Inline,
                dest_url: link.url.clone().into(),
                title: link.title.clone().into(),
                id: "".into(),
            }),
            NodeValue::Image(link) => Some(Tag::Image {
                link_type: LinkType::Inline,
                dest_url: link.url.clone().into(),
                title: link.title.clone().into(),
                id: "".into(),
            }),
            NodeValue::Table(table) => Some(Tag::Table(
                table
                    .alignments
                    .iter()
                    .map(|a| match a {
                        TableAlignment::None => Alignment::None,
                        TableAlignment::Left => Alignment::Left,
                        TableAlignment::Center => Alignment::Center,
                        TableAlignment::Right => Alignment::Right,
                    })
                    .collect(),
            )),
            NodeValue::TableRow(true) => Some(Tag::TableHead),
            NodeValue::TableRow(false) => Some(Tag::TableRow),
            NodeValue::TableCell => Some(Tag::TableCell),
            NodeValue::FootnoteDefinition(note) => {
                Some(Tag::FootnoteDefinition(note.name.clone().into()))
            }
            NodeValue::DescriptionList => Some(Tag::DefinitionList),
            NodeValue::DescriptionTerm => Some(Tag::DefinitionListTitle),
            NodeValue::DescriptionDetails => Some(Tag::DefinitionListDefinition),
            _ => None,
        };
        if let Some(tag) = tag {
            events.push(if entering {
                Event::Start(tag)
            } else {
                Event::End(tag.to_end())
            });
        }
        if entering {
            let leaf = match &data.value {
                NodeValue::Text(text) => Some(Event::Text(text.to_string().into())),
                NodeValue::Code(code) => Some(Event::Code(code.literal.clone().into())),
                NodeValue::CodeBlock(code) => Some(Event::Text(code.literal.clone().into())),
                NodeValue::HtmlBlock(html) => Some(Event::Html(html.literal.clone().into())),
                NodeValue::HtmlInline(html) => Some(Event::InlineHtml(html.clone().into())),
                NodeValue::SoftBreak => Some(Event::SoftBreak),
                NodeValue::LineBreak => Some(Event::HardBreak),
                NodeValue::ThematicBreak => Some(Event::Rule),
                NodeValue::Paragraph if node.previous_sibling().is_none() => {
                    node.parent().and_then(|parent| match &parent.data().value {
                        NodeValue::TaskItem(task) => {
                            Some(Event::TaskListMarker(task.symbol.is_some()))
                        }
                        _ => None,
                    })
                }
                NodeValue::FootnoteReference(note) => {
                    Some(Event::FootnoteReference(note.name.clone().into()))
                }
                _ => None,
            };
            if let Some(event) = leaf {
                events.push(event);
            }
        }
    }
    events
}

fn tight_paragraph<'a>(node: &'a AstNode<'a>) -> bool {
    node.parent()
        .and_then(|parent| parent.parent())
        .is_some_and(|list| matches!(&list.data().value, NodeValue::List(list) if list.tight))
}

/// Image-only paragraphs use the existing inert HTML image widget so linked
/// badges share a row and their entire image remains clickable.
fn image_row<'a>(paragraph: &'a AstNode<'a>) -> Option<String> {
    let mut tags = String::new();
    for child in paragraph.children() {
        let data = child.data();
        match &data.value {
            NodeValue::Text(text) if text.trim().is_empty() => continue,
            NodeValue::SoftBreak | NodeValue::LineBreak => continue,
            NodeValue::Image(_) => tags.push_str(&image_tag(child, "")?),
            NodeValue::Link(link) => {
                for image in child.children() {
                    tags.push_str(&image_tag(image, &link.url)?);
                }
            }
            _ => return None,
        }
    }
    (!tags.is_empty()).then_some(tags)
}

fn image_tag<'a>(node: &'a AstNode<'a>, href: &str) -> Option<String> {
    let data = node.data();
    let NodeValue::Image(image) = &data.value else {
        return None;
    };
    let mut alt = String::new();
    for child in node.descendants().skip(1) {
        match &child.data().value {
            NodeValue::Text(text) => alt.push_str(text),
            NodeValue::Code(code) => alt.push_str(&code.literal),
            NodeValue::SoftBreak | NodeValue::LineBreak => alt.push(' '),
            _ => {}
        }
    }
    Some(format!(
        "<img src=\"{}\" data-href=\"{}\" alt=\"{}\">",
        escape(&image.url),
        escape(href),
        escape(&alt)
    ))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comrak_gfm_events_preserve_structure_and_code() {
        let source = "# Title\n\n3. outer\n   - inner\n\n- [x] done\n\n- [ ] pending\n\n| Name | Value |\n| :--- | ---: |\n| café | 42 |\n\n~~gone~~ https://example.org\n\n[reference][site] and note[^n].\n\n[site]: https://example.org/reference\n\n[^n]: Footnote text\n\n```rust\nlet text = \"<img src='code.png'> **literal**\";\n```\n\n> [!NOTE]\n> Alert text\n";
        let events = parse(source);
        assert!(events.contains(&Event::Start(Tag::List(Some(3)))));
        assert!(events.contains(&Event::TaskListMarker(true)));
        assert!(events.contains(&Event::TaskListMarker(false)));
        assert!(events.contains(&Event::Start(Tag::Table(vec![
            Alignment::Left,
            Alignment::Right
        ]))));
        assert!(events.contains(&Event::Start(Tag::Strikethrough)));
        assert!(events.iter().any(|e| matches!(e, Event::Start(Tag::Link { dest_url, .. }) if dest_url.as_ref() == "https://example.org/reference")));
        assert!(events.iter().any(|e| matches!(e, Event::Start(Tag::Link { dest_url, .. }) if dest_url.as_ref() == "https://example.org")));
        assert!(events.contains(&Event::FootnoteReference("n".into())));
        assert!(events.iter().any(
            |e| matches!(e, Event::Text(text) if text.contains("<img src='code.png'> **literal**"))
        ));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::Html(html) if html.contains("code.png")))
        );
        assert!(events.contains(&Event::Text("[!NOTE]".into())));
        // The native renderer relies on correctly nested start/end events.
        let mut stack = vec![];
        for event in &events {
            match event {
                Event::Start(tag) => stack.push(tag.to_end()),
                Event::End(end) => assert_eq!(stack.pop(), Some(*end)),
                _ => {}
            }
        }
        assert!(stack.is_empty());
        let checked = events
            .iter()
            .position(|e| *e == Event::TaskListMarker(true))
            .unwrap();
        assert_eq!(
            events[checked - 1],
            Event::Start(Tag::Paragraph),
            "Loose task markers belong inside their paragraph"
        );
    }

    #[test]
    fn badge_rows_preserve_reference_links_alt_and_inline_images() {
        let source = "[![**passing** & safe][badge]][project] ![release](release.svg)\n\nProse ![inline](photo.png).\n\n```md\n![code](not-an-image.png)\n```\n\n[badge]: badge.svg\n[project]: https://example.org/project?a=1&b=2\n";
        let events = parse(source);
        let html = events
            .iter()
            .filter_map(|e| match e {
                Event::Html(html) => Some(html.as_ref()),
                _ => None,
            })
            .collect::<String>();
        assert!(html.contains("<div data-coder-images>"));
        assert!(html.contains(
            "data-href=\"https://example.org/project?a=1&amp;b=2\" alt=\"passing &amp; safe\""
        ));
        assert_eq!(html.matches("<img ").count(), 2);
        assert!(events.iter().any(
            |e| matches!(e, Event::Start(Tag::Image { dest_url, .. }) if dest_url.as_ref() == "photo.png")
        ));
        assert!(events.contains(&Event::Text("![code](not-an-image.png)\n".into())));
        assert!(!html.contains("not-an-image.png"));
    }

    #[test]
    fn long_lists_are_paged_with_balanced_tags_and_correct_ordinals() {
        let source = (0..1000)
            .map(|i| format!("{}. item {i}\n", i + 1))
            .collect::<String>();
        let parsed = prepare(&source, false);
        assert!(parsed.blocks.len() > 100);
        for (page, range) in parsed.blocks.iter().enumerate() {
            assert!(
                matches!(&parsed.events[range.start],Event::Start(Tag::List(Some(start))) if *start==1+page as u64*8)
            );
            let mut depth = 0;
            for event in &parsed.events[range.clone()] {
                match event {
                    Event::Start(_) => depth += 1,
                    Event::End(_) => depth -= 1,
                    _ => {}
                }
                assert!(depth >= 0);
            }
            assert_eq!(depth, 0);
        }
    }

    #[test]
    fn virtual_blocks_keep_nested_lists_tables_and_code_balanced() {
        let mut cache = MarkdownCache::default();
        cache.prepare("# Title\n\n- outer\n  - inner\n\n| A | B |\n| - | - |\n| a | b |\n\n```rs\nfn main() {}\n```\n");
        assert_eq!(cache.blocks.len(), 4);
        for range in &cache.blocks {
            let mut depth = 0;
            for event in &cache.events[range.clone()] {
                match event {
                    Event::Start(_) => depth += 1,
                    Event::End(_) => depth -= 1,
                    _ => {}
                }
                assert!(depth >= 0);
            }
            assert_eq!(depth, 0);
        }
        cache.heights.fill(Some(50.0));
        cache.prepare("# Edited");
        assert_eq!(cache.heights, vec![None]);
    }

    #[test]
    fn cache_refreshes_on_edits_and_document_switches() {
        let mut cache = MarkdownCache::default();
        cache.prepare("# First");
        assert!(cache.events.contains(&Event::Text("First".into())));
        let previous = cache.events.as_ptr();
        cache.prepare("# First");
        assert_eq!(cache.events.as_ptr(), previous);
        cache.prepare("# Edited");
        assert!(cache.events.contains(&Event::Text("Edited".into())));
        assert!(!cache.events.contains(&Event::Text("First".into())));
        cache.prepare("");
        assert!(cache.events.is_empty());
    }
}
