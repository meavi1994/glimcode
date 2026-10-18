use eframe::egui::{self, text::LayoutJob};
use egui_extras::syntax_highlighting::{CodeTheme, SyntectSettings, highlight_with};
use std::sync::OnceLock;
use syntect::highlighting::{Color, FontStyle, StyleModifier, Theme, ThemeItem, ThemeSettings};

fn color(hex: u32) -> Color {
    Color {
        r: (hex >> 16) as u8,
        g: (hex >> 8) as u8,
        b: hex as u8,
        a: 255,
    }
}

fn theme(dark: bool) -> Theme {
    let mut theme = Theme {
        name: Some(
            if dark {
                "Coder Darcula"
            } else {
                "Coder IntelliJ Light"
            }
            .into(),
        ),
        settings: ThemeSettings {
            foreground: Some(color(if dark { 0xA9B7C6 } else { 0x000000 })),
            background: Some(color(if dark { 0x2B2B2B } else { 0xFFFFFF })),
            ..Default::default()
        },
        ..Default::default()
    };
    for (scope, dark_color, light_color, italic) in [
        ("comment", 0x7A7E85, 0x808080, true),
        ("keyword, storage", 0xCC7832, 0x000080, false),
        ("string", 0x6AAB73, 0x008000, false),
        (
            "meta.structure.dictionary.key.json string, meta.structure.dictionary.key.json punctuation.definition.string, support.type.property-name.json",
            0xC77DBB,
            0x660E7A,
            false,
        ),
        (
            "meta.structure.dictionary.value.json string.quoted.double.json punctuation.definition.string",
            0x6AAB73,
            0x008000,
            false,
        ),
        ("constant.numeric", 0x6897BB, 0x0000FF, false),
        ("constant.language", 0xCC7832, 0x000080, false),
        (
            "entity.name.function, support.function",
            0xFFC66D,
            0x7A7A43,
            false,
        ),
        (
            "entity.name.type, entity.name.class, support.type, support.class",
            0xA9B7C6,
            0x000000,
            false,
        ),
        (
            "variable.other.member, variable.other.field",
            0x9876AA,
            0x660E7A,
            false,
        ),
        (
            "constant.other, variable.other.constant",
            0x9876AA,
            0x660E7A,
            false,
        ),
        (
            "meta.annotation, entity.name.tag",
            0xD5B778,
            0x808000,
            false,
        ),
        ("entity.other.attribute-name", 0xBABABA, 0x000080, false),
        (
            "entity.name.section, markup.heading",
            0x56A8F5,
            0x000080,
            false,
        ),
        ("keyword.operator", 0xBCBEC4, 0x000000, false),
        ("punctuation", 0xA9B7C6, 0x000000, false),
        (
            "entity.name.type, entity.name.class, support.type, support.class",
            0x56A8B8,
            0x2B6C80,
            false,
        ),
        ("variable.parameter", 0xB9A3CC, 0x660E7A, false),
        (
            "entity.name.namespace, entity.name.module, entity.name.package, support.module",
            0xA9B665,
            0x5B7400,
            false,
        ),
        (
            "storage.type.primitive, support.type.primitive, variable.language",
            0xCC7832,
            0x000080,
            false,
        ),
        (
            "constant.character.escape, constant.other.placeholder",
            0xCC7832,
            0x000080,
            false,
        ),
        (
            "entity.name.function.preprocessor, entity.name.function.macro, meta.annotation, entity.name.annotation",
            0xBBB529,
            0x808000,
            false,
        ),
    ] {
        theme.scopes.push(ThemeItem {
            scope: scope.parse().expect("built-in syntax selector"),
            style: StyleModifier {
                foreground: Some(color(if dark { dark_color } else { light_color })),
                font_style: Some(if italic {
                    FontStyle::ITALIC
                } else {
                    FontStyle::empty()
                }),
                ..Default::default()
            },
        });
    }
    theme
}

fn settings() -> &'static SyntectSettings {
    static SETTINGS: OnceLock<SyntectSettings> = OnceLock::new();
    SETTINGS.get_or_init(|| {
        let mut settings = SyntectSettings::default();
        // These are the stable keys used by CodeTheme::dark/light.
        settings
            .ts
            .themes
            .insert("base16-mocha.dark".into(), theme(true));
        settings
            .ts
            .themes
            .insert("Solarized (light)".into(), theme(false));
        settings
    })
}

