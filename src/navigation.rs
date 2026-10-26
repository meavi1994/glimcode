use anyhow::{Context, Result, bail};
use grep::{
    matcher::Matcher,
    regex::{RegexMatcher, RegexMatcherBuilder},
    searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch},
};
use ignore::{WalkBuilder, WalkState, overrides::OverrideBuilder};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tree_sitter::{Language, Node, Parser};

pub const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_RESULTS: usize = 2_000;

/// The start of every line of a text.
///
/// Looking a line up by walking the text from the start is linear, and the
/// line is asked for once per match and once per candidate. The index turns
/// that into a lookup.
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(source: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(source.match_indices('\n').map(|(index, _)| index + 1));
        Self { starts }
    }

    /// The text of a one-based line.
    pub fn line<'a>(&self, source: &'a str, line: usize) -> &'a str {
        let Some(start) = self.starts.get(line.saturating_sub(1)).copied() else {
            return "";
        };
        let end = self
            .starts
            .get(line)
            .copied()
            .unwrap_or(source.len())
            .min(source.len());
        source[start..end].trim_end_matches(['\n', '\r'])
    }

    /// The number of lines.
    pub fn line_count(&self) -> usize {
        self.starts.len()
    }
}

#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub line: usize,
    pub column: usize,
    /// The type, class or module that holds the symbol, when it has one.
    pub owner: Option<String>,
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

#[cfg(test)]
static CLASSIFY_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The syntactic construct surrounding a position, used by click-to-navigate
/// ranking to separate locals and parameters from file-level declarations.
#[derive(Clone, Debug)]
pub struct Scope {
    pub start: usize,
    pub end: usize,
}

pub struct Analysis {
    pub category: MatchCategory,
    pub scope: Option<Scope>,
    /// The type, class or module around the position.
    pub owner: Option<String>,
}

/// Classify a byte range and find its innermost function, method, type or
/// module. This parses one file on demand; there is no index to keep in sync.
pub fn analyze_at(path: &Path, source: &str, start: usize, end: usize) -> Analysis {
    let Some(classifier) = Classifier::new(path, source) else {
        return Analysis {
            category: MatchCategory::Other,
            scope: None,
            owner: None,
        };
    };
    Analysis {
        category: classifier.category(start, end),
        scope: classifier.scope(start, end),
        owner: classifier.owner(source, start, end),
    }
}

/// Node kinds that hold the symbols inside them.
fn is_owner_kind(kind: &str) -> bool {
    matches!(
        kind,
        "struct_item"
            | "enum_item"
            | "trait_item"
            | "impl_item"
            | "mod_item"
            | "type_item"
            | "class_declaration"
            | "class_definition"
            | "abstract_class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "object_declaration"
            | "record_declaration"
            | "type_declaration"
    )
}

/// The receiver type of a Go method, which is written on the declaration
/// instead of around it.
fn go_receiver(node: Node<'_>, source: &[u8]) -> Option<String> {
    let receiver = node.child_by_field_name("receiver")?;
    let mut cursor = receiver.walk();
    let parameter = receiver
        .named_children(&mut cursor)
        .find(|child| child.child_by_field_name("type").is_some())?;
    let kind = parameter.child_by_field_name("type")?;
    let text = kind.utf8_text(source).ok()?;
    let text = text.trim().trim_start_matches('*');
    (!text.is_empty()).then(|| text.to_owned())
}

/// The name of the type, class or module that holds `node`.
fn owner_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() == "method_declaration"
        && let Some(receiver) = go_receiver(node, source)
    {
        return Some(receiver);
    }
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "method_declaration"
            && let Some(receiver) = go_receiver(parent, source)
        {
            return Some(receiver);
        }
        let kind = parent.kind();
        if kind == "type_declaration" {
            // Go keeps the name in a `type_spec` child.
            let mut cursor = parent.walk();
            let spec = parent
                .named_children(&mut cursor)
                .find(|child| child.kind() == "type_spec")?;
            let name = spec.child_by_field_name("name")?;
            return name.utf8_text(source).ok().map(|text| text.to_owned());
        }
        if is_owner_kind(kind) {
            let mut cursor = parent.walk();
            let name = parent
                .child_by_field_name("name")
                .or_else(|| parent.child_by_field_name("type"))
                .or_else(|| {
                    parent
                        .named_children(&mut cursor)
                        .find(|child| matches!(child.kind(), "identifier" | "type_identifier"))
                })?;
            let text = name.utf8_text(source).ok()?;
            // Strip generic arguments so `Store<T>` is stored as `Store`.
            let text = text.split('<').next().unwrap_or(text);
            let text = text.trim().trim_start_matches('\'');
            return (!text.is_empty()).then(|| text.to_owned());
        }
        current = parent.parent();
    }
    None
}

