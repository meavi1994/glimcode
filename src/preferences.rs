//! Application preferences shared by the editor and terminal.
use anyhow::{Context, Result, bail};
use eframe::egui;
use std::{
    io::Write,
    path::{Path, PathBuf},
};

const DEFAULT_FONT_SIZE: f32 = 14.0;
pub mod tmux;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SettingsPage {
    #[default]
    Appearance,
    Terminal,
    Tmux,
    Sharing,
}

impl SettingsPage {
    pub const ALL: [Self; 4] = [Self::Appearance, Self::Terminal, Self::Tmux, Self::Sharing];

    pub fn title(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Terminal => "Terminal",
            Self::Tmux => "tmux settings",
            Self::Sharing => "Terminal sharing",
        }
    }

    pub fn subtitle(self) -> &'static str {
        match self {
            Self::Appearance => "Fonts for the editor, dialogs, search and terminal",
            Self::Terminal => "Shell and terminal tab defaults",
            Self::Tmux => "Persistence, client flags and tmux configuration",
            Self::Sharing => "HTTPS certificates for ttyd browser sharing",
        }
    }
}

#[derive(Clone)]
pub struct Settings {
    pub shell: String,
    pub tab_name: String,
    pub terminal_tls_certificate: String,
    pub terminal_tls_private_key: String,
    pub tmux_enabled: bool,
    pub terminal_backend: String,
    pub tmux_client_args: String,
    pub tmux_options: Vec<tmux::Entry>,
    pub editor_font_size: f32,
    pub terminal_font_size: f32,
    pub dialog_font_size: f32,
    pub search_results_font_size: f32,
    pub search_filters_font_size: f32,
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
            terminal_tls_certificate: String::new(),
            terminal_tls_private_key: String::new(),
            tmux_enabled: true,
            terminal_backend: "tmux".into(),
            tmux_client_args: "-u".into(),
            tmux_options: tmux::defaults(),
            editor_font_size: DEFAULT_FONT_SIZE,
            terminal_font_size: DEFAULT_FONT_SIZE,
            dialog_font_size: DEFAULT_FONT_SIZE,
            search_results_font_size: DEFAULT_FONT_SIZE,
            search_filters_font_size: DEFAULT_FONT_SIZE,
        }
    }
}

pub fn path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| PathBuf::from(home).join(".glimcode/settings.json"))
}

pub fn expand_home_path(value: &str) -> PathBuf {
    if let Some(relative) = value.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(relative);
    }
    PathBuf::from(value)
}

pub fn global_ignore_path() -> Option<PathBuf> {
    path().and_then(|path| path.parent().map(|parent| parent.join(".gitignore")))
}