/// Worker-thread highlighting: no egui context/cache lock is held while scanning a file.
pub fn background(
    text: &str,
    extension: &str,
    dark: bool,
    cancelled: &impl Fn() -> bool,
) -> Option<LayoutJob> {
    use syntect::{easy::HighlightLines, util::LinesWithEndings};
    if matches!(
        extension.to_lowercase().as_str(),
        "java"
            | "kt"
            | "kts"
            | "go"
            | "rs"
            | "py"
            | "js"
            | "mjs"
            | "cjs"
            | "ts"
            | "tsx"
            | "jsx"
            | "mts"
            | "cts"
    ) {
        return typed_highlight(text, extension, dark, cancelled);
    }
    let settings = settings();
    let extension = extension.to_lowercase();
    let language = match extension.as_str() {
        "ts" | "tsx" | "jsx" | "mjs" | "cjs" => "js",
        "markdown" => "md",
        "mts" | "cts" => "js",
        "bash" | "zsh" | "fish" => "sh",
        "yml" => "yaml",
        "htm" | "vue" | "svelte" => "html",
        "dockerfile" => "Dockerfile",
        other => other,
    };
    let syntax = settings
        .ps
        .find_syntax_by_extension(language)
        .unwrap_or_else(|| settings.ps.find_syntax_plain_text());
    let theme = &settings.ts.themes[if dark {
        "base16-mocha.dark"
    } else {
        "Solarized (light)"
    }];
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut job = LayoutJob {
        text: text.to_owned(),
        ..Default::default()
    };
    let mut offset = 0;
    for line in LinesWithEndings::from(text) {
        if cancelled() {
            return None;
        }
        for (style, part) in highlighter.highlight_line(line, &settings.ps).ok()? {
            let start = offset;
            offset += part.len();
            job.sections.push(egui::text::LayoutSection {
                leading_space: 0.0,
                byte_range: start..offset,
                format: egui::TextFormat {
                    font_id: egui::FontId::monospace(13.0),
                    color: egui::Color32::from_rgb(
                        style.foreground.r,
                        style.foreground.g,
                        style.foreground.b,
                    ),
                    italics: style.font_style.contains(FontStyle::ITALIC),
                    ..Default::default()
                },
            });
        }
    }
    if extension == "json" {
        normalize_json_quotes(&mut job, dark);
    }
    (!cancelled()).then_some(job)
}

pub fn highlight(ui: &egui::Ui, text: &str, extension: &str) -> LayoutJob {
    let mut job = highlight_base(ui, text, extension);
    for section in &mut job.sections {
        section.format.font_id.size = crate::preferences::editor_font_size(ui.ctx());
    }
    job
}

/// Match outline entries to the exact foreground painted on their declaration
/// in the editor, including language-specific and theme-specific overrides.
pub fn symbol_colors(
    job: &LayoutJob,
    symbols: &[&crate::navigation::Symbol],
) -> Vec<egui::Color32> {
    let starts: Vec<_> = std::iter::once(0)
        .chain(job.text.match_indices('\n').map(|(index, _)| index + 1))
        .collect();
    symbols
        .iter()
        .map(|symbol| {
            let byte = starts
                .get(symbol.line.saturating_sub(1))
                .copied()
                .unwrap_or(job.text.len())
                + symbol.column.saturating_sub(1);
            let index = job
                .sections
                .partition_point(|section| section.byte_range.end <= byte);
            job.sections
                .get(index)
                .filter(|section| section.byte_range.contains(&byte))
                .map_or(crate::chrome::TEXT, |section| section.format.color)
        })
        .collect()
}

pub fn symbol_badge(kind: &str) -> &'static str {
    if kind.contains("constructor") {
        "C"
    } else if kind.contains("function") || kind.contains("method") {
        "f"
    } else if kind.contains("enum") {
        "E"
    } else if kind.contains("struct") || kind.contains("record") {
        "S"
    } else if kind.contains("class") || kind.contains("object") {
        "C"
    } else if kind.contains("interface") || kind.contains("trait") {
        "I"
    } else if kind.contains("type") {
        "T"
    } else if kind.contains("mod_") {
        "M"
    } else if kind.contains("const") || kind.contains("static") {
        "#"
    } else {
        "v"
    }
}

