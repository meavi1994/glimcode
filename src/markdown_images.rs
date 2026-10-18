use eframe::egui::{
    self,
    load::{BytesPoll, ImageLoadResult, ImageLoader, ImagePoll, LoadError, SizeHint},
};

/// Badge endpoints often return SVG with no .svg suffix. Delegate decoding and caching
/// to egui's native SVG loader after identifying the response's actual content type.
#[derive(Default)]
pub struct BadgeLoader(egui_extras::loaders::svg_loader::SvgLoader);
impl ImageLoader for BadgeLoader {
    fn id(&self) -> &str {
        "coder::markdown_badges"
    }
    fn load(&self, ctx: &egui::Context, uri: &str, hint: SizeHint) -> ImageLoadResult {
        if uri.ends_with(".svg") {
            return Err(LoadError::NotSupported);
        }
        match ctx.try_load_bytes(uri)? {
            BytesPoll::Pending { size } => Ok(ImagePoll::Pending { size }),
            BytesPoll::Ready { bytes, mime, .. } => {
                if !mime
                    .as_deref()
                    .is_some_and(|m| m.starts_with("image/svg+xml"))
                    && !bytes.windows(4).take(1024).any(|w| w == b"<svg")
                {
                    return Err(LoadError::NotSupported);
                }
                let alias = format!("{uri}#coder.svg");
                ctx.include_bytes(alias.clone(), bytes);
                self.0.load(ctx, &alias, hint)
            }
        }
    }
    fn forget(&self, uri: &str) {
        self.0.forget(&format!("{uri}#coder.svg"));
    }
    fn forget_all(&self) {
        self.0.forget_all();
    }
    fn byte_size(&self) -> usize {
        self.0.byte_size()
    }
    fn end_pass(&self, pass: u64) {
        self.0.end_pass(pass);
    }
}

/// Keep image-only paragraphs in one layout row. In particular a linked badge must
/// allocate the image's width, rather than an empty text hyperlink after the image.
pub fn prepare(source: &str) -> String {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};
    let mut paragraph = None;
    let mut image: Option<(String, String, String)> = None;
    let mut valid = false;
    let mut link: Option<String> = None;
    let mut tags = String::new();
    let mut replacements = Vec::new();
    for (event, range) in Parser::new_ext(source, pulldown_cmark::Options::all()).into_offset_iter()
    {
        match event {
            Event::Start(Tag::Paragraph) => {
                paragraph = Some(range.start);
                valid = true;
                tags.clear();
            }
            Event::Start(Tag::Link { dest_url, .. }) => link = Some(dest_url.into_string()),
            Event::End(TagEnd::Link) => link = None,
            Event::Start(Tag::Image { dest_url, .. }) if paragraph.is_some() => {
                image = Some((
                    dest_url.into_string(),
                    link.clone().unwrap_or_default(),
                    String::new(),
                ));
            }
            Event::End(TagEnd::Image) => {
                if let Some((src, href, alt)) = image.take() {
                    tags.push_str(&format!(
                        "<img src=\"{}\" data-href=\"{}\" alt=\"{}\">",
                        escape(&src),
                        escape(&href),
                        escape(&alt)
                    ));
                }
            }
            Event::Text(text) => {
                if let Some((_, _, alt)) = &mut image {
                    alt.push_str(&text);
                } else if !text.trim().is_empty() {
                    valid = false;
                }
            }
            Event::Code(_) | Event::InlineHtml(_) => valid = false,
            Event::End(TagEnd::Paragraph) => {
                if let Some(start) = paragraph.take()
                    && valid
                    && !tags.is_empty()
                {
                    replacements.push((
                        start..range.end,
                        format!("<div data-coder-images>\n{tags}\n</div>\n"),
                    ));
                }
            }
            _ => {}
        }
    }
    let mut result = source.to_owned();
    for (range, replacement) in replacements.into_iter().rev() {
        result.replace_range(range, &replacement);
    }
    result
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
    fn badge_rows_preserve_links_and_alt_without_rewriting_code_or_prose() {
        let source = "[![passing](https://example.org/badge)](https://example.org/project) ![release](https://example.org/release)\n\nProse ![inline](photo.png).\n\n```md\n![code](not-an-image.png)\n```\n";
        let prepared = prepare(source);
        assert!(prepared.contains("<div data-coder-images>"));
        assert!(prepared.contains("data-href=\"https://example.org/project\" alt=\"passing\""));
        assert_eq!(prepared.matches("<img ").count(), 2);
        assert!(prepared.contains("Prose ![inline](photo.png)."));
        assert!(prepared.contains("```md\n![code](not-an-image.png)\n```"));
    }
    #[test]
    fn extensionless_svg_badges_render_and_cache() {
        let ctx = egui::Context::default();
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="80" height="20"><rect width="80" height="20" fill="green"/></svg>"#;
        ctx.include_bytes("bytes://badge", svg.as_slice());
        let loader = BadgeLoader::default();
        assert!(matches!(
            loader.load(&ctx, "bytes://badge", SizeHint::default()),
            Ok(ImagePoll::Ready { .. })
        ));
        assert!(loader.byte_size() > 0);
        loader.forget("bytes://badge");
        assert_eq!(loader.byte_size(), 0);
    }
}