fn ensure_global_ignore(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("No preferences directory")?)?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(b"# Extra ignore rules for all Glimcode projects.\n# Use .gitignore syntax; project ignore rules can override these defaults.\n")?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.into()),
    }
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
    pub fn validate_terminal_tls(&self) -> Result<()> {
        let certificate = self.terminal_tls_certificate.trim();
        let private_key = self.terminal_tls_private_key.trim();
        if certificate.is_empty() && private_key.is_empty() {
            return Ok(());
        }
        if certificate.is_empty() || private_key.is_empty() {
            bail!(
                "Set both the HTTPS certificate and private key, or clear both to use the generated certificate."
            );
        }
        for (label, value) in [
            ("HTTPS certificate", certificate),
            ("HTTPS private key", private_key),
        ] {
            if !expand_home_path(value).is_file() {
                bail!("{label} file does not exist: {value}");
            }
        }
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Self> {
        let value = json(path)?;
        let defaults = Self::default();
        let size = |key| {
            value[key]
                .as_f64()
                .filter(|n| n.is_finite())
                .map(|n| n.clamp(8.0, 40.0) as f32)
                .unwrap_or(DEFAULT_FONT_SIZE)
        };
        let mut settings = Self {
            tmux_enabled: value["tmux_enabled"].as_bool().unwrap_or(true),
            terminal_backend: if value["terminal_backend"].as_str() == Some("zmx") {
                "zmx"
            } else {
                "tmux"
            }
            .into(),
            tmux_client_args: value["tmux_client_args"].as_str().unwrap_or("-u").into(),
            tmux_options: match value.get("tmux_options") {
                None => defaults.tmux_options,
                Some(value) => value
                    .as_array()
                    .context("tmux_options must be an array")?
                    .iter()
                    .map(|entry| {
                        Ok(tmux::Entry {
                            enabled: entry["enabled"]
                                .as_bool()
                                .context("tmux entry needs an enabled flag")?,
                            command: entry["command"]
                                .as_str()
                                .context("tmux entry needs a command")?
                                .into(),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            },
            shell: value["shell"].as_str().unwrap_or(&defaults.shell).into(),
            tab_name: value["tab_name"].as_str().unwrap_or("").into(),
            terminal_tls_certificate: value["terminal_tls_certificate"]
                .as_str()
                .unwrap_or("")
                .into(),
            terminal_tls_private_key: value["terminal_tls_private_key"]
                .as_str()
                .unwrap_or("")
                .into(),
            editor_font_size: size("editor_font_size"),
            terminal_font_size: size("terminal_font_size"),
            dialog_font_size: size("dialog_font_size"),
            search_results_font_size: size("search_results_font_size"),
            search_filters_font_size: size("search_filters_font_size"),
        };
        if settings.tmux_options == tmux::legacy_defaults()
            || settings.tmux_options == tmux::previous_defaults()
        {
            settings.tmux_options = tmux::defaults();
        }
        Ok(settings)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        self.tmux_config()?;
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
        value["tmux_enabled"] = self.tmux_enabled.into();
        value["terminal_backend"] = self.terminal_backend.clone().into();
        value["tmux_client_args"] = self.tmux_client_args.clone().into();
        value["tmux_options"] = self
            .tmux_options
            .iter()
            .map(|entry| serde_json::json!({"enabled": entry.enabled, "command": entry.command}))
            .collect::<Vec<_>>()
            .into();
        value["terminal_tls_certificate"] = self.terminal_tls_certificate.clone().into();
        value["terminal_tls_private_key"] = self.terminal_tls_private_key.clone().into();
        value["editor_font_size"] = self.editor_font_size.into();
        value["terminal_font_size"] = self.terminal_font_size.into();
        value["dialog_font_size"] = self.dialog_font_size.into();
        value["search_results_font_size"] = self.search_results_font_size.into();
        value["search_filters_font_size"] = self.search_filters_font_size.into();
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        let bytes = serde_json::to_vec_pretty(&value)?;
        if bytes.len() > 65536 {
            bail!("Preferences file is too large");
        }
        temp.write_all(&bytes)?;
        temp.flush()?;
        temp.persist(path)?;
        Ok(())
    }
    pub fn load_saved() -> Result<Self> {
        let destination = path().context("No home directory for preferences")?;
        let previous = destination
            .parent()
            .and_then(Path::parent)
            .map(|home| home.join(".coder/settings.json"));
        let legacy = previous.filter(|path| path.is_file()).or_else(legacy_path);
        Self::load_or_migrate(&destination, legacy.as_deref())
    }
    fn load_or_migrate(path: &Path, legacy: Option<&Path>) -> Result<Self> {
        ensure_global_ignore(
            &path
                .parent()
                .context("No preferences directory")?
                .join(".gitignore"),
        )?;
        if path.exists() {
            return Self::load(path);
        }
        let settings = if let Some(legacy) = legacy.filter(|path| path.exists()) {
            Self::load(legacy)?
        } else {
            Self::default()
        };
        if let Some(legacy) = legacy.filter(|legacy| legacy.is_file()) {
            let parent = path.parent().context("No preferences directory")?;
            std::fs::create_dir_all(parent)?;
            let mut temp = tempfile::NamedTempFile::new_in(parent)?;
            serde_json::to_writer_pretty(&mut temp, &json(legacy)?)?;
            temp.flush()?;
            temp.persist(path)?;
        }
        settings.save(path)?;
        Ok(settings)
    }
    pub fn apply(&self, ctx: &egui::Context) {
        ctx.data_mut(|data| {
            data.insert_temp(egui::Id::new("editor-font-size"), self.editor_font_size);
            data.insert_temp(egui::Id::new("dialog-font-size"), self.dialog_font_size);
            data.insert_temp(
                egui::Id::new("search-results-font-size"),
                self.search_results_font_size,
            );
            data.insert_temp(
                egui::Id::new("search-filters-font-size"),
                self.search_filters_font_size,
            );
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
        .unwrap_or(DEFAULT_FONT_SIZE)
}
pub fn dialog_font_size(ctx: &egui::Context) -> f32 {
    ctx.data(|data| data.get_temp(egui::Id::new("dialog-font-size")))
        .unwrap_or(DEFAULT_FONT_SIZE)
}
pub fn search_results_font_size(ctx: &egui::Context) -> f32 {
    ctx.data(|data| data.get_temp(egui::Id::new("search-results-font-size")))
        .unwrap_or(DEFAULT_FONT_SIZE)
}
pub fn search_filters_font_size(ctx: &egui::Context) -> f32 {
    ctx.data(|data| data.get_temp(egui::Id::new("search-filters-font-size")))
        .unwrap_or(DEFAULT_FONT_SIZE)
}
pub fn search_filter_style(ui: &mut egui::Ui) {
    let size = search_filters_font_size(ui.ctx());
    for style in [
        egui::TextStyle::Body,
        egui::TextStyle::Button,
        egui::TextStyle::Small,
    ] {
        ui.style_mut()
            .text_styles
            .insert(style, egui::FontId::proportional(size));
    }
}

pub fn line_height(ctx: &egui::Context) -> f32 {
    20.0 * editor_font_size(ctx) / 13.0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dialogs_use_their_own_font_for_fields_labels_and_buttons() {
        let ctx = egui::Context::default();
        let mut settings = Settings {
            editor_font_size: 24.0,
            dialog_font_size: 18.0,
            ..Default::default()
        };
        for size in [18.0, 28.0] {
            settings.dialog_font_size = size;
            settings.apply(&ctx);
            let output = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    crate::chrome::dialog_style(ui, 500.0);
                    assert_eq!(egui::TextStyle::Body.resolve(ui.style()).size, size);
                    assert_eq!(egui::TextStyle::Monospace.resolve(ui.style()).size, size);
                    ui.label("Dialog body");
                    crate::chrome::dialog_button(ui, "Confirm", None);
                });
            });
            for label in ["Dialog body", "Confirm"] {
                let text = output
                    .shapes
                    .iter()
                    .find_map(|shape| match &shape.shape {
                        egui::Shape::Text(text) if text.galley.text() == label => Some(text),
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(text.galley.job.sections[0].format.font_id.size, size);
            }
            assert_eq!(editor_font_size(&ctx), 24.0);
        }
    }

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
        settings.dialog_font_size = 19.0;
        settings.search_results_font_size = 22.0;
        settings.search_filters_font_size = 11.0;
        settings.save(&path)?;
        let loaded = Settings::load(&path)?;
        assert_eq!(loaded.editor_font_size, 17.5);
        assert_eq!(loaded.terminal_font_size, 16.0);
        assert_eq!(loaded.dialog_font_size, 19.0);
        assert_eq!(loaded.search_results_font_size, 22.0);
        assert_eq!(loaded.search_filters_font_size, 11.0);
        assert_eq!(json(&path)?["future_preference"], true);
        std::fs::write(&path, "broken json")?;
        assert!(settings.save(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path)?, "broken json");
        Ok(())
    }
    #[test]
    fn https_preferences_roundtrip_and_can_be_reset_to_generated_defaults() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let certificate = directory.path().join("certificate with spaces.pem");
        let private_key = directory.path().join("private key.pem");
        std::fs::write(&certificate, "certificate")?;
        std::fs::write(&private_key, "key")?;
        let mut settings = Settings {
            terminal_tls_certificate: certificate.to_string_lossy().into_owned(),
            terminal_tls_private_key: private_key.to_string_lossy().into_owned(),
            ..Default::default()
        };
        settings.validate_terminal_tls()?;
        let path = directory.path().join("settings.json");
        settings.save(&path)?;
        let loaded = Settings::load(&path)?;
        assert_eq!(
            loaded.terminal_tls_certificate,
            settings.terminal_tls_certificate
        );
        assert_eq!(
            loaded.terminal_tls_private_key,
            settings.terminal_tls_private_key
        );
        settings.terminal_tls_private_key.clear();
        assert!(settings.validate_terminal_tls().is_err());
        settings.terminal_tls_certificate.clear();
        settings.validate_terminal_tls()?;
        settings.save(&path)?;
        let reset = Settings::load(&path)?;
        assert!(reset.terminal_tls_certificate.is_empty());
        assert!(reset.terminal_tls_private_key.is_empty());
        Ok(())
    }

    #[test]
    fn legacy_settings_migrate_once_and_defaults_are_saved() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let old = dir.path().join("terminal.json");
        let path = dir.path().join(".glimcode/settings.json");
        std::fs::write(
            &old,
            r#"{"shell":"/bin/sh","tab_name":"Local","future_setting":true}"#,
        )?;
        let mut settings = Settings::load_or_migrate(&path, Some(&old))?;
        assert_eq!(settings.shell, "/bin/sh");
        assert_eq!(settings.tab_name, "Local");
        assert_eq!(settings.editor_font_size, 14.0);
        assert_eq!(settings.dialog_font_size, 14.0);
        assert_eq!(settings.search_results_font_size, 14.0);
        assert_eq!(settings.search_filters_font_size, 14.0);
        assert!(path.is_file());
        assert!(old.is_file());
        assert_eq!(json(&path)?["future_setting"], true);
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
    fn global_ignore_is_created_once_without_overwriting_user_rules() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let settings = dir.path().join(".glimcode/settings.json");
        let ignore = settings.parent().unwrap().join(".gitignore");
        Settings::load_or_migrate(&settings, None)?;
        assert!(std::fs::read_to_string(&ignore)?.starts_with("# Extra ignore rules"));
        std::fs::write(&ignore, "*.log\ncache/\n")?;
        Settings::load_or_migrate(&settings, None)?;
        assert_eq!(std::fs::read_to_string(ignore)?, "*.log\ncache/\n");
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
                    std::sync::Arc::new(preview.line_for_ui(ui, 0, "")),
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
