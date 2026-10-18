//! Comrak owns Markdown parsing. The event types below are only a compatibility
//! boundary to egui_commonmark's native widgets; they are never reparsed.
use comrak::{
    Arena, Options,
    arena_tree::NodeEdge,
    nodes::{AstNode, ListType, NodeValue, TableAlignment},
    parse_document,
};
use egui_commonmark::CommonMarkCache;
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, LinkType, Tag};

#[derive(Default)]
pub struct MarkdownCache {
    pub native: CommonMarkCache,
    source: Option<String>,
    pub events: Vec<Event<'static>>,
}

impl MarkdownCache {
    pub fn prepare(&mut self, source: &str) {
        if self.source.as_deref() != Some(source) {
            self.events = parse(source);
            self.source = Some(source.to_owned());
        }
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
