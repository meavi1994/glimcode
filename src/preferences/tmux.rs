use super::*;

pub(crate) const MOUSE_EVENTS: &[&str] = &[
    "WheelUpPane",
    "WheelDownPane",
    "MouseDown1Pane",
    "MouseUp1Pane",
    "MouseDrag1Pane",
    "MouseDragEnd1Pane",
    "MouseDown2Pane",
    "MouseUp2Pane",
    "MouseDrag2Pane",
    "MouseDragEnd2Pane",
    "MouseDown3Pane",
    "MouseUp3Pane",
    "MouseDrag3Pane",
    "MouseDragEnd3Pane",
    "DoubleClick1Pane",
    "TripleClick1Pane",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub enabled: bool,
    pub command: String,
}

pub fn defaults() -> Vec<Entry> {
    let mut entries = previous_defaults();
    // -q keeps older tmux versions that lack this option usable.
    entries.push(Entry {
        enabled: true,
        command: "set -gq copy-mode-position-format ''".into(),
    });
    entries
}

pub(super) fn previous_defaults() -> Vec<Entry> {
    ["set -g status off", "set -g mouse on"]
        .into_iter()
        .map(|command| Entry {
            enabled: true,
            command: command.into(),
        })
        .collect()
}

pub(super) fn legacy_defaults() -> Vec<Entry> {
    let mut commands: Vec<String> = [
        "set -g status off",
        "set -g prefix None",
        "set -g prefix2 None",
        "unbind-key -q -a -T prefix",
        "unbind-key -q -a -T root",
        "set -g mouse off",
        "set -g default-shell /bin/sh",
        "set -g default-terminal xterm-256color",
        "set -g history-limit 5000",
        "set -g destroy-unattached off",
        "set -g set-titles on",
        "set -g set-titles-string '#{pane_title}'",
        "set -g focus-events on",
        "set -s escape-time 0",
        "set -s extended-keys on",
        "set -s terminal-overrides 'xterm-256color:smcup@:rmcup@:XT:RGB'",
    ]
    .map(str::to_owned)
    .to_vec();
    commands.extend(
        MOUSE_EVENTS
            .iter()
            .map(|event| format!("bind-key -T root {event} send-keys -M")),
    );
    commands.push("if-shell -F '#{==:#{pane_mode},copy-mode}' 'send-keys -X cancel'".into());
    commands
        .into_iter()
        .map(|command| Entry {
            enabled: true,
            command,
        })
        .collect()
}

impl Settings {
    pub fn tmux_arguments(&self) -> Result<Vec<String>> {
        let args =
            shell_words::split(&self.tmux_client_args).context("Invalid tmux client flags")?;
        if args.iter().any(|arg| {
            ["-L", "-S", "-f"]
                .iter()
                .any(|reserved| arg.starts_with(reserved))
        }) {
            bail!(
                "Glimcode manages tmux's server socket and configuration file; omit -L, -S and -f from client flags"
            );
        }
        Ok(args)
    }

    pub fn tmux_config(&self) -> Result<String> {
        self.tmux_arguments()?;
        if self.tmux_options.len() > 256 {
            bail!("Use at most 256 tmux configuration entries");
        }
        let mut config = String::new();
        let mut total = 0;
        for entry in &self.tmux_options {
            total += entry.command.len();
            if entry.command.contains('\0') {
                bail!("tmux configuration cannot contain NUL characters");
            }
            if entry.enabled && !entry.command.trim().is_empty() {
                config.push_str(&entry.command);
                config.push('\n');
            }
        }
        if total > 48 * 1024 {
            bail!("tmux configuration must fit within 48 KiB");
        }
        Ok(config)
    }