/// True only for the binding name within a pattern, excluding default values,
/// destructuring property keys, type names, and writes through existing objects.
fn binding_contains(mut binding: Node<'_>, start: usize, end: usize) -> bool {
    let contains = |node: Node<'_>| start >= node.start_byte() && end <= node.end_byte();
    if start >= end || !contains(binding) {
        return false;
    }
    loop {
        let field = match binding.kind() {
            "identifier"
            | "type_identifier"
            | "field_identifier"
            | "property_identifier"
            | "shorthand_field_identifier"
            | "shorthand_property_identifier_pattern" => return true,
            "assignment_pattern" | "object_assignment_pattern" => Some("left"),
            "pair_pattern" => Some("value"),
            "field_pattern" => {
                if let Some(pattern) = binding.child_by_field_name("pattern") {
                    binding = pattern;
                    if !contains(binding) {
                        return false;
                    }
                    continue;
                }
                return binding.child_by_field_name("name").is_some_and(|name| {
                    name.kind() == "shorthand_field_identifier" && contains(name)
                });
            }
            "tuple_pattern"
            | "slice_pattern"
            | "array_pattern"
            | "object_pattern"
            | "pattern_list"
            | "list_pattern"
            | "tuple"
            | "list"
            | "expression_list"
            | "parameters"
            | "formal_parameters"
            | "lambda_parameters"
            | "closure_parameters"
            | "rest_pattern"
            | "list_splat_pattern"
            | "dictionary_splat_pattern"
            | "mut_pattern"
            | "ref_pattern"
            | "reference_pattern"
            | "captured_pattern"
            | "struct_pattern"
            | "tuple_struct_pattern"
            | "typed_parameter" => None,
            _ => return false,
        };
        let next = if let Some(field) = field {
            binding
                .child_by_field_name(field)
                .filter(|node| contains(*node))
        } else {
            let excluded_type = binding.child_by_field_name("type").map(|node| node.id());
            binding
                .named_children(&mut binding.walk())
                .find(|node| Some(node.id()) != excluded_type && contains(*node))
        };
        let Some(next) = next else {
            return false;
        };
        binding = next;
    }
}

struct Classifier {
    tree: tree_sitter::Tree,
}

impl Classifier {
    pub(crate) fn new(path: &Path, source: &str) -> Option<Self> {
        let mut parser = Parser::new();
        parser.set_language(&language(path)?).ok()?;
        #[cfg(test)]
        let start = Instant::now();
        let tree = parser.parse(source, None)?;
        #[cfg(test)]
        CLASSIFY_NS.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        Some(Self { tree })
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
                "assignment" | "named_expression" | "short_var_declaration" => Some("left"),
                "arrow_function" => Some("parameter"),
                "formal_parameters" | "lambda_parameters" | "closure_parameters"
                | "typed_parameter" => {
                    if binding_contains(current, start, end) {
                        category = MatchCategory::Declaration;
                    }
                    None
                }
                "parameter" | "required_parameter" | "optional_parameter" => Some("pattern"),
                "default_parameter" | "typed_default_parameter" => Some("name"),
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
                    .any(|binding| binding_contains(binding, start, end));
                let fallback = current
                    .child_by_field_name("name")
                    .is_some_and(|binding| binding_contains(binding, start, end));
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

    /// Innermost scope containing the range. `block` is deliberately excluded:
    /// a local variable is only special relative to its function or type.
    fn scope(&self, start: usize, end: usize) -> Option<Scope> {
        let mut node = self.tree.root_node().descendant_for_byte_range(start, end);
        while let Some(current) = node {
            if matches!(
                current.kind(),
                "function_item"
                    | "function_definition"
                    | "function_declaration"
                    | "method_declaration"
                    | "method_definition"
                    | "constructor_declaration"
                    | "constructor_definition"
                    | "arrow_function"
                    | "function_expression"
                    | "generator_function_declaration"
                    | "class_definition"
                    | "class_declaration"
                    | "struct_item"
                    | "enum_item"
                    | "trait_item"
                    | "impl_item"
                    | "interface_declaration"
                    | "object_declaration"
                    | "record_declaration"
                    | "mod_item"
            ) {
                return Some(Scope {
                    start: current.start_byte(),
                    end: current.end_byte(),
                });
            }
            node = current.parent();
        }
        None
    }

    /// The type, class or module around the range.
    fn owner(&self, source: &str, start: usize, end: usize) -> Option<String> {
        let node = self
            .tree
            .root_node()
            .descendant_for_byte_range(start, end)?;
        owner_name(node, source.as_bytes())
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
    // Generated expressions can create tens of thousands of AST levels. Walk
    // with TreeCursor's heap-backed path rather than one Rust frame per node.
    let mut cursor = node.walk();
    loop {
        let current = cursor.node();
        if current.is_named() {
            collect_symbol(current, source, result);
        }
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return;
            }
        }
    }
}

fn collect_symbol(node: Node<'_>, source: &[u8], result: &mut Vec<Symbol>) {
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
                    owner: owner_name(node, source),
                });
            }
        }
    }
}

pub fn read_text(path: &Path) -> Result<String> {
    read_text_bounded(path, MAX_FILE_BYTES as usize)
}

/// Reject oversized input before allocating it; the bounded read also handles files
/// that grow after the metadata check.
pub fn read_text_bounded(path: &Path, limit: usize) -> Result<String> {
    let file =
        std::fs::File::open(path).with_context(|| format!("Cannot open {}", path.display()))?;
    if file.metadata()?.len() > limit as u64 {
        bail!("File exceeds the {} byte read limit", limit);
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("File exceeds the {} byte read limit", limit);
    }
    if bytes.contains(&0) {
        bail!("Binary files cannot be edited");
    }
    String::from_utf8(bytes).context("This file is not valid UTF-8")
}