fn highlight_base(ui: &egui::Ui, text: &str, extension: &str) -> LayoutJob {
    if matches!(
        extension.to_lowercase().as_str(),
        "java"
            | "kt"
            | "kts"
            | "go"
            | "rs"
            | "py"
            | "js"
            | "mjs"
            | "cjs"
            | "ts"
            | "tsx"
            | "jsx"
            | "mts"
            | "cts"
    ) {
        return ui.ctx().memory_mut(|memory| {
            memory
                .caches
                .cache::<egui::cache::FrameCache<LayoutJob, TypedHighlighter>>()
                .get((text, extension, ui.visuals().dark_mode))
        });
    }
    let theme = if ui.visuals().dark_mode {
        CodeTheme::dark(13.0)
    } else {
        CodeTheme::light(13.0)
    };
    let extension = extension.to_lowercase();
    let language = match extension.as_str() {
        "ts" | "tsx" | "jsx" | "mjs" | "cjs" => "js",
        "markdown" => "md",
        "mts" | "cts" => "js",
        "bash" | "zsh" | "fish" => "sh",
        "yml" => "yaml",
        "htm" | "vue" | "svelte" => "html",
        "dockerfile" => "Dockerfile",
        other => other,
    };
    let mut job = highlight_with(ui.ctx(), ui.style(), &theme, text, language, settings());
    // egui_extras also underlines italics; keep comments italic without that decoration.
    for section in &mut job.sections {
        section.format.underline = egui::Stroke::NONE;
    }
    if extension == "json" {
        normalize_json_quotes(&mut job, ui.visuals().dark_mode);
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn structure_colors_match_editor_declarations_across_languages_and_themes() {
        for (extension, source) in [
            (
                "rs",
                "// λ\nstruct Widget {}\nconst LIMIT: usize = 3;\nfn render() {}\n",
            ),
            ("java", "class Widget { int value; void render() {} }"),
            (
                "go",
                "package demo\ntype Widget struct {}\nconst Limit = 3\nfunc render() {}",
            ),
            (
                "ts",
                "class Widget {}\nfunction render() {}\nconst limit = 3;",
            ),
        ] {
            let path = std::path::PathBuf::from(format!("sample.{extension}"));
            let symbols = crate::navigation::symbols(&path, source);
            assert!(!symbols.is_empty());
            for dark in [true, false] {
                let job = background(source, extension, dark, &|| false).unwrap();
                let refs: Vec<_> = symbols.iter().collect();
                let colors = symbol_colors(&job, &refs);
                for (symbol, actual) in symbols.iter().zip(colors) {
                    let offset = source
                        .split_inclusive('\n')
                        .take(symbol.line - 1)
                        .map(str::len)
                        .sum::<usize>()
                        + symbol.column
                        - 1;
                    let expected = job
                        .sections
                        .iter()
                        .find(|section| section.byte_range.contains(&offset))
                        .unwrap()
                        .format
                        .color;
                    assert_eq!(actual, expected, "{extension} {}", symbol.name);
                }
                if extension == "rs" && dark {
                    let colors = symbol_colors(&job, &refs);
                    assert!(colors.contains(&egui::Color32::from_rgb(0xFF, 0xC6, 0x6D)));
                    assert!(colors.contains(&egui::Color32::from_rgb(0x56, 0xA8, 0xF5)));
                }
            }
        }
    }

    #[test]
    fn json_opening_and_closing_quotes_follow_their_key_or_value() {
        let source = r#"{
  "expo": {
    "name": "metronome-app",
    "note": "escaped \"quoted\" 猫",
    "flags": ["one", "two"]
  }
}"#;
        for dark in [true, false] {
            let job = background(source, "json", dark, &|| false).unwrap();
            for (quoted, key) in [
                (r#""expo""#, true),
                (r#""name""#, true),
                (r#""metronome-app""#, false),
                (r#""flags""#, true),
                (r#""one""#, false),
            ] {
                let start = source.find(quoted).unwrap();
                let hex = match (dark, key) {
                    (true, true) => 0xC77DBB,
                    (false, true) => 0x660E7A,
                    (true, false) => 0x6AAB73,
                    _ => 0x008000,
                };
                let c = color(hex);
                for offset in [start, start + quoted.len() - 1] {
                    let section = job
                        .sections
                        .iter()
                        .find(|s| s.byte_range.contains(&offset))
                        .unwrap();
                    assert_eq!(
                        section.format.color,
                        egui::Color32::from_rgb(c.r, c.g, c.b),
                        "{quoted}: quote at {offset}"
                    );
                }
            }
        }
    }

    #[test]
    fn richer_roles_work_across_parser_languages_in_both_themes() {
        for (extension, source, tokens) in [
            (
                "rs",
                "fn render(value: &str) { println!(\"hello\\n\"); }",
                vec![
                    ("value", 0xB9A3CC),
                    ("str", 0xCC7832),
                    ("println", 0xBBB529),
                    ("\\n", 0xCC7832),
                ],
            ),
            (
                "py",
                "@cached\ndef render(value: str):\n    print(\"hello\\n\")",
                vec![("cached", 0xBBB529), ("value", 0xB9A3CC), ("\\n", 0xCC7832)],
            ),
            (
                "js",
                "function render(value) { return this.field + \"hello\\n\"; }",
                vec![
                    ("value", 0xB9A3CC),
                    ("field", 0xC77DBB),
                    ("this", 0xCC7832),
                    ("\\n", 0xCC7832),
                ],
            ),
            (
                "ts",
                "function render(value: string): boolean { return true; }",
                vec![
                    ("value", 0xB9A3CC),
                    ("string", 0xCC7832),
                    ("true", 0xCC7832),
                ],
            ),
            (
                "java",
                "class View { int field; String render(String value) { return value + \"hi\\n\"; } }",
                vec![
                    ("View", 0x56A8F5),
                    ("field", 0xC77DBB),
                    ("value", 0xB9A3CC),
                    ("int", 0xCC7832),
                    ("\\n", 0xCC7832),
                ],
            ),
            (
                "kt",
                "package demo\nclass View(private val prefix: String) { fun render(value: String): String { return \"hi\\n\" } }",
                vec![
                    ("View", 0x56A8F5),
                    ("render", 0xFFC66D),
                    ("value", 0xB9A3CC),
                    ("\\n", 0xCC7832),
                ],
            ),
            (
                "kts",
                "val answer = 42\nprintln(answer)",
                vec![("42", 0x6897BB), ("println", 0xFFC66D)],
            ),
        ] {
            for dark in [true, false] {
                let job = background(source, extension, dark, &|| false).expect(extension);
                assert_eq!(job.text, source);
                assert!(
                    job.sections
                        .iter()
                        .all(|section| source.is_char_boundary(section.byte_range.start)
                            && source.is_char_boundary(section.byte_range.end))
                );
                if dark {
                    for (token, expected) in &tokens {
                        let position = source.find(token).unwrap();
                        let section = job
                            .sections
                            .iter()
                            .find(|s| s.byte_range.contains(&position))
                            .unwrap();
                        let c = color(*expected);
                        assert_eq!(
                            section.format.color,
                            egui::Color32::from_rgb(c.r, c.g, c.b),
                            "{extension}: {token}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn go_packages_receivers_constants_and_calls_have_separate_roles() {
        let source = r#"package main
import "flag"
const Filename = "CHANGELOG.md"
type exitError struct { exitCode int }
func (e exitError) Error() string { return "" }
func main() { flag.String("name", "value", "help"); enabled := false; _ = enabled }
func local(flag Config) { flag.String() }
"#;
        let job = background(source, "go", true, &|| false).unwrap();
        for (token, expected) in [
            ("Filename", 0x9876AA),
            ("int", 0xCC7832),
            ("(e exitError", 0x56A8B8),
            ("flag.String", 0xA9B665),
            ("String(\"name", 0xB9A979),
            ("false", 0xCC7832),
        ] {
            let offset = source.find(token).unwrap() + if token == "(e exitError" { 3 } else { 0 };
            let section = job
                .sections
                .iter()
                .find(|s| s.byte_range.contains(&offset))
                .unwrap();
            let c = color(expected);
            assert_eq!(
                section.format.color,
                egui::Color32::from_rgb(c.r, c.g, c.b),
                "{token}"
            );
        }
        let local = source.rfind("flag.String").unwrap();
        let format = &job
            .sections
            .iter()
            .find(|s| s.byte_range.contains(&local))
            .unwrap()
            .format;
        assert_ne!(
            format.color,
            egui::Color32::from_rgb(0xA9, 0xB6, 0x65),
            "local variables must not be colored as packages"
        );
    }

    #[test]
    fn bundled_syntax_languages_use_shared_richer_theme() {
        for (extension, source) in [
            (
                "cpp",
                "class View { public: int render(int value) { return value; } };",
            ),
            (
                "cs",
                "class View { int Render(int value) { return value; } }",
            ),
            (
                "rb",
                "class View\n  def render(value)\n    value\n  end\nend",
            ),
            ("sh", "echo \"hello $USER\" # comment"),
            ("sql", "SELECT name FROM users WHERE id = 42;"),
        ] {
            let job = background(source, extension, true, &|| false).unwrap();
            assert_eq!(job.text, source);
            let colors: std::collections::HashSet<_> =
                job.sections.iter().map(|s| s.format.color).collect();
            assert!(
                colors.len() >= 2,
                "{extension} should retain syntax distinctions"
            );
        }
    }

    #[test]
    fn parsed_languages_distinguish_calls_fields_and_literals() {
        for (extension, source, tokens) in [
            (
                "go",
                "package main\nfunc main() { server.Listen(\"hello\") }",
                vec![("Listen", 0xB9A979), ("hello", 0x6AAB73)],
            ),
            (
                "rs",
                "fn main() { user.render(); }",
                vec![("render", 0xFFC66D)],
            ),
            (
                "py",
                "def main():\n    user.render(\"hello\")",
                vec![("render", 0xFFC66D), ("hello", 0x6AAB73)],
            ),
            (
                "js",
                "const view = user.render(\"hello\");",
                vec![("render", 0xFFC66D), ("hello", 0x6AAB73)],
            ),
        ] {
            for dark in [false, true] {
                let job = background(source, extension, dark, &|| false).unwrap();
                assert_eq!(job.text, source);
                if dark {
                    for (token, expected) in &tokens {
                        let offset = source.find(token).unwrap();
                        let section = job
                            .sections
                            .iter()
                            .find(|s| s.byte_range.contains(&offset))
                            .unwrap();
                        let c = color(*expected);
                        assert_eq!(
                            section.format.color,
                            egui::Color32::from_rgb(c.r, c.g, c.b),
                            "{extension}: {token}"
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn typescript_and_jsx_use_native_grammars() {
        for (extension, source) in [
            ("ts", "interface User { name: string }"),
            ("tsx", "const view = <Button label=\"Hello\" />;"),
        ] {
            let job = background(source, extension, true, &|| false).unwrap();
            assert_eq!(job.text, source);
            assert!(
                job.sections
                    .iter()
                    .any(|s| s.format.color != crate::chrome::TEXT)
            );
        }
    }
    #[test]
    fn json_properties_and_values_use_distinct_intellij_colors() {
        let source = r#"{"short_name":"React App","icons":[{"src":"favicon.ico"}],"count":12,"enabled":true}"#;
        let job = background(source, "json", true, &|| false).unwrap();
        for (token, expected) in [
            ("short_name", 0xC77DBB),
            ("React App", 0x6AAB73),
            ("src", 0xC77DBB),
            ("favicon.ico", 0x6AAB73),
        ] {
            let offset = source.find(token).unwrap();
            let section = job
                .sections
                .iter()
                .find(|section| section.byte_range.contains(&offset))
                .unwrap();
            let color = color(expected);
            assert_eq!(
                section.format.color,
                egui::Color32::from_rgb(color.r, color.g, color.b),
                "token {token}"
            );
        }
    }
    #[test]
    fn darcula_has_distinct_keyword_string_and_comment_colors() {
        let ctx = egui::Context::default();
        ctx.set_visuals(egui::Visuals::dark());
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let job = highlight(ui, "fn main() { let s = \"hello\"; } // note\n", "rs");
                let colors: Vec<_> = job.sections.iter().map(|s| s.format.color).collect();
                for hex in [0xCC7832, 0x6AAB73, 0x7A7E85] {
                    let c = color(hex);
                    assert!(colors.contains(&egui::Color32::from_rgb(c.r, c.g, c.b)));
                }
            });
        });
    }
}

#[derive(Default)]
struct TypedHighlighter;
impl egui::cache::ComputerMut<(&str, &str, bool), LayoutJob> for TypedHighlighter {
    fn compute(&mut self, (text, extension, dark): (&str, &str, bool)) -> LayoutJob {
        typed_highlight(text, extension, dark, &|| false).unwrap_or_else(|| {
            LayoutJob::simple(
                text.into(),
                egui::FontId::monospace(13.0),
                crate::chrome::TEXT,
                f32::INFINITY,
            )
        })
    }
}

fn typed_highlight(
    text: &str,
    extension: &str,
    dark: bool,
    cancelled: &impl Fn() -> bool,
) -> Option<LayoutJob> {
    use std::collections::BTreeMap;
    use streaming_iterator::StreamingIterator;
    let extension = extension.to_lowercase();
    static JAVA: OnceLock<tree_sitter::Query> = OnceLock::new();
    static KOTLIN: OnceLock<tree_sitter::Query> = OnceLock::new();
    static GO: OnceLock<tree_sitter::Query> = OnceLock::new();
    static RUST: OnceLock<tree_sitter::Query> = OnceLock::new();
    static PYTHON: OnceLock<tree_sitter::Query> = OnceLock::new();
    static JS: OnceLock<tree_sitter::Query> = OnceLock::new();
    static TS: OnceLock<tree_sitter::Query> = OnceLock::new();
    static TSX: OnceLock<tree_sitter::Query> = OnceLock::new();
    let (language, query_cache, source): (tree_sitter::Language, _, String) =
        match extension.as_str() {
            "java" => (
                tree_sitter_java::LANGUAGE.into(),
                &JAVA,
                format!(
                    "{}\n{}",
                    tree_sitter_java::HIGHLIGHTS_QUERY,
                    include_str!("queries/java-highlights.scm")
                ),
            ),
            "kt" | "kts" => (
                tree_sitter_kotlin_ng::LANGUAGE.into(),
                &KOTLIN,
                include_str!("queries/kotlin-highlights.scm").into(),
            ),
            "go" => (
                tree_sitter_go::LANGUAGE.into(),
                &GO,
                format!(
                    "{}\n{}",
                    tree_sitter_go::HIGHLIGHTS_QUERY,
                    include_str!("queries/go-highlights.scm")
                ),
            ),
            "rs" => (
                tree_sitter_rust::LANGUAGE.into(),
                &RUST,
                tree_sitter_rust::HIGHLIGHTS_QUERY.into(),
            ),
            "py" => (
                tree_sitter_python::LANGUAGE.into(),
                &PYTHON,
                format!(
                    "{}\n{}",
                    tree_sitter_python::HIGHLIGHTS_QUERY,
                    include_str!("queries/python-highlights.scm")
                ),
            ),
            "js" | "mjs" | "cjs" => (
                tree_sitter_javascript::LANGUAGE.into(),
                &JS,
                format!(
                    "{}\n{}",
                    tree_sitter_javascript::HIGHLIGHT_QUERY,
                    include_str!("queries/javascript-highlights.scm")
                ),
            ),
            other => {
                let jsx = matches!(other, "tsx" | "jsx");
                let language = if jsx {
                    tree_sitter_typescript::LANGUAGE_TSX
                } else {
                    tree_sitter_typescript::LANGUAGE_TYPESCRIPT
                };
                let mut source = tree_sitter_javascript::HIGHLIGHT_QUERY.to_owned();
                source.push_str("\n(escape_sequence) @escape\n");
                source.push_str(tree_sitter_typescript::HIGHLIGHTS_QUERY);
                if jsx {
                    source.push_str(tree_sitter_javascript::JSX_HIGHLIGHT_QUERY);
                }
                (language.into(), if jsx { &TSX } else { &TS }, source)
            }
        };
    let query = query_cache.get_or_init(|| {
        tree_sitter::Query::new(&language, &source).expect("bundled syntax highlight queries")
    });
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(text, None)?;
    let go = (extension == "go").then(|| GoNames::collect(tree.root_node(), text));
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), text.as_bytes());
    let mut events = Vec::new();
    let mut styles = Vec::new();
    while let Some(m) = matches.next() {
        if cancelled() {
            return None;
        }
        for capture in m.captures {
            let name = query.capture_names()[capture.index as usize];
            if name == "namespace.candidate"
                && !go
                    .as_ref()
                    .is_some_and(|go| go.is_package(capture.node, text))
            {
                continue;
            }
            let root = name.split('.').next().unwrap_or("");
            let (dark_color, light_color) = match root {
                "comment" => (0x7A7E85, 0x808080),
                "string" | "character" => (0x6AAB73, 0x008000),
                "keyword" | "boolean" | "conditional" | "repeat" | "exception" | "include" => {
                    (0xCC7832, 0x000080)
                }
                "number" | "float" => (0x6897BB, 0x0000FF),
                "function" => (0xFFC66D, 0x7A7A43),
                "type" | "constructor" => (0x56A8F5, 0x000080),
                "property" => (0xC77DBB, 0x660E7A),
                "tag" => (0xD5B778, 0x808000),
                "constant" => (0x9876AA, 0x660E7A),
                "attribute" | "annotation" | "decorator" => (0xBBB529, 0x808000),
                "namespace" | "module" => (0xA9B665, 0x5B7400),
                "escape" | "character.special" => (0xCC7832, 0x000080),
                "label" => (0xC77DBB, 0x660E7A),
                _ => (0xBCBEC4, 0x000000),
            };
            let (dark_color, light_color) = match name {
                "type.builtin" | "constant.builtin" | "variable.builtin" => (0xCC7832, 0x000080),
                "variable.parameter" => (0xB9A3CC, 0x660E7A),
                "function.macro" => (0xBBB529, 0x808000),
                "string.special" | "string.escape" => (0xCC7832, 0x000080),
                _ => (dark_color, light_color),
            };
            let (dark_color, light_color) = if go.is_some() {
                match name {
                    "namespace" | "namespace.candidate" => (0xA9B665, 0x5B7400),
                    "type.builtin" | "constant.builtin" => (0xCC7832, 0x000080),
                    "type.receiver" => (0x56A8B8, 0x2B6C80),
                    "type" | "type.declaration" | "property" => (0xA9B7C6, 0x000000),
                    "function.call" => (0xB9A979, 0x7A7A43),
                    "escape" => (0xCC7832, 0x000080),
                    _ => (dark_color, light_color),
                }
            } else {
                (dark_color, light_color)
            };
            let c = color(if dark { dark_color } else { light_color });
            let index = styles.len();
            styles.push(egui::TextFormat {
                font_id: egui::FontId::monospace(13.0),
                color: egui::Color32::from_rgb(c.r, c.g, c.b),
                italics: name.starts_with("comment") || name == "constant.declaration",
                ..Default::default()
            });
            // Narrow captures beat enclosing strings; semantic roles beat generic identifiers.
            let rank = if name == "variable.parameter" {
                2
            } else {
                match root {
                    "function" | "type" | "constructor" | "keyword" | "namespace" => 3,
                    "property" | "constant" | "tag" => 2,
                    "attribute" | "annotation" | "decorator" => 4,
                    "variable" | "punctuation" => 0,
                    _ => 1,
                }
            };
            let priority = (
                std::cmp::Reverse(capture.node.byte_range().len()),
                rank,
                m.pattern_index,
            );
            events.push((capture.node.start_byte(), false, index, priority));
            events.push((capture.node.end_byte(), true, index, priority));
        }
    }
    events.sort_unstable();
    let mut active = BTreeMap::new();
    let mut job = LayoutJob::default();
    let default = egui::TextFormat::simple(
        egui::FontId::monospace(13.0),
        if dark {
            crate::chrome::TEXT
        } else {
            egui::Color32::BLACK
        },
    );
    let mut offset = 0;
    for (position, end, index, priority) in events {
        if position > offset {
            let format = active
                .last_key_value()
                .map_or(&default, |(_, index): (_, &usize)| &styles[*index]);
            job.append(&text[offset..position], 0.0, format.clone());
            offset = position;
        }
        if !end {
            active.insert((priority, index), index);
        } else {
            active.remove(&(priority, index));
        }
    }
    job.append(&text[offset..], 0.0, default);
    Some(job)
}

#[derive(Default)]
struct GoNames {
    packages: std::collections::HashSet<String>,
    shadows: Vec<(String, std::ops::Range<usize>)>,
}

impl GoNames {
    fn collect(root: tree_sitter::Node<'_>, text: &str) -> Self {
        fn visit(
            node: tree_sitter::Node<'_>,
            text: &str,
            scope: std::ops::Range<usize>,
            names: &mut GoNames,
        ) {
            let kind = node.kind();
            let scope = if matches!(
                kind,
                "block"
                    | "function_declaration"
                    | "method_declaration"
                    | "func_literal"
                    | "for_statement"
            ) {
                node.byte_range()
            } else {
                scope
            };
            if kind == "import_spec" {
                let name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(text.as_bytes()).ok())
                    .map(str::to_owned)
                    .or_else(|| {
                        node.child_by_field_name("path")
                            .and_then(|n| n.utf8_text(text.as_bytes()).ok())
                            .map(|s| {
                                s.trim_matches(['"', '`'])
                                    .rsplit('/')
                                    .next()
                                    .unwrap_or(s)
                                    .to_owned()
                            })
                    });
                if let Some(name) = name {
                    names.packages.insert(name);
                }
            }
            if matches!(
                kind,
                "parameter_declaration"
                    | "variadic_parameter_declaration"
                    | "var_spec"
                    | "const_spec"
            ) {
                for name in node.children_by_field_name("name", &mut node.walk()) {
                    if let Ok(name) = name.utf8_text(text.as_bytes()) {
                        names.shadows.push((name.to_owned(), scope.clone()));
                    }
                }
            }
            if kind == "short_var_declaration"
                && let Some(left) = node.child_by_field_name("left")
            {
                for name in left.named_children(&mut left.walk()) {
                    if name.kind() == "identifier"
                        && let Ok(name) = name.utf8_text(text.as_bytes())
                    {
                        names
                            .shadows
                            .push((name.to_owned(), node.end_byte()..scope.end));
                    }
                }
            }
            for child in node.named_children(&mut node.walk()) {
                visit(child, text, scope.clone(), names);
            }
        }
        let mut names = Self::default();
        visit(root, text, root.byte_range(), &mut names);
        names
    }
    fn is_package(&self, node: tree_sitter::Node<'_>, text: &str) -> bool {
        let Ok(name) = node.utf8_text(text.as_bytes()) else {
            return false;
        };
        self.packages.contains(name)
            && !self
                .shadows
                .iter()
                .any(|(shadow, range)| shadow == name && range.contains(&node.start_byte()))
    }
}

fn tint_ranges(job: &mut LayoutJob, mut ranges: Vec<(std::ops::Range<usize>, u32)>) {
    ranges.sort_unstable_by_key(|(range, _)| range.start);
    let mut sections = Vec::with_capacity(job.sections.len() + ranges.len() * 2);
    let mut first = 0;
    for section in &job.sections {
        while first < ranges.len() && ranges[first].0.end <= section.byte_range.start {
            first += 1;
        }
        let mut start = section.byte_range.start;
        for (range, hex) in ranges[first..]
            .iter()
            .take_while(|(range, _)| range.start < section.byte_range.end)
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
            let c = color(*hex);
            part.format.color = egui::Color32::from_rgb(c.r, c.g, c.b);
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

/// JSON's opening quote enters the key scope one token late in the bundled syntax.
/// Color both quote tokens from the following colon, keeping escaped quotes inside strings.
fn normalize_json_quotes(job: &mut LayoutJob, dark: bool) {
    let text = &job.text;
    let bytes = text.as_bytes();
    let mut ranges = vec![];
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'"' {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        let mut escapes = vec![];
        while index < bytes.len() && bytes[index] != b'"' {
            if bytes[index] == b'\\' {
                let escape = index;
                index += 1;
                if index < bytes.len() {
                    if bytes[index] == b'u'
                        && bytes
                            .get(index + 1..index + 5)
                            .is_some_and(|b| b.iter().all(u8::is_ascii_hexdigit))
                    {
                        index += 5;
                    } else {
                        index += text[index..].chars().next().unwrap().len_utf8();
                    }
                }
                escapes.push(escape..index);
            } else {
                index += text[index..].chars().next().unwrap().len_utf8();
            }
        }
        let closed = index < bytes.len();
        let end = index + usize::from(closed);
        let key = closed && text[end..].trim_start().starts_with(':');
        let hex = match (dark, key) {
            (true, true) => 0xC77DBB,
            (false, true) => 0x660E7A,
            (true, false) => 0x6AAB73,
            (false, false) => 0x008000,
        };
        ranges.push((start..(start + 1), hex));
        if closed {
            ranges.push((index..end, hex));
        }
        // Explicit escape colors survive the quote correction.
        for escape in escapes {
            ranges.push((escape, if dark { 0xCC7832 } else { 0x000080 }));
        }
        index = end;
    }
    tint_ranges(job, ranges);
}