    pub fn tmux_preferences(&mut self, ui: &mut egui::Ui) {
        crate::chrome::dialog_section(
            ui,
            "tmux",
            "Customize persistence, tmux options and key bindings.",
        );
        crate::chrome::dialog_card(ui, |ui| {
            ui.label("Choose the persistence backend in Terminal settings. These options apply only to tmux sessions.");
            ui.label("tmux client flags");
            ui.add(
                egui::TextEdit::singleline(&mut self.tmux_client_args)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY)
                    .hint_text("-u"),
            );
            ui.small("Arguments are passed directly to tmux. Glimcode manages the server socket and configuration file.");
            egui::CollapsingHeader::new(format!("Configuration entries ({})", self.tmux_options.len()))
                .id_salt("tmux-preferences-entries").show(ui, |ui| {
                    ui.label("Commands run in order on each connection, on Glimcode’s private server. Edit any tmux command, or add your own. Disabled entries are skipped; tmux retains previously applied values until explicitly changed.");
                    let mut remove = None;
                    let mut move_entry = None;
                    let len = self.tmux_options.len();
                    for (index, entry) in self.tmux_options.iter_mut().enumerate() {
                        ui.push_id(index, |ui| {
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut entry.enabled, "").on_hover_text("Apply this entry");
                                ui.label(format!("{}", index + 1));
                                if ui.add_enabled(index > 0, egui::Button::new("↑")).on_hover_text("Move earlier").clicked() { move_entry = Some((index, index - 1)); }
                                if ui.add_enabled(index + 1 < len, egui::Button::new("↓")).on_hover_text("Move later").clicked() { move_entry = Some((index, index + 1)); }
                                if ui.button("Remove").clicked() { remove = Some(index); }
                            });
                            ui.add(egui::TextEdit::multiline(&mut entry.command).code_editor().desired_rows(1).desired_width(f32::INFINITY).hint_text("set -g option value"));
                            ui.add_space(6.0);
                        });
                    }
                    if let Some(index) = remove { self.tmux_options.remove(index); }
                    else if let Some((a, b)) = move_entry { self.tmux_options.swap(a, b); }
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Add command").clicked() { self.tmux_options.push(Entry { enabled: true, command: String::new() }); }
                        if ui.button("Enable all").clicked() { for entry in &mut self.tmux_options { entry.enabled = true; } }
                        if ui.button("Disable all").clicked() { for entry in &mut self.tmux_options { entry.enabled = false; } }
                        if ui.button("Restore Glimcode defaults").clicked() { self.tmux_options = defaults(); }
                        if ui.button("Copy active configuration").clicked() && let Ok(config) = self.tmux_config() { ui.ctx().copy_text(config); }
                    });
                });
            ui.label("Saved in ~/.glimcode/settings.json. Configuration changes apply to new or reconnected terminals; running shells stay connected.");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_backend_roundtrips_without_changing_tmux_configuration() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("settings.json");
        let mut settings = Settings::default();
        settings.terminal_backend = "zmx".into();
        settings.save(&path)?;
        let loaded = Settings::load(&path)?;
        assert_eq!(loaded.terminal_backend, "zmx");
        assert_eq!(loaded.tmux_options, settings.tmux_options);
        assert!(loaded.tmux_enabled);
        Ok(())
    }

    #[test]
    fn legacy_defaults_migrate_but_custom_configuration_is_preserved() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("settings.json");
        let mut settings = Settings::default();
        for entries in [legacy_defaults(), previous_defaults()] {
            settings.tmux_options = entries;
            settings.save(&path)?;
            assert_eq!(Settings::load(&path)?.tmux_options, defaults());
            settings.tmux_options[0].command = "set -g status on".into();
            settings.save(&path)?;
            assert_eq!(Settings::load(&path)?.tmux_options, settings.tmux_options);
        }
        Ok(())
    }

    #[test]
    fn tmux_preferences_preserve_disabled_entries_order_flags_and_unknown_keys() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"future_setting":42}"#)?;
        let mut settings = Settings::load(&path)?;
        assert!(settings.tmux_enabled);
        assert_eq!(settings.tmux_options, defaults());
        settings.tmux_enabled = false;
        settings.tmux_client_args = "-2 -u".into();
        settings.tmux_options = vec![
            Entry {
                enabled: false,
                command: "set -g mouse off".into(),
            },
            Entry {
                enabled: true,
                command: "set -g mouse on\nset -g status on".into(),
            },
            Entry {
                enabled: true,
                command: "bind-key -T root WheelUpPane send-keys -M".into(),
            },
        ];
        settings.save(&path)?;
        let loaded = Settings::load(&path)?;
        assert!(!loaded.tmux_enabled);
        assert_eq!(loaded.tmux_options, settings.tmux_options);
        assert_eq!(loaded.tmux_arguments()?, vec!["-2", "-u"]);
        assert_eq!(
            loaded.tmux_config()?,
            "set -g mouse on\nset -g status on\nbind-key -T root WheelUpPane send-keys -M\n"
        );
        assert_eq!(super::super::json(&path)?["future_setting"], 42);
        settings.tmux_options.clear();
        settings.tmux_client_args.clear();
        settings.save(&path)?;
        let loaded = Settings::load(&path)?;
        assert!(loaded.tmux_options.is_empty());
        assert!(loaded.tmux_arguments()?.is_empty());
        assert!(loaded.tmux_config()?.is_empty());
        Ok(())
    }

    #[test]
    fn tmux_configuration_rejects_invalid_flags_and_never_overwrites_saved_settings() -> Result<()>
    {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("settings.json");
        let mut settings = Settings::default();
        settings.save(&path)?;
        let original = std::fs::read(&path)?;
        for args in [
            "'unclosed",
            "-L personal",
            "-S/private/socket",
            "-f /tmp/config",
        ] {
            settings.tmux_client_args = args.into();
            assert!(settings.save(&path).is_err());
            assert_eq!(std::fs::read(&path)?, original);
        }
        settings.tmux_client_args.clear();
        settings.tmux_options = vec![Entry {
            enabled: true,
            command: "invalid\0command".into(),
        }];
        assert!(settings.save(&path).is_err());
        std::fs::write(&path, r#"{"tmux_options":[{"enabled":true}]}"#)?;
        assert!(Settings::load(&path).is_err());
        Ok(())
    }
}
