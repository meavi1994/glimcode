use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tree_sitter::{Language, Node, Parser};

pub const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_RESULTS: usize = 2_000;

#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug)]
pub struct Location {
    pub path: PathBuf,
    pub line: usize,
    pub column: usize, // One-based UTF-8 byte column, as returned by ripgrep.
    pub preview: String,
    pub category: MatchCategory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchCategory {
    Declaration,
    Usage,
    String,
    Comment,
    Import,
    Other,
}

impl MatchCategory {
    pub const ALL: [Self; 6] = [
        Self::Declaration,
        Self::Usage,
        Self::String,
        Self::Comment,
        Self::Import,
        Self::Other,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Declaration => "Declarations",
            Self::Usage => "Usages",
            Self::String => "Strings",
            Self::Comment => "Comments",
            Self::Import => "Imports",
            Self::Other => "Other / unclassified",
        }
    }
}

struct Classifier {
    tree: tree_sitter::Tree,
}

impl Classifier {
    fn new(path: &Path, source: &str) -> Option<Self> {
        let mut parser = Parser::new();
        parser.set_language(&language(path)?).ok()?;
        Some(Self {
            tree: parser.parse(source, None)?,
        })
    }

    fn category(&self, start: usize, end: usize) -> MatchCategory {
        let mut node = self.tree.root_node().descendant_for_byte_range(start, end);
        let mut category = MatchCategory::Usage;
        let mut interpolation = false;
        while let Some(current) = node {
            let kind = current.kind();
            if current.is_error() || current.is_missing() {
                return MatchCategory::Other;
            }
            if kind.contains("comment") {
                return MatchCategory::Comment;
            }
            if matches!(kind, "interpolation" | "template_substitution") {
                interpolation = true;
            }
            if !interpolation
                && (kind.contains("string")
                    || matches!(kind, "char_literal" | "template_string" | "regex"))
            {
                return MatchCategory::String;
            }
            if matches!(
                kind,
                "use_declaration"
                    | "extern_crate_declaration"
                    | "import_statement"
                    | "import_from_statement"
                    | "import_declaration"
                    | "import"
            ) {
                return MatchCategory::Import;
            }
            let field = match kind {
                "let_declaration" => Some("pattern"),
                "assignment" | "named_expression" => Some("left"),
                "parameter" | "required_parameter" | "optional_parameter" => Some("pattern"),
                "default_parameter" | "typed_parameter" | "typed_default_parameter" => Some("name"),
                "function_item"
                | "struct_item"
                | "enum_item"
                | "trait_item"
                | "type_item"
                | "const_item"
                | "static_item"
                | "mod_item"
                | "function_definition"
                | "class_definition"
                | "function_declaration"
                | "method_declaration"
                | "constructor_declaration"
                | "compact_constructor_declaration"
                | "record_declaration"
                | "annotation_type_declaration"
                | "enum_constant"
                | "object_declaration"
                | "variable_declaration"
                | "class_parameter"
                | "formal_parameter"
                | "catch_formal_parameter"
                | "type_spec"
                | "type_alias"
                | "var_spec"
                | "const_spec"
                | "parameter_declaration"
                | "variadic_parameter_declaration"
                | "type_parameter_declaration"
                | "class_declaration"
                | "method_definition"
                | "interface_declaration"
                | "type_alias_declaration"
                | "enum_declaration"
                | "variable_declarator"
                | "field_declaration"
                | "enum_variant"
                | "type_parameter" => Some("name"),
                _ => None,
            };
            if let Some(field) = field {
                let mut cursor = current.walk();
                let binding = current
                    .children_by_field_name(field, &mut cursor)
                    .any(|binding| start >= binding.start_byte() && end <= binding.end_byte());
                let fallback = current.child_by_field_name("name").is_some_and(|binding| {
                    start >= binding.start_byte() && end <= binding.end_byte()
                });
                let kotlin_binding = matches!(
                    kind,
                    "variable_declaration" | "parameter" | "class_parameter" | "type_parameter"
                ) && current
                    .named_children(&mut current.walk())
                    .find(|n| n.kind() == "identifier")
                    .is_some_and(|n| start >= n.start_byte() && end <= n.end_byte());
                let alias = kind == "type_alias"
                    && current.child_by_field_name("type").is_some_and(|n| {
                        n.kind() == "identifier" && start >= n.start_byte() && end <= n.end_byte()
                    });
                if binding || fallback || kotlin_binding || alias {
                    category = MatchCategory::Declaration;
                }
            }
            // Python parameters are bare identifiers, rather than named parameter nodes.
            if kind == "parameters"
                && current
                    .parent()
                    .is_some_and(|p| p.kind() == "function_definition")
                && current.named_children(&mut current.walk()).any(|child| {
                    child.kind() == "identifier"
                        && start >= child.start_byte()
                        && end <= child.end_byte()
                })
                && category == MatchCategory::Usage
            {
                category = MatchCategory::Declaration;
            }
            node = current.parent();
        }
        category
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    Text,
    References,
    Definitions,
}

pub fn language(path: &Path) -> Option<Language> {
    Some(match path.extension()?.to_str()?.to_lowercase().as_str() {
        "java" => tree_sitter_java::LANGUAGE.into(),
        "kt" | "kts" => tree_sitter_kotlin_ng::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "rs" => tree_sitter_rust::LANGUAGE.into(),
        "py" => tree_sitter_python::LANGUAGE.into(),
        "js" | "jsx" | "mjs" | "cjs" => tree_sitter_javascript::LANGUAGE.into(),
        "ts" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        _ => return None,
    })
}

pub fn symbols(path: &Path, source: &str) -> Vec<Symbol> {
    let Some(language) = language(path) else {
        return vec![];
    };
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return vec![];
    }
    let Some(tree) = parser.parse(source, None) else {
        return vec![];
    };
    let mut result = vec![];
    collect(tree.root_node(), source.as_bytes(), &mut result);
    result
}

