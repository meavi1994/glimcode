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

#[cfg(test)]
mod tests {
    use super::*;
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