/// Shared traversal rules for the explorer, file picker and project search.
fn workspace_walk(root: &Path) -> Result<WalkBuilder> {
    workspace_walk_with_ignore(root, crate::preferences::global_ignore_path().as_deref())
}

fn workspace_walk_with_ignore(root: &Path, global_ignore: Option<&Path>) -> Result<WalkBuilder> {
    let mut overrides = OverrideBuilder::new(root);
    overrides.add("!.git")?.add("!target")?;
    let mut walk = WalkBuilder::new(root);
    walk.hidden(false)
        .add_custom_ignore_filename(".rgignore")
        .overrides(overrides.build()?);
    if let Some(path) = global_ignore.filter(|path| path.is_file()) {
        // Anchor /patterns to this project, rather than the app's launch CWD
        // or ~/.glimcode. Local ignore files retain their usual precedence.
        walk.current_dir(root);
        if let Some(error) = walk.add_ignore(path) {
            return Err(error).with_context(|| {
                format!("Cannot load global ignore rules from {}", path.display())
            });
        }
    }
    Ok(walk)
}

#[derive(Default)]
pub struct WorkspacePaths {
    pub files: Vec<PathBuf>,
    pub folders: Vec<PathBuf>,
}

pub fn workspace_paths(root: &Path) -> Result<WorkspacePaths> {
    let mut paths = WorkspacePaths::default();
    for entry in workspace_walk(root)?.build() {
        let entry = entry.context("Cannot list project files")?;
        if entry.file_type().is_some_and(|kind| kind.is_file()) {
            paths.files.push(entry.into_path());
        } else if entry.depth() > 0 && entry.file_type().is_some_and(|kind| kind.is_dir()) {
            paths.folders.push(entry.into_path());
        }
    }
    paths.files.sort();
    paths.folders.sort();
    Ok(paths)
}

