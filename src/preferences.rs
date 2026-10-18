//! Application preferences shared by the editor and terminal.
use anyhow::{Context, Result, bail};
use eframe::egui;
use std::{
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct Settings {
    pub shell: String,
    pub tab_name: String,
    pub editor_font_size: f32,
    pub terminal_font_size: f32,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            shell: std::env::var(if cfg!(windows) { "COMSPEC" } else { "SHELL" })
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| {
                    if cfg!(windows) {
                        "cmd.exe"
                    } else if cfg!(target_os = "macos") {
                        "/bin/zsh"
                    } else {
                        "/bin/sh"
                    }
                    .into()
                }),
            tab_name: String::new(),
            editor_font_size: 13.0,
            terminal_font_size: 13.0,
        }
    }
}

pub fn path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| PathBuf::from(home).join(".coder/settings.json"))
}
fn legacy_path() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    };
    base.map(|base| base.join("Coder/terminal.json"))
}

fn json(path: &Path) -> Result<serde_json::Value> {
    let bytes = std::fs::read(path)?;
    if bytes.len() > 65536 {
        bail!("Preferences file is too large");
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    if !value.is_object() {
        bail!("Preferences must be a JSON object");
    }
    Ok(value)
}
impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        let value = json(path)?;
        let defaults = Self::default();
        let size = |key| {
            value[key]
                .as_f64()
                .filter(|n| n.is_finite())
                .map(|n| n.clamp(8.0, 40.0) as f32)
                .unwrap_or(13.0)
        };
        Ok(Self {
            shell: value["shell"].as_str().unwrap_or(&defaults.shell).into(),
            tab_name: value["tab_name"].as_str().unwrap_or("").into(),
            editor_font_size: size("editor_font_size"),
            terminal_font_size: size("terminal_font_size"),
        })
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path.parent().context("No preferences directory")?;
        std::fs::create_dir_all(parent)?;
        // Preserve keys owned by other preferences and future versions.
        let mut value = if path.exists() {
            json(path)?
        } else {
            serde_json::json!({})
        };
        value["shell"] = self.shell.clone().into();
        value["tab_name"] = self.tab_name.clone().into();
        value["editor_font_size"] = self.editor_font_size.into();
        value["terminal_font_size"] = self.terminal_font_size.into();
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(&mut temp, &value)?;
        temp.flush()?;
        temp.persist(path)?;
        Ok(())
    }
    pub fn load_saved() -> Result<Self> {
        Self::load_or_migrate(
            &path().context("No home directory for preferences")?,
            legacy_path().as_deref(),
        )
    }
    fn load_or_migrate(path: &Path, legacy: Option<&Path>) -> Result<Self> {
        if path.exists() {
            return Self::load(path);
        }
        let settings = if let Some(legacy) = legacy.filter(|path| path.exists()) {
            Self::load(legacy)?
        } else {
            Self::default()
        };
        settings.save(path)?;
        Ok(settings)
    }
    pub fn apply(&self, ctx: &egui::Context) {
        ctx.data_mut(|data| {
            data.insert_temp(egui::Id::new("editor-font-size"), self.editor_font_size)
        });
        ctx.all_styles_mut(|style| {
            style.text_styles.insert(
                egui::TextStyle::Monospace,
                egui::FontId::monospace(self.editor_font_size),
            );
        });
        ctx.request_repaint();
    }
}
pub fn editor_font_size(ctx: &egui::Context) -> f32 {
    ctx.data(|data| data.get_temp(egui::Id::new("editor-font-size")))
        .unwrap_or(13.0)
}
pub fn line_height(ctx: &egui::Context) -> f32 {
    20.0 * editor_font_size(ctx) / 13.0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preferences_round_trip_preserve_other_keys_and_validate_font_sizes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"future_preference":true,"editor_font_size":99,"terminal_font_size":2}"#,
        )?;
        let mut settings = Settings::load(&path)?;
        assert_eq!(settings.editor_font_size, 40.0);
        assert_eq!(settings.terminal_font_size, 8.0);
        settings.editor_font_size = 17.5;
        settings.terminal_font_size = 16.0;
        settings.save(&path)?;
        let loaded = Settings::load(&path)?;
        assert_eq!(loaded.editor_font_size, 17.5);
        assert_eq!(loaded.terminal_font_size, 16.0);
        assert_eq!(json(&path)?["future_preference"], true);
        std::fs::write(&path, "broken json")?;
        assert!(settings.save(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path)?, "broken json");
        Ok(())
    }
    #[test]
    fn legacy_settings_migrate_once_and_defaults_are_saved() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let old = dir.path().join("terminal.json");
        let path = dir.path().join(".coder/settings.json");
        std::fs::write(&old, r#"{"shell":"/bin/sh","tab_name":"Local"}"#)?;
        let mut settings = Settings::load_or_migrate(&path, Some(&old))?;
        assert_eq!(settings.shell, "/bin/sh");
        assert_eq!(settings.tab_name, "Local");
        assert_eq!(settings.editor_font_size, 13.0);
        assert!(path.is_file());
        assert!(old.is_file());
        settings.editor_font_size = 19.0;
        settings.save(&path)?;
        assert_eq!(
            Settings::load_or_migrate(&path, Some(&old))?.editor_font_size,
            19.0
        );
        let fresh = dir.path().join("fresh/settings.json");
        Settings::load_or_migrate(&fresh, None)?;
        assert!(fresh.is_file());
        Ok(())
    }
    #[test]
    fn editor_font_applies_to_syntax_and_existing_preview_jobs() {
        let ctx = egui::Context::default();
        let settings = Settings {
            editor_font_size: 21.0,
            terminal_font_size: 18.0,
            ..Default::default()
        };
        settings.apply(&ctx);
        let preview = crate::preview::SourcePreview::build(
            "sample.rs".into(),
            "fn main() {}".into(),
            true,
            &|| false,
        )
        .unwrap();
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                for job in [
                    crate::highlighting::highlight(ui, "fn main() {}", "rs"),
                    crate::highlighting::highlight(ui, "{\"key\":true}", "json"),
                    preview.line_for_ui(ui, 0, ""),
                ] {
                    assert!(
                        job.sections
                            .iter()
                            .all(|section| section.format.font_id.size == 21.0)
                    );
                }
                assert_eq!(egui::TextStyle::Monospace.resolve(ui.style()).size, 21.0);
            });
        });
    }
}