fn collect(node: Node<'_>, source: &[u8], result: &mut Vec<Symbol>) {
    let kind = node.kind();
    if matches!(
        kind,
        "function_item"
            | "struct_item"
            | "enum_item"
            | "trait_item"
            | "type_item"
            | "const_item"
            | "static_item"
            | "mod_item"
            | "function_definition"
            | "class_definition"
            | "function_declaration"
            | "method_declaration"
            | "constructor_declaration"
            | "compact_constructor_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
            | "enum_constant"
            | "object_declaration"
            | "variable_declaration"
            | "type_spec"
            | "type_alias"
            | "var_spec"
            | "const_spec"
            | "class_declaration"
            | "method_definition"
            | "interface_declaration"
            | "type_alias_declaration"
            | "enum_declaration"
            | "variable_declarator"
    ) {
        let mut cursor = node.walk();
        let mut names: Vec<_> = node.children_by_field_name("name", &mut cursor).collect();
        if names.is_empty() {
            if kind == "type_alias" {
                names.extend(
                    node.child_by_field_name("type")
                        .filter(|n| n.kind() == "identifier"),
                );
            } else if kind == "variable_declaration" {
                names.extend(
                    node.named_children(&mut node.walk())
                        .find(|n| n.kind() == "identifier"),
                );
            }
        }
        for name in names {
            if name.named_child_count() == 0
                && let Ok(text) = name.utf8_text(source)
            {
                result.push(Symbol {
                    name: text.to_owned(),
                    kind: kind.to_owned(),
                    line: name.start_position().row + 1,
                    column: name.start_position().column + 1,
                });
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect(child, source, result);
    }
}

pub fn read_text(path: &Path) -> Result<String> {
    let file =
        std::fs::File::open(path).with_context(|| format!("Cannot open {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        bail!("File exceeds the 20 MiB editor limit");
    }
    if bytes.contains(&0) {
        bail!("Binary files cannot be edited");
    }
    String::from_utf8(bytes).context("This file is not valid UTF-8")
}

pub fn files(root: &Path) -> Result<Vec<PathBuf>> {
    let output = Command::new("rg")
        .args([
            "--files", "--null", "--hidden", "-g", "!.git", "-g", "!target",
        ])
        .current_dir(root)
        .output()
        .context("Install ripgrep (rg) and ensure it is on PATH")?;
    if !output.status.success() && output.status.code() != Some(1) {
        bail!("ripgrep: {}", String::from_utf8_lossy(&output.stderr));
    }
    let mut files: Vec<_> = output
        .stdout
        .split(|&b| b == 0)
        .filter(|b| !b.is_empty())
        .map(|b| root.join(path_from_bytes(b)))
        .collect();
    files.sort();
    Ok(files)
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}
#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).as_ref())
}

#[cfg(any(test, feature = "ui-capture"))]
pub struct SearchResults {
    pub locations: Vec<Location>,
    pub truncated: bool,
}

pub enum SearchUpdate {
    OpenFiles(Vec<Location>),
    Project(Vec<Location>),
    Finished { truncated: bool },
    Failed(String),
}

/// Search an editor snapshot, including unsaved changes, without waiting for disk search.
pub fn search_buffer(
    path: &Path,
    source: &str,
    query: &str,
    kind: SearchKind,
    cancelled: &AtomicBool,
) -> Vec<Location> {
    if query.is_empty() || cancelled.load(Ordering::Relaxed) {
        return vec![];
    }
    if kind == SearchKind::Definitions {
        return symbols(path, source)
            .into_iter()
            .filter(|s| s.name == query)
            .take(MAX_RESULTS)
            .map(|s| Location {
                path: path.to_path_buf(),
                line: s.line,
                column: s.column,
                preview: source
                    .lines()
                    .nth(s.line - 1)
                    .unwrap_or("")
                    .trim_end()
                    .to_owned(),
                category: MatchCategory::Declaration,
            })
            .collect();
    }
    static WORD: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let word = WORD.get_or_init(|| regex::Regex::new(r"\w").unwrap());
    let word_character = |character: char| word.is_match(character.encode_utf8(&mut [0; 4]));
    let classifier = Classifier::new(path, source);
    let mut locations = vec![];
    let mut offset = 0;
    for (line, text) in source.split_inclusive('\n').enumerate() {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        for (column, _) in text.match_indices(query) {
            let end = column + query.len();
            if kind != SearchKind::Text
                && (text[..column]
                    .chars()
                    .next_back()
                    .is_some_and(word_character)
                    || text[end..].chars().next().is_some_and(word_character))
            {
                continue;
            }
            locations.push(Location {
                path: path.to_path_buf(),
                line: line + 1,
                column: column + 1,
                preview: text.trim_end().to_owned(),
                category: classifier.as_ref().map_or(MatchCategory::Other, |c| {
                    c.category(offset + column, offset + end)
                }),
            });
            if locations.len() == MAX_RESULTS {
                return locations;
            }
        }
        offset += text.len();
    }
    locations
}

pub fn search_workspace(
    root: &Path,
    query: &str,
    kind: SearchKind,
    buffers: Vec<(PathBuf, String)>,
    priority_paths: Vec<PathBuf>,
    cancelled: Arc<AtomicBool>,
    emit: impl Fn(SearchUpdate),
) {
    if cancelled.load(Ordering::Relaxed) {
        return;
    }
    let mut remaining = MAX_RESULTS;
    let mut excluded = std::collections::HashSet::new();
    for (path, snapshot) in buffers
        .into_iter()
        .map(|(path, source)| (path, Some(source)))
        .chain(priority_paths.into_iter().map(|path| (path, None)))
    {
        if cancelled.load(Ordering::Relaxed) {
            return;
        }
        if !excluded.insert(path.clone()) {
            continue;
        }
        let source = match snapshot {
            Some(source) => source,
            None => match read_text(&path) {
                Ok(source) => source,
                Err(_) => continue,
            },
        };
        let mut locations = search_buffer(&path, &source, query, kind, &cancelled);
        locations.truncate(remaining);
        remaining -= locations.len();
        if !locations.is_empty() {
            emit(SearchUpdate::OpenFiles(locations));
        }
        if remaining == 0 {
            emit(SearchUpdate::Finished { truncated: true });
            return;
        }
    }
    // Project search starts only after editor snapshots and Git-changed files are delivered.
    match search_stream(
        root,
        query,
        kind,
        &excluded,
        remaining,
        cancelled.clone(),
        |batch| emit(SearchUpdate::Project(batch)),
    ) {
        Ok(truncated) if !cancelled.load(Ordering::Relaxed) => {
            emit(SearchUpdate::Finished { truncated })
        }
        Err(error) if !cancelled.load(Ordering::Relaxed) => {
            emit(SearchUpdate::Failed(format!("{error:#}")))
        }
        _ => {}
    }
}

#[cfg(any(test, feature = "ui-capture"))]
pub fn search(root: &Path, query: &str, kind: SearchKind) -> Result<SearchResults> {
    let mut locations = vec![];
    let truncated = search_stream(
        root,
        query,
        kind,
        &Default::default(),
        MAX_RESULTS,
        Arc::new(AtomicBool::new(false)),
        |batch| locations.extend(batch),
    )?;
    Ok(SearchResults {
        locations,
        truncated,
    })
}

fn search_stream(
    root: &Path,
    query: &str,
    kind: SearchKind,
    excluded: &std::collections::HashSet<PathBuf>,
    limit: usize,
    cancelled: Arc<AtomicBool>,
    mut emit: impl FnMut(Vec<Location>),
) -> Result<bool> {
    if query.is_empty() {
        return Ok(false);
    }
    let mut command = Command::new("rg");
    command.args([
        "--json",
        "--fixed-strings",
        "--hidden",
        "-g",
        "!.git",
        "-g",
        "!target",
        "--max-filesize",
        "20M",
    ]);
    if kind != SearchKind::Text {
        command.arg("--word-regexp");
    }
    let mut child = command
        .arg("--")
        .arg(query)
        .arg(".")
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Cannot start ripgrep. Install rg and ensure it is on PATH")?;
    let stderr = child.stderr.take().unwrap();
    // Drain stderr concurrently so an unread error pipe cannot stall the search.
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = BufReader::new(stderr).read_to_string(&mut text);
        text
    });
    let mut locations = vec![];
    let mut delivered = 0;
    let mut last_batch = Instant::now();
    let mut candidates = std::collections::HashSet::new();
    let mut classifiers = std::collections::HashMap::new();
    let mut truncated = false;
    let stdout = child.stdout.take().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = stop.clone();
    let cancel_worker = cancelled.clone();
    let process = std::thread::spawn(move || -> std::io::Result<std::process::ExitStatus> {
        loop {
            if stop_worker.load(Ordering::Relaxed) || cancel_worker.load(Ordering::Relaxed) {
                let _ = child.kill();
                return child.wait();
            }
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    for line in BufReader::new(stdout).lines() {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        let Ok(line) = line else {
            break;
        };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value["type"] != "match" {
            continue;
        }
        let data = &value["data"];
        let Some(path) = data["path"]["text"].as_str() else {
            continue;
        };
        let path: PathBuf = root.join(path).components().collect();
        if excluded.contains(&path)
            || (!excluded.is_empty()
                && path
                    .canonicalize()
                    .is_ok_and(|path| excluded.contains(&path)))
        {
            continue;
        }
        if kind == SearchKind::Definitions {
            if candidates.insert(path.clone())
                && let Ok(source) = read_text(&path)
            {
                for symbol in symbols(&path, &source)
                    .into_iter()
                    .filter(|s| s.name == query)
                {
                    locations.push(Location {
                        category: MatchCategory::Declaration,
                        path: path.clone(),
                        line: symbol.line,
                        column: symbol.column,
                        preview: source
                            .lines()
                            .nth(symbol.line - 1)
                            .unwrap_or("")
                            .trim()
                            .to_owned(),
                    });
                }
            }
        } else if let Some(matches) = data["submatches"].as_array() {
            // ripgrep emits each file's matches together. Retain just its syntax tree.
            if !classifiers.contains_key(&path) {
                classifiers.clear();
            }
            let classifier = classifiers.entry(path.clone()).or_insert_with(|| {
                read_text(&path)
                    .ok()
                    .and_then(|source| Classifier::new(&path, &source))
            });
            for entry in matches {
                let start = data["absolute_offset"].as_u64().unwrap_or(0) as usize
                    + entry["start"].as_u64().unwrap_or(0) as usize;
                let end = data["absolute_offset"].as_u64().unwrap_or(0) as usize
                    + entry["end"].as_u64().unwrap_or(0) as usize;
                locations.push(Location {
                    category: classifier
                        .as_ref()
                        .map_or(MatchCategory::Other, |c| c.category(start, end)),
                    path: path.clone(),
                    line: data["line_number"].as_u64().unwrap_or(1) as usize,
                    column: entry["start"].as_u64().unwrap_or(0) as usize + 1,
                    preview: data["lines"]["text"]
                        .as_str()
                        .unwrap_or("")
                        .trim_end()
                        .to_owned(),
                });
            }
        }
        if delivered + locations.len() >= limit {
            locations.truncate(limit.saturating_sub(delivered));
            truncated = true;
            break;
        }
        if !locations.is_empty()
            && (delivered == 0
                || locations.len() >= 64
                || last_batch.elapsed() >= Duration::from_millis(40))
        {
            delivered += locations.len();
            emit(std::mem::take(&mut locations));
            last_batch = Instant::now();
        }
    }
    stop.store(
        truncated || cancelled.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
    let status = process
        .join()
        .map_err(|_| anyhow::anyhow!("Search process monitor stopped"))??;
    let errors = errors.join().unwrap_or_default();
    if !cancelled.load(Ordering::Relaxed)
        && !truncated
        && !status.success()
        && status.code() != Some(1)
    {
        bail!("ripgrep: {errors}");
    }
    if !locations.is_empty() && !cancelled.load(Ordering::Relaxed) {
        emit(locations);
    }
    Ok(truncated)
}

/// Convert a one-based line / UTF-8 byte column into egui's Unicode character offset.
pub fn char_offset(text: &str, line: usize, column: usize) -> usize {
    let mut offset = 0;
    for (index, chunk) in text.split_inclusive('\n').enumerate() {
        if index + 1 == line {
            return offset
                + chunk
                    .char_indices()
                    .take_while(|(byte, _)| *byte < column.saturating_sub(1))
                    .count();
        }
        offset += chunk.chars().count();
    }
    offset
}

pub fn word_at(text: &str, position: usize) -> String {
    let chars: Vec<_> = text.chars().collect();
    let mut start = position.min(chars.len());
    let mut end = start;
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    while start > 0 && ident(chars[start - 1]) {
        start -= 1;
    }
    while end < chars.len() && ident(chars[end]) {
        end += 1;
    }
    chars[start..end].iter().collect()
}

/// Unlike keyboard navigation, a click on whitespace must not select the preceding token.
pub fn token_at(text: &str, position: usize) -> Option<String> {
    let character = text.chars().nth(position)?;
    if !character.is_alphanumeric() && character != '_' {
        return None;
    }
    Some(word_at(text, position))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn open_snapshots_arrive_first_and_replace_their_disk_results() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let open = dir.path().join("open.rs");
        std::fs::write(&open, "// needle on disk\n")?;
        std::fs::write(dir.path().join("project.rs"), "fn f() { needle(); }\n")?;
        let mut updates = vec![];
        // RefCell permits recording events through the worker's immutable callback.
        let recorded = std::cell::RefCell::new(&mut updates);
        search_workspace(
            dir.path(),
            "needle",
            SearchKind::References,
            vec![(open.clone(), "fn needle() {} // unsaved\n".into())],
            vec![],
            Arc::new(AtomicBool::new(false)),
            |update| recorded.borrow_mut().push(update),
        );
        assert!(
            matches!(updates.first(), Some(SearchUpdate::OpenFiles(locations)) if locations.len() == 1 && locations[0].category == MatchCategory::Declaration)
        );
        let project: Vec<_> = updates
            .iter()
            .filter_map(|update| {
                if let SearchUpdate::Project(locations) = update {
                    Some(locations)
                } else {
                    None
                }
            })
            .flatten()
            .collect();
        assert_eq!(project.len(), 1);
        assert!(project.iter().all(|location| location.path != open));
        assert!(matches!(
            updates.last(),
            Some(SearchUpdate::Finished { truncated: false })
        ));
        Ok(())
    }

    #[test]
    fn changed_files_are_searched_after_buffers_before_project_and_only_once() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let open = root.join("open.rs");
        let changed = root.join("changed.rs");
        let ignored = root.join("ignored.rs");
        let rest = root.join("project.rs");
        for path in [&open, &changed, &ignored, &rest] {
            std::fs::write(path, "// needle disk\n")?;
        }
        std::fs::write(root.join(".ignore"), "ignored.rs\n")?;
        std::fs::write(root.join("binary.rs"), b"needle\0")?;
        let updates = std::cell::RefCell::new(vec![]);
        search_workspace(
            &root,
            "needle",
            SearchKind::Text,
            vec![(open.clone(), "// needle unsaved\n".into())],
            vec![
                open.clone(),
                changed.clone(),
                ignored.clone(),
                changed.clone(),
                root.join("deleted.rs"),
                root.join("binary.rs"),
            ],
            Arc::new(AtomicBool::new(false)),
            |update| updates.borrow_mut().push(update),
        );
        let updates = updates.into_inner();
        let mut priority = vec![];
        let mut project = vec![];
        let mut started_project = false;
        for update in updates {
            match update {
                SearchUpdate::OpenFiles(rows) => {
                    assert!(!started_project);
                    priority.extend(rows);
                }
                SearchUpdate::Project(rows) => {
                    started_project = true;
                    project.extend(rows);
                }
                SearchUpdate::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert_eq!(
            priority.iter().map(|r| &r.path).collect::<Vec<_>>(),
            vec![&open, &changed, &ignored]
        );
        assert!(priority[0].preview.contains("unsaved"));
        assert_eq!(project.len(), 1);
        assert_eq!(project[0].path, rest);
        Ok(())
    }

    #[test]
    fn cancelled_search_publishes_nothing() {
        search_workspace(
            Path::new("/does-not-exist"),
            "needle",
            SearchKind::Text,
            vec![],
            vec![],
            Arc::new(AtomicBool::new(true)),
            |_| panic!("cancelled search published an update"),
        );
    }

    #[test]
    fn streaming_result_limit_is_respected() -> Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("many.txt"),
            "needle\n".repeat(MAX_RESULTS + 1),
        )?;
        let results = search(dir.path(), "needle", SearchKind::Text)?;
        assert_eq!(results.locations.len(), MAX_RESULTS);
        assert!(results.truncated);
        Ok(())
    }
    #[test]
    fn whole_words_exclude_identifier_substrings_but_allow_punctuation() -> Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("sample.txt"),
            "generate generated generate_keys build-generate generate()",
        )?;
        assert_eq!(
            search(dir.path(), "generate", SearchKind::Text)?
                .locations
                .len(),
            5
        );
        assert_eq!(
            search(dir.path(), "generate", SearchKind::References)?
                .locations
                .len(),
            3
        );
        Ok(())
    }
    #[test]
    fn search_categories_distinguish_code_strings_comments_and_imports() -> Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("a.rs"),
            "use lib::needle;\nfn needle() {}\nfn main() { needle(); let x = \"needle\"; }\n// needle\n",
        )?;
        std::fs::write(dir.path().join("notes.md"), "needle")?;
        let results = search(dir.path(), "needle", SearchKind::References)?;
        for category in MatchCategory::ALL {
            assert_eq!(
                results
                    .locations
                    .iter()
                    .filter(|r| r.category == category)
                    .count(),
                1,
                "{category:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn java_and_kotlin_support_classification_outlines_and_definitions() -> Result<()> {
        for (extension, source) in [
            (
                "java",
                "import p.render;\nclass Demo { void render() {} void caller() { render(); String note = \"render\"; } }\n// render\n",
            ),
            (
                "kt",
                "import p.render\nclass Demo {\n fun render() {}\n fun caller() { render(); val note = \"render\" }\n}\n// render\n",
            ),
            (
                "kts",
                "fun render() {}\nrender()\nval note = \"render\"\n// render\n",
            ),
        ] {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join(format!("Demo.{extension}"));
            std::fs::write(&path, source)?;
            let mut parser = Parser::new();
            parser.set_language(&language(&path).unwrap())?;
            let tree = parser.parse(source, None).unwrap();
            assert!(
                !tree.root_node().has_error(),
                "{extension}: {}",
                tree.root_node().to_sexp()
            );
            let expected = if extension == "kts" {
                vec![
                    MatchCategory::Declaration,
                    MatchCategory::Usage,
                    MatchCategory::String,
                    MatchCategory::Comment,
                ]
            } else {
                vec![
                    MatchCategory::Import,
                    MatchCategory::Declaration,
                    MatchCategory::Usage,
                    MatchCategory::String,
                    MatchCategory::Comment,
                ]
            };
            let results = search_buffer(
                &path,
                source,
                "render",
                SearchKind::References,
                &AtomicBool::new(false),
            );
            assert_eq!(
                results.iter().map(|r| r.category).collect::<Vec<_>>(),
                expected,
                "{extension}"
            );
            let defs = search(dir.path(), "render", SearchKind::Definitions)?;
            assert_eq!(defs.locations.len(), 1, "{extension}");
            let outline = symbols(&path, source);
            assert!(outline.iter().any(|s| s.name == "render"), "{extension}");
            assert!(outline.iter().any(|s| s.name == "note"), "{extension}");
            if extension != "kts" {
                assert!(outline.iter().any(|s| s.name == "Demo"), "{extension}");
            }
        }
        Ok(())
    }

    #[test]
    fn go_function_search_categorizes_declaration_calls_and_comment() -> Result<()> {
        let source = "package apps\n\n// updateDaemonSetWithRetries updates daemonsets.\nfunc updateDaemonSetWithRetries(ctx context.Context, c clientset.Interface, namespace, name string, applyUpdate updateDSFunc) (ds *appsv1.DaemonSet, err error) { return }\nfunc caller() {\n    updateDaemonSetWithRetries(ctx, applyUpdate)\n    updateDaemonSetWithRetries(ctx, applyUpdate)\n}\n";
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("daemon_set.go");
        std::fs::write(&path, source)?;
        let query = "updateDaemonSetWithRetries";
        let expected = [
            MatchCategory::Comment,
            MatchCategory::Declaration,
            MatchCategory::Usage,
            MatchCategory::Usage,
        ];
        // Both open editor snapshots and disk searches use the Go classifier.
        let open = search_buffer(
            &path,
            source,
            query,
            SearchKind::References,
            &AtomicBool::new(false),
        );
        let disk = search(dir.path(), query, SearchKind::References)?;
        for results in [&open, &disk.locations] {
            assert_eq!(
                results
                    .iter()
                    .map(|location| location.category)
                    .collect::<Vec<_>>(),
                expected
            );
        }
        let open_defs = search_buffer(
            &path,
            source,
            query,
            SearchKind::Definitions,
            &AtomicBool::new(false),
        );
        let disk_defs = search(dir.path(), query, SearchKind::Definitions)?;
        for results in [&open_defs, &disk_defs.locations] {
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].line, 4);
            assert_eq!(results[0].column, 6);
            assert_eq!(results[0].category, MatchCategory::Declaration);
        }
        Ok(())
    }

    #[test]
    fn go_outline_and_classifier_recognize_methods_types_and_bindings() {
        let source = "package apps\ntype Worker struct {}\ntype Alias = Worker\nconst Limit = 2\nvar first, second Worker\nfunc (worker *Worker) Update(first, second Worker) { worker.Update(first, second) }\n";
        let path = Path::new("apps.go");
        let outline = symbols(path, source);
        for name in ["Worker", "Alias", "Limit", "first", "second", "Update"] {
            assert!(
                outline.iter().any(|symbol| symbol.name == name),
                "missing {name}"
            );
        }
        let classifier = Classifier::new(path, source).unwrap();
        for query in ["Update", "first", "second"] {
            let categories: Vec<_> = source
                .match_indices(query)
                .map(|(start, _)| classifier.category(start, start + query.len()))
                .collect();
            let expected = if query == "Update" {
                vec![MatchCategory::Declaration, MatchCategory::Usage]
            } else {
                vec![
                    MatchCategory::Declaration,
                    MatchCategory::Declaration,
                    MatchCategory::Usage,
                ]
            };
            assert_eq!(categories, expected, "{query}");
        }
    }

    #[test]
    fn classifies_bindings_and_interpolated_expressions() {
        for (path, source, expected) in [
            (
                "a.rs",
                "fn f(needle: i32) { let needle = needle; }",
                vec![
                    MatchCategory::Declaration,
                    MatchCategory::Declaration,
                    MatchCategory::Usage,
                ],
            ),
            (
                "a.py",
                "def f(needle=0, other=needle):\n    return f'hi {needle}'\n",
                vec![
                    MatchCategory::Declaration,
                    MatchCategory::Usage,
                    MatchCategory::Usage,
                ],
            ),
            (
                "a.ts",
                "const needle = `needle ${needle}`; // needle\n",
                vec![
                    MatchCategory::Declaration,
                    MatchCategory::String,
                    MatchCategory::Usage,
                    MatchCategory::Comment,
                ],
            ),
        ] {
            let classifier = Classifier::new(Path::new(path), source).unwrap();
            let actual: Vec<_> = source
                .match_indices("needle")
                .map(|(offset, _)| classifier.category(offset, offset + 6))
                .collect();
            assert_eq!(actual, expected, "{path}");
        }
    }

    #[test]
    fn clicked_token_does_not_use_the_previous_word_on_whitespace() {
        assert_eq!(token_at("猫 café rest", 3).as_deref(), Some("café"));
        assert_eq!(token_at("foo bar", 3), None);
        assert_eq!(token_at("foo()", 3), None);
        assert_eq!(token_at("foo", 3), None);
    }
    #[test]
    fn outline_recognizes_nested_definitions() {
        let result = symbols(
            Path::new("a.rs"),
            "struct Cat {}\nimpl Cat { fn purr(&self) {} }\n",
        );
        assert_eq!(
            result.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["Cat", "purr"]
        );
        assert_eq!(result[1].line, 2);
        assert!(symbols(Path::new("a.txt"), "fn fake() {}").is_empty());
    }
    #[test]
    fn unicode_navigation_uses_character_offsets() {
        assert_eq!(char_offset("éx\n猫 hi", 2, 5), 5);
        assert_eq!(word_at("fn café()", 6), "café");
    }
    #[test]
    fn ripgrep_respects_literal_queries_and_filters_definitions() -> Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("a.rs"),
            "fn needle() {}\nfn caller() { needle(); }\n// a.b\n",
        )?;
        let refs = search(dir.path(), "needle", SearchKind::References)?;
        assert_eq!(refs.locations.len(), 2);
        let defs = search(dir.path(), "needle", SearchKind::Definitions)?;
        assert_eq!(defs.locations.len(), 1);
        assert_eq!(defs.locations[0].line, 1);
        assert_eq!(
            search(dir.path(), "a.b", SearchKind::Text)?.locations.len(),
            1
        );
        assert!(
            search(dir.path(), "absent", SearchKind::Text)?
                .locations
                .is_empty()
        );
        Ok(())
    }
}