#[cfg(any(test, feature = "ui-capture"))]
pub fn files(root: &Path) -> Result<Vec<PathBuf>> {
    Ok(workspace_paths(root)?.files)
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
        let lines = LineIndex::new(source);
        return symbols(path, source)
            .into_iter()
            .filter(|s| s.name == query)
            .take(MAX_RESULTS)
            .map(|s| Location {
                path: path.to_path_buf(),
                line: s.line,
                column: s.column,
                preview: lines.line(source, s.line).trim_end().to_owned(),
                category: MatchCategory::Declaration,
            })
            .collect();
    }
    static WORD: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let word = WORD.get_or_init(|| regex::Regex::new(r"\w").unwrap());
    let word_character = |character: char| word.is_match(character.encode_utf8(&mut [0; 4]));
    // The file is only parsed when it holds a match at all. Most files of a
    // search do not, and a tree-sitter parse of every one of them is the
    // most expensive part of a search.
    let mut classifier: Option<Option<Classifier>> = None;
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
            let category = match &classifier {
                Some(Some(parsed)) => parsed.category(offset + column, offset + end),
                Some(None) => MatchCategory::Other,
                None => {
                    let parsed = Classifier::new(path, source);
                    let category = parsed.as_ref().map_or(MatchCategory::Other, |parsed| {
                        parsed.category(offset + column, offset + end)
                    });
                    classifier = Some(parsed);
                    category
                }
            };
            locations.push(Location {
                path: path.to_path_buf(),
                line: line + 1,
                column: column + 1,
                preview: text.trim_end().to_owned(),
                category,
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
    exclude_tests: bool,
    buffers: Vec<(PathBuf, Option<String>)>,
    priority_paths: Vec<PathBuf>,
    cancelled: Arc<AtomicBool>,
    mut emit: impl FnMut(SearchUpdate),
) {
    if cancelled.load(Ordering::Relaxed) {
        return;
    }
    let mut remaining = MAX_RESULTS;
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut excluded = std::collections::HashSet::new();
    for (path, source) in buffers {
        if cancelled.load(Ordering::Relaxed) {
            return;
        }
        if !excluded.insert(path.clone())
            || (exclude_tests && is_test_search_path(root, &canonical_root, &path))
        {
            continue;
        }
        // A file that was not changed is read here, so the interface does not
        // have to copy every open file to start a search.
        let source = match source {
            Some(source) => source,
            None => {
                let Ok(source) = read_text(&path) else {
                    continue;
                };
                source
            }
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
    for path in priority_paths {
        if cancelled.load(Ordering::Relaxed) {
            return;
        }
        if !excluded.insert(path.clone())
            || (exclude_tests && is_test_search_path(root, &canonical_root, &path))
        {
            continue;
        }
        let Ok(source) = read_text(&path) else {
            continue;
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
        exclude_tests,
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
        false,
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

#[derive(Clone, Copy)]
struct SearchCancellation<'a> {
    requested: &'a AtomicBool,
    stopped: &'a AtomicBool,
}
impl SearchCancellation<'_> {
    fn is_set(self) -> bool {
        self.requested.load(Ordering::Relaxed) || self.stopped.load(Ordering::Relaxed)
    }
}

/// Stop reading even a large file with no matches when a newer search supersedes it.
struct SearchReader<'a> {
    file: std::fs::File,
    cancellation: SearchCancellation<'a>,
}

impl Read for SearchReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.cancellation.is_set() {
            return Ok(0);
        }
        self.file.read(buffer)
    }
}

struct MatchSink<F>(F);
impl<F> Sink for MatchSink<F>
where
    F: FnMut(&SinkMatch<'_>) -> std::io::Result<bool>,
{
    type Error = std::io::Error;
    fn matched(&mut self, _: &Searcher, matched: &SinkMatch<'_>) -> std::io::Result<bool> {
        (self.0)(matched)
    }
}

fn search_file(
    path: &Path,
    matcher: &RegexMatcher,
    query: &str,
    kind: SearchKind,
    searcher: &mut Searcher,
    cancellation: SearchCancellation<'_>,
    mut emit: impl FnMut(Vec<Location>) -> bool,
) -> Result<()> {
    let mut batch = Vec::new();
    let mut last_batch = Instant::now();
    let mut first_batch = true;
    // Keep at most one syntax tree per worker, and only parse files with matches.
    let classifier = std::cell::OnceCell::new();
    // The file is read and parsed once, not once per matched line.
    let definitions = std::cell::OnceCell::new();
    let reader = SearchReader {
        file: std::fs::File::open(path)?,
        cancellation,
    };
    searcher.search_reader(
        matcher,
        reader,
        MatchSink(|matched: &SinkMatch<'_>| {
            if cancellation.is_set() {
                return Ok(false);
            }
            if kind == SearchKind::Definitions {
                let Some((source, symbols)) = definitions.get_or_init(|| {
                    read_text(path).ok().map(|source| {
                        let symbols = symbols(path, &source);
                        (source, symbols)
                    })
                }) else {
                    return Ok(false);
                };
                let lines = LineIndex::new(source);
                for symbol in symbols.iter().filter(|s| s.name == query) {
                    batch.push(Location {
                        path: path.to_path_buf(),
                        line: symbol.line,
                        column: symbol.column,
                        category: MatchCategory::Declaration,
                        preview: lines.line(source, symbol.line).trim().to_owned(),
                    });
                    if batch.len() == 64 && !emit(std::mem::take(&mut batch)) {
                        return Ok(false);
                    }
                }
                return Ok(false);
            }
            let Ok(text) = std::str::from_utf8(matched.bytes()) else {
                return Ok(true);
            };
            let classifier = classifier.get_or_init(|| {
                read_text(path)
                    .ok()
                    .and_then(|source| Classifier::new(path, &source))
            });
            let offset = matched.absolute_byte_offset() as usize;
            let mut keep_searching = true;
            matcher
                .find_iter(matched.bytes(), |found| {
                    if cancellation.is_set() {
                        keep_searching = false;
                        return false;
                    }
                    batch.push(Location {
                        path: path.to_path_buf(),
                        line: matched.line_number().unwrap_or(1) as usize,
                        column: found.start() + 1,
                        preview: text.trim_end().to_owned(),
                        category: classifier.as_ref().map_or(MatchCategory::Other, |c| {
                            c.category(offset + found.start(), offset + found.end())
                        }),
                    });
                    if first_batch
                        || batch.len() >= 64
                        || last_batch.elapsed() >= Duration::from_millis(40)
                    {
                        keep_searching = emit(std::mem::take(&mut batch));
                        first_batch = false;
                        last_batch = Instant::now();
                    }
                    keep_searching
                })
                .map_err(std::io::Error::other)?;
            Ok(keep_searching)
        }),
    )?;
    if !batch.is_empty() && !cancellation.is_set() {
        emit(batch);
    }
    Ok(())
}

// Classify relative to the workspace, so a checkout named `tests` is not all tests.
fn is_test_search_path(root: &Path, canonical_root: &Path, path: &Path) -> bool {
    crate::test_files::is_test_file(
        path.strip_prefix(root)
            .or_else(|_| path.strip_prefix(canonical_root))
            .unwrap_or(path),
    )
}

fn search_stream(
    root: &Path,
    query: &str,
    kind: SearchKind,
    exclude_tests: bool,
    excluded: &std::collections::HashSet<PathBuf>,
    limit: usize,
    cancelled: Arc<AtomicBool>,
    mut emit: impl FnMut(Vec<Location>),
) -> Result<bool> {
    if query.is_empty() || cancelled.load(Ordering::Relaxed) {
        return Ok(false);
    }
    if limit == 0 {
        return Ok(true);
    }
    let matcher = RegexMatcherBuilder::new()
        .fixed_strings(true)
        .word(kind != SearchKind::Text)
        .line_terminator(Some(b'\n'))
        .build(query)?;
    // Normalize editor aliases once, rather than resolving every scanned file.
    let canonical_root = root.canonicalize()?;
    let excluded: std::collections::HashSet<_> = excluded
        .iter()
        .flat_map(|path| {
            let resolved = path.canonicalize().ok().and_then(|path| {
                path.strip_prefix(&canonical_root)
                    .ok()
                    .map(|relative| root.join(relative))
            });
            std::iter::once(path.clone()).chain(resolved)
        })
        .collect();
    // Cap concurrent syntax trees and I/O buffers, while using more than two cores
    // on larger machines. The bounded channel provides backpressure.
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get().min(4));
    let walk = workspace_walk(root)?
        .max_filesize(Some(MAX_FILE_BYTES))
        .threads(threads)
        .build_parallel();
    let stopped = AtomicBool::new(false);
    let (sender, receiver) = std::sync::mpsc::sync_channel::<Result<Vec<Location>>>(4);
    std::thread::scope(|scope| {
        let matcher = &matcher;
        let stopped = &stopped;
        let cancelled = &*cancelled;
        let excluded = &excluded;
        let canonical_root = &canonical_root;
        scope.spawn(move || {
            walk.run(|| {
                let sender = sender.clone();
                // Clones share the compiled pattern but own their search scratch state.
                let matcher = matcher.clone();
                let mut searcher = SearcherBuilder::new()
                    .line_number(true)
                    .binary_detection(BinaryDetection::quit(b'\0'))
                    .build();
                Box::new(move |entry| {
                    if cancelled.load(Ordering::Relaxed) || stopped.load(Ordering::Relaxed) {
                        return WalkState::Quit;
                    }
                    let result = (|| -> Result<()> {
                        let entry = entry.context("Cannot walk project files")?;
                        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                            return Ok(());
                        }
                        let path = entry.path();
                        if excluded.contains(path)
                            || (exclude_tests && is_test_search_path(root, canonical_root, path))
                        {
                            return Ok(());
                        }
                        search_file(
                            path,
                            &matcher,
                            query,
                            kind,
                            &mut searcher,
                            SearchCancellation {
                                requested: cancelled,
                                stopped,
                            },
                            |batch| sender.send(Ok(batch)).is_ok(),
                        )
                        .with_context(|| format!("Cannot search {}", path.display()))
                    })();
                    if let Err(error) = result {
                        let _ = sender.send(Err(error));
                    }
                    WalkState::Continue
                })
            });
        });
        let mut delivered = 0;
        loop {
            if cancelled.load(Ordering::Relaxed) {
                stopped.store(true, Ordering::Relaxed);
                drop(receiver);
                return Ok(false);
            }
            match receiver.recv_timeout(Duration::from_millis(20)) {
                Ok(Ok(mut batch)) => {
                    batch.truncate(limit - delivered);
                    delivered += batch.len();
                    if !batch.is_empty() {
                        emit(batch);
                    }
                    if delivered == limit {
                        stopped.store(true, Ordering::Relaxed);
                        drop(receiver);
                        return Ok(true);
                    }
                }
                Ok(Err(error)) => {
                    stopped.store(true, Ordering::Relaxed);
                    drop(receiver);
                    return Err(error);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(false),
            }
        }
    })
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

/// The word around a character position.
///
/// The text is walked once, so a click in a large file does not scan it
/// again for every part of the answer.
fn word_bounds(text: &str, position: usize) -> Option<(usize, usize, char)> {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    // Byte offset of the character at `position`, without copying the text.
    let mut found = None;
    for (index, (byte, character)) in text.char_indices().enumerate() {
        if index == position {
            found = Some((byte, character));
            break;
        }
    }
    let (mut start, character) = found?;
    let mut end = start + character.len_utf8();
    let mut tail = end;
    while let Some(character) = text[tail..].chars().next() {
        if !ident(character) {
            break;
        }
        tail += character.len_utf8();
    }
    end = tail;
    while let Some(character) = text[..start].chars().next_back() {
        if !ident(character) {
            break;
        }
        start -= character.len_utf8();
    }
    Some((start, end, character))
}

pub fn word_at(text: &str, position: usize) -> String {
    word_bounds(text, position).map_or(String::new(), |(start, end, _)| text[start..end].to_owned())
}

/// Unlike keyboard navigation, a click on whitespace must not select the preceding token.
pub fn token_at(text: &str, position: usize) -> Option<String> {
    let (start, end, character) = word_bounds(text, position)?;
    if !character.is_alphanumeric() && character != '_' {
        return None;
    }
    Some(text[start..end].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deeply_nested_typescript_outline_uses_bounded_call_stack() {
        const CHILD: &str = "GLIMCODE_DEEP_OUTLINE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            // Long generated expressions produce a deep AST even with few braces.
            let source = format!(
                "export const huge = {}value;\nexport function after() {{}}\n",
                "value +\n".repeat(12_000)
            );
            std::thread::Builder::new()
                .name("outline-regression".into())
                .stack_size(256 * 1024)
                .spawn(move || {
                    let outline = symbols(Path::new("generated.ts"), &source);
                    assert!(outline.iter().any(|symbol| symbol.name == "huge"));
                    assert!(outline.iter().any(|symbol| symbol.name == "after"));
                    let syntax =
                        crate::highlighting::background(&source, "ts", true, &|| false).unwrap();
                    assert_eq!(syntax.text, source);
                    let structure = crate::editor_structure::Structure::build(&syntax);
                    let _ = crate::folding::regions(&source, &structure, "ts");
                })
                .unwrap()
                .join()
                .unwrap();
            return;
        }
        // A stack overflow aborts the process, so isolate this regression check.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "navigation::tests::deeply_nested_typescript_outline_uses_bounded_call_stack",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "outline worker crashed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn search_classifies_each_occurrence_instead_of_its_declaration_line() -> Result<()> {
        use MatchCategory::{Declaration as D, Usage as U};
        let cases: &[(&str, &str, &str, &[MatchCategory])] = &[
            (
                "rs",
                "fn demo() { let value = value(); consume(value); }",
                "value",
                &[D, U, U],
            ),
            (
                "rs",
                "fn demo() { let Widget { field: value } = input; }",
                "Widget",
                &[U],
            ),
            (
                "rs",
                "fn demo() { let Widget { field: value } = input; }",
                "field",
                &[U],
            ),
            (
                "rs",
                "fn demo() { let Widget { field: value } = input; }",
                "value",
                &[D],
            ),
            (
                "go",
                "package p\nfunc demo() { value := value(); consume(value) }",
                "value",
                &[D, U, U],
            ),
            (
                "js",
                "function demo(value = value()) { consume(value); }",
                "value",
                &[D, U, U],
            ),
            (
                "js",
                "const { value: local = value() } = input; consume(value);",
                "value",
                &[U, U, U],
            ),
            (
                "js",
                "const { value: local = value() } = input; consume(local);",
                "local",
                &[D, U],
            ),
            ("js", "const { value = value() } = input;", "value", &[D, U]),
            (
                "js",
                "const demo = value => consume(value);",
                "value",
                &[D, U],
            ),
            (
                "ts",
                "function demo(value: Value = value()) { consume(value); }",
                "value",
                &[D, U, U],
            ),
            (
                "ts",
                "function demo(value: Value = value()) { consume(value); }",
                "Value",
                &[U],
            ),
            (
                "py",
                "def demo(value: Value = value()): return value\n",
                "value",
                &[D, U, U],
            ),
            (
                "py",
                "def demo(value: Value): return value\n",
                "value",
                &[D, U],
            ),
            ("py", "obj.value = value\n", "value", &[U, U]),
            (
                "java",
                "class Demo { Value value = value(); void run() { consume(value); } }",
                "value",
                &[D, U, U],
            ),
            (
                "kt",
                "fun demo(value: Value = value()) = consume(value)",
                "value",
                &[D, U, U],
            ),
        ];
        let dir = tempfile::tempdir()?;
        for (extension, source, query, expected) in cases {
            let path = dir.path().join(format!("sample.{extension}"));
            std::fs::write(&path, source)?;
            let open = search_buffer(
                &path,
                source,
                query,
                SearchKind::Text,
                &AtomicBool::new(false),
            );
            let disk = search(dir.path(), query, SearchKind::Text)?;
            for results in [&open, &disk.locations] {
                let categories: Vec<_> = results
                    .iter()
                    .filter(|hit| hit.path == path)
                    .map(|hit| hit.category)
                    .collect();
                assert_eq!(&categories, expected, "{extension}: {source} / {query}");
            }
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    #[test]
    fn locals_are_classified_as_declarations() {
        let source =
            "fn compute() {\n    let value = 1;\n    println!(\"{value}\");\n    value\n}\n";
        let cancelled = AtomicBool::new(false);
        let locations = search_buffer(
            Path::new("src/lib.rs"),
            source,
            "value",
            SearchKind::References,
            &cancelled,
        );
        assert!(
            locations
                .iter()
                .any(|location| location.line == 2
                    && location.category == MatchCategory::Declaration),
            "a local binding is a declaration: {locations:?}"
        );
    }

    #[test]
    fn test_exclusion_covers_buffers_changed_files_and_project_for_all_search_kinds() -> Result<()>
    {
        let dir = tempfile::tempdir()?;
        // The checkout name itself must not cause production files to be excluded.
        let root = dir.path().join("tests");
        std::fs::create_dir_all(root.join("__tests__"))?;
        let source = "function needle() { needle(); }\n";
        let production = root.join("production.ts");
        let opened = root.join("opened.test.ts");
        let changed = root.join("changed.spec.js");
        let project = root.join("__tests__/helper.ts");
        let unsaved = root.join("unsaved.test.tsx");
        for path in [&production, &opened, &changed, &project] {
            std::fs::write(path, source)?;
        }
        for kind in [
            SearchKind::Text,
            SearchKind::References,
            SearchKind::Definitions,
        ] {
            for exclude_tests in [true, false] {
                let updates = std::cell::RefCell::new(vec![]);
                search_workspace(
                    &root,
                    "needle",
                    kind,
                    exclude_tests,
                    vec![
                        (opened.clone(), Some(source.into())),
                        (unsaved.clone(), Some(source.into())),
                    ],
                    vec![changed.clone(), opened.clone()],
                    Arc::new(AtomicBool::new(false)),
                    |update| updates.borrow_mut().push(update),
                );
                let updates = updates.into_inner();
                assert!(matches!(
                    updates.last(),
                    Some(SearchUpdate::Finished { truncated: false })
                ));
                let found: std::collections::BTreeSet<_> = updates
                    .into_iter()
                    .flat_map(|update| match update {
                        SearchUpdate::OpenFiles(rows) | SearchUpdate::Project(rows) => rows,
                        _ => vec![],
                    })
                    .map(|row| row.path)
                    .collect();
                let expected = if exclude_tests {
                    vec![production.clone()]
                } else {
                    vec![
                        production.clone(),
                        opened.clone(),
                        changed.clone(),
                        project.clone(),
                        unsaved.clone(),
                    ]
                };
                assert_eq!(found, expected.into_iter().collect());
            }
        }
        Ok(())
    }

    #[test]
    fn excluded_tests_cannot_exhaust_the_search_result_limit() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let test = dir.path().join("large.test.js");
        let production = dir.path().join("production.js");
        std::fs::write(&test, "needle\n".repeat(MAX_RESULTS + 1))?;
        std::fs::write(&production, "needle\n")?;
        let updates = std::cell::RefCell::new(vec![]);
        search_workspace(
            dir.path(),
            "needle",
            SearchKind::Text,
            true,
            vec![],
            vec![test],
            Arc::new(AtomicBool::new(false)),
            |update| updates.borrow_mut().push(update),
        );
        let updates = updates.into_inner();
        assert!(matches!(
            updates.last(),
            Some(SearchUpdate::Finished { truncated: false })
        ));
        let rows: Vec<_> = updates
            .into_iter()
            .flat_map(|update| match update {
                SearchUpdate::OpenFiles(rows) | SearchUpdate::Project(rows) => rows,
                _ => vec![],
            })
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, production);
        Ok(())
    }

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
            false,
            vec![(open.clone(), Some("fn needle() {} // unsaved\n".into()))],
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
            false,
            vec![(open.clone(), Some("// needle unsaved\n".into()))],
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
    fn embedded_walk_and_search_share_ignore_rules_and_include_hidden_files() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git"))?;
        std::fs::create_dir_all(root.join("target/nested"))?;
        std::fs::create_dir_all(root.join("nested/target"))?;
        std::fs::write(
            root.join(".gitignore"),
            "git-ignored.txt\nignored-folder/\n",
        )?;
        for folder in ["empty", ".hidden-folder", "ignored-folder", "target/empty"] {
            std::fs::create_dir_all(root.join(folder))?;
        }
        let indexed = workspace_paths(root)?;
        assert!(indexed.folders.contains(&root.join("empty")));
        assert!(indexed.folders.contains(&root.join(".hidden-folder")));
        assert!(!indexed.folders.contains(&root.join("ignored-folder")));
        assert!(!indexed.folders.contains(&root.join("target/empty")));
        std::fs::write(root.join(".ignore"), "ignored.txt\n")?;
        std::fs::write(root.join(".rgignore"), "rg-ignored.txt\n")?;
        for name in [
            "visible.txt",
            ".hidden.txt",
            "git-ignored.txt",
            "ignored.txt",
            "rg-ignored.txt",
            ".git/private.txt",
            "target/nested/generated.txt",
            "nested/target/generated.txt",
        ] {
            std::fs::write(root.join(name), "needle\n")?;
        }
        let paths = files(root)?;
        assert!(paths.contains(&root.join("visible.txt")));
        assert!(paths.contains(&root.join(".hidden.txt")));
        for name in [
            "git-ignored.txt",
            "ignored.txt",
            "rg-ignored.txt",
            ".git/private.txt",
            "target/nested/generated.txt",
            "nested/target/generated.txt",
        ] {
            assert!(!paths.contains(&root.join(name)), "must ignore {name}");
        }
        let results = search(root, "needle", SearchKind::Text)?;
        assert_eq!(results.locations.len(), 2);
        assert!(results.locations.iter().all(|r| paths.contains(&r.path)));
        Ok(())
    }

    #[test]
    fn global_ignore_adds_project_relative_rules_to_both_walkers_with_local_precedence()
    -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("project");
        let global = dir.path().join(".glimcode/.gitignore");
        std::fs::create_dir_all(global.parent().unwrap())?;
        for folder in [".git", "nested", "cache/empty", "generated"] {
            std::fs::create_dir_all(root.join(folder))?;
        }
        std::fs::write(
            &global,
            "*.log\ncache/\n/root-only.txt\ngenerated/*.rs\n!keep.log\nreinclude.txt\n!local-only.txt\n",
        )?;
        std::fs::write(root.join(".gitignore"), "local-only.txt\n!reinclude.txt\n")?;
        std::fs::write(root.join("nested/.gitignore"), "!nested.log\n")?;
        let visible = [
            "visible.txt",
            "keep.log",
            "reinclude.txt",
            "nested/nested.log",
            "nested/root-only.txt",
        ];
        let ignored = [
            "debug.log",
            "nested/debug.log",
            "root-only.txt",
            "local-only.txt",
            "cache/hidden.txt",
            "generated/output.rs",
        ];
        for name in visible.iter().chain(&ignored) {
            std::fs::write(root.join(name), "needle\n")?;
        }
        let walk = workspace_walk_with_ignore(&root, Some(&global))?;
        let serial = walk
            .build()
            .map(|entry| entry.map(|e| e.into_path()))
            .collect::<std::result::Result<std::collections::BTreeSet<_>, _>>()?;
        for name in visible {
            assert!(serial.contains(&root.join(name)), "must include {name}");
        }
        for name in ignored {
            assert!(!serial.contains(&root.join(name)), "must ignore {name}");
        }
        assert!(!serial.contains(&root.join("cache")));
        assert!(!serial.contains(&root.join("cache/empty")));
        let parallel = Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new()));
        walk.build_parallel().run(|| {
            let parallel = parallel.clone();
            Box::new(move |entry| {
                parallel.lock().unwrap().insert(entry.unwrap().into_path());
                WalkState::Continue
            })
        });
        assert_eq!(
            serial,
            *parallel.lock().unwrap(),
            "file listing and parallel search must agree"
        );
        // Global defaults also apply to ordinary folders without a Git repository.
        std::fs::remove_dir(root.join(".git"))?;
        let paths = workspace_walk_with_ignore(&root, Some(&global))?
            .build()
            .map(|entry| entry.map(|e| e.into_path()))
            .collect::<std::result::Result<std::collections::BTreeSet<_>, _>>()?;
        assert!(!paths.contains(&root.join("debug.log")));
        assert!(!paths.contains(&root.join("root-only.txt")));
        // The next scan picks up edits or deletion without restarting the app.
        std::fs::write(&global, "")?;
        assert!(
            workspace_walk_with_ignore(&root, Some(&global))?
                .build()
                .any(|entry| entry.is_ok_and(|e| e.path() == root.join("debug.log")))
        );
        std::fs::remove_file(&global)?;
        assert!(
            workspace_walk_with_ignore(&root, Some(&global))?
                .build()
                .any(|entry| entry.is_ok_and(|e| e.path() == root.join("debug.log")))
        );
        Ok(())
    }

    #[test]
    fn embedded_search_preserves_unicode_byte_columns_crlf_and_size_limits() -> Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("unicode.txt"),
            "猫 needle needle\r\nneedle\r\n",
        )?;
        std::fs::write(dir.path().join("binary.bin"), b"\0needle\n")?;
        let large = std::fs::File::create(dir.path().join("large.txt"))?;
        use std::io::Write;
        (&large).write_all(b"needle\n")?;
        large.set_len(MAX_FILE_BYTES + 1)?;
        let results = search(dir.path(), "needle", SearchKind::References)?;
        assert_eq!(
            results
                .locations
                .iter()
                .map(|r| (r.line, r.column))
                .collect::<Vec<_>>(),
            [(1, 5), (1, 12), (2, 1)]
        );
        assert_eq!(results.locations[0].preview, "猫 needle needle");
        assert!(
            results
                .locations
                .iter()
                .all(|r| r.path.ends_with("unicode.txt"))
        );
        Ok(())
    }

    #[test]
    fn cancelling_during_streaming_unblocks_parallel_workers() -> Result<()> {
        let dir = tempfile::tempdir()?;
        for index in 0..8 {
            std::fs::write(
                dir.path().join(format!("{index}.txt")),
                "needle\n".repeat(10_000),
            )?;
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut batches = 0;
        let truncated = search_stream(
            dir.path(),
            "needle",
            SearchKind::Text,
            false,
            &Default::default(),
            MAX_RESULTS,
            cancelled.clone(),
            |_| {
                batches += 1;
                cancelled.store(true, Ordering::Relaxed);
            },
        )?;
        assert_eq!(batches, 1);
        assert!(!truncated);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn embedded_search_supports_non_utf8_file_names() -> Result<()> {
        use std::os::unix::ffi::OsStringExt;
        let dir = tempfile::tempdir()?;
        let path = dir
            .path()
            .join(std::ffi::OsString::from_vec(b"file-\xff.txt".to_vec()));
        std::fs::write(&path, "needle")?;
        assert_eq!(files(dir.path())?, vec![path.clone()]);
        assert_eq!(
            search(dir.path(), "needle", SearchKind::Text)?.locations[0].path,
            path
        );
        Ok(())
    }

    /// Optional local comparison; requires rg only for the CLI baseline.
    #[test]
    #[ignore]
    fn search_benchmark() -> Result<()> {
        let root = PathBuf::from(std::env::var("GLIMCODE_BENCH_ROOT")?);
        let query = std::env::var("GLIMCODE_BENCH_QUERY").unwrap_or_else(|_| "main".into());
        for _ in 0..3 {
            let start = Instant::now();
            let output = std::process::Command::new("rg")
                .args([
                    "--json",
                    "--fixed-strings",
                    "--word-regexp",
                    "--hidden",
                    "-g",
                    "!.git",
                    "-g",
                    "!target",
                    "--max-filesize",
                    "20M",
                    "--",
                    &query,
                    ".",
                ])
                .current_dir(&root)
                .output()?;
            assert!(output.status.success() || output.status.code() == Some(1));
            let cli = start.elapsed();
            CLASSIFY_NS.store(0, Ordering::Relaxed);
            let start = Instant::now();
            let mut first = None;
            let mut count = 0;
            search_stream(
                &root,
                &query,
                SearchKind::References,
                false,
                &[root.join("cmd/cloud-controller-manager/main.go")].into(),
                MAX_RESULTS,
                Arc::new(AtomicBool::new(false)),
                |batch| {
                    first.get_or_insert_with(|| start.elapsed());
                    count += batch.len();
                },
            )?;
            eprintln!(
                "cli={cli:?} embedded={:?} first={first:?} hits={count} tree_parse_sum={:?}",
                start.elapsed(),
                Duration::from_nanos(CLASSIFY_NS.load(Ordering::Relaxed))
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn canonical_editor_paths_are_excluded_through_a_project_alias() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("project");
        std::fs::create_dir(&root)?;
        let open = root.join("open.txt");
        std::fs::write(&open, "needle")?;
        std::fs::write(root.join("other.txt"), "needle")?;
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias)?;
        let mut results = vec![];
        search_stream(
            &alias,
            "needle",
            SearchKind::Text,
            false,
            &[open.canonicalize()?].into(),
            MAX_RESULTS,
            Arc::new(AtomicBool::new(false)),
            |batch| results.extend(batch),
        )?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, alias.join("other.txt"));
        Ok(())
    }

    #[test]
    fn cancelled_search_publishes_nothing() {
        search_workspace(
            Path::new("/does-not-exist"),
            "needle",
            SearchKind::Text,
            false,
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
