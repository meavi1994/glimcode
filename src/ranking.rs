//! Heuristic ranking for click-to-navigation.
//!
//! There is no symbol index and no language server. A click extracts the token
//! and its local context, the normal literal search produces candidates, and
//! this module scores them with cheap structural and scope signals: the
//! clicked file's imports, the receiver's declared type, declaration shape,
//! visibility, path affinity, test/vendor noise and so on. The best candidate
//! is the one the search dialog focuses.
//!
//! Everything is recomputed per click from the clicked file plus the files that
//! actually matched, so there is nothing to invalidate when code changes.

use crate::navigation::{self, Location, MatchCategory, Symbol};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

/// Candidate files allowed to be read and parsed during one click.
const MAX_FILE_READS: usize = 60;
/// Candidates that survive the cheap path-only pass and get a full read.
const MAX_RICH_CANDIDATES: usize = 60;
/// Largest candidate file that is read for one click.
const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
/// Total candidate source kept in memory for one click.
const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameShape {
    /// `UserStore`, `T`
    Type,
    /// `MAX_SIZE`, `HTTP2`
    Constant,
    /// `user_store`, `userStore`
    Other,
}

#[derive(Clone, Debug)]
pub struct ClickContext {
    pub root: PathBuf,
    pub path: PathBuf,
    pub source: String,
    pub token: String,
    /// Byte offset of the token in `source`.
    pub byte: usize,
    /// One-based line and UTF-8 byte column, matching `Location`.
    pub line: usize,
    pub column: usize,
    pub shape: NameShape,
    /// The token is followed by `(` (or a short generic list and `(`).
    pub called: bool,
    /// `foo` in `foo.Bar` or `foo::Bar`.
    pub qualifier: Option<String>,
    /// Best-effort declared type of `qualifier`, resolved from the clicked file.
    pub receiver_type: Option<String>,
    /// Innermost function/type scope around the click.
    pub scope: Option<(usize, usize)>,
    /// The type, class or module around the click.
    pub owner: Option<String>,
    /// Byte offset of the start of every line of `source`.
    line_starts: Vec<usize>,
    pub category: MatchCategory,
    pub imports: Vec<Import>,
    pub package: Option<String>,
    pub open_paths: HashSet<PathBuf>,
    pub recent_paths: HashSet<PathBuf>,
}

impl ClickContext {
    pub fn new(
        root: &Path,
        path: &Path,
        source: &str,
        char_index: usize,
        open_paths: HashSet<PathBuf>,
        recent_paths: HashSet<PathBuf>,
    ) -> Option<Self> {
        if source.is_empty() {
            return None;
        }
        let mut byte = char_to_byte(source, char_index);
        if byte >= source.len() {
            byte = previous_char(source, source.len())?;
        }
        if !source[byte..].chars().next().is_some_and(is_ident) {
            // A caret parked at the end of a word still means that word.
            let previous = previous_char(source, byte)?;
            if source[previous..].chars().next().is_some_and(is_ident) {
                byte = previous;
            } else {
                return None;
            }
        }
        let mut start = byte;
        while let Some(previous) = previous_char(source, start) {
            if source[previous..].chars().next().is_some_and(is_ident) {
                start = previous;
            } else {
                break;
            }
        }
        let mut end = byte;
        while let Some(character) = source[end..].chars().next() {
            if is_ident(character) {
                end += character.len_utf8();
            } else {
                break;
            }
        }
        let token = source[start..end].to_owned();
        if token.is_empty() {
            return None;
        }
        let token_shape = shape(&token);
        let line = source[..start]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            + 1;
        let line_start = source[..start].rfind('\n').map_or(0, |index| index + 1);
        let column = start - line_start + 1;
        let analysis = if source.len() <= 4 * 1024 * 1024 {
            navigation::analyze_at(path, source, start, end)
        } else {
            navigation::Analysis {
                category: MatchCategory::Other,
                scope: None,
                owner: None,
            }
        };
        let extension = extension(path);
        let imports = parse_imports(source, extension);
        let qualifier = qualifier_before(source, start);
        let receiver_type = qualifier
            .as_deref()
            .and_then(|qualifier| receiver_type(source, qualifier, start, extension));
        Some(Self {
            root: root.to_path_buf(),
            path: path.to_path_buf(),
            source: source.to_owned(),
            token,
            byte: start,
            line,
            column,
            shape: token_shape,
            called: is_called(source, end),
            qualifier,
            receiver_type,
            scope: analysis.scope.map(|scope| (scope.start, scope.end)),
            owner: analysis.owner,
            line_starts: line_starts(source),
            category: analysis.category,
            imports,
            package: parse_package(source, extension),
            open_paths,
            recent_paths,
        })
    }

    fn same_package(&self, candidate: &Path, candidate_package: Option<&str>) -> bool {
        if self.path.parent() == candidate.parent() {
            return true;
        }
        matches!((self.package.as_deref(), candidate_package), (Some(a), Some(b)) if a == b)
    }
}

#[derive(Clone, Debug)]
pub struct Ranked {
    pub location: Location,
    pub score: f32,
    /// Largest contributions, used by the picker and `GLIMCODE_RANK_DEBUG`.
    pub reasons: Vec<(&'static str, f32)>,
}

/// One weight per heuristic. Keeping them in a struct makes hand tuning a
/// single-place edit and leaves room for loading overrides later.
#[derive(Clone, Debug)]
pub struct Weights {
    pub definition: f32,
    pub kind_match: f32,
    pub file_scope: f32,
    pub scope: f32,
    pub name_exact: f32,
    pub owner: f32,
    pub file_stem: f32,
    pub path_overlap: f32,
    pub visibility: f32,
    pub top_level: f32,
    pub doc_comment: f32,
    pub kind_prior: f32,
    pub uniqueness: f32,
    pub specificity: f32,
    pub proximity: f32,
    pub anchored: f32,
    pub recent: f32,
    pub focus: f32,
    pub noise: f32,
    pub huge_file: f32,
    pub duplicate: f32,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            definition: 0.45,
            kind_match: 0.20,
            file_scope: 0.36,
            scope: 0.22,
            name_exact: 0.25,
            owner: 0.40,
            file_stem: 0.12,
            path_overlap: 0.08,
            visibility: 0.08,
            top_level: 0.10,
            doc_comment: 0.06,
            kind_prior: 0.10,
            uniqueness: 0.15,
            specificity: 0.08,
            proximity: 0.10,
            anchored: 0.04,
            recent: 0.07,
            focus: 0.05,
            noise: 0.30,
            huge_file: 0.08,
            duplicate: 0.05,
        }
    }
}

/// Find every textual definition of the clicked token and rank the results.
///
/// The caller owns `buffers` (unsaved editor snapshots); those are searched
/// first and never read from disk. The rest comes from a normal `ignore`-aware
/// workspace walk, so no state survives the call.
///
/// The work stops as soon as `cancelled` is set, so a superseded click does
/// not keep reading files.
pub fn rank(
    context: &ClickContext,
    locations: Vec<Location>,
    buffers: &[(PathBuf, String)],
    cancelled: &AtomicBool,
) -> Vec<Ranked> {
    let weights = Weights::default();
    if locations.is_empty() {
        return vec![];
    }
    let uniqueness = uniqueness(&context.token, &locations);
    let snapshot: HashMap<&Path, &str> = buffers
        .iter()
        .map(|(path, source)| (path.as_path(), source.as_str()))
        .collect();

    // Cheap pass: path-only signals, so we only read the files that could win.
    let mut pre: Vec<(f32, Location)> = locations
        .into_iter()
        .map(|location| (cheap_score(context, &location), location))
        .collect();
    pre.sort_by(|a, b| b.0.total_cmp(&a.0));
    pre.truncate(MAX_RICH_CANDIDATES);

    let mut cache = FileCache::new(snapshot);
    let mut ranked: Vec<Ranked> = pre
        .into_iter()
        .filter(|_| !cancelled.load(Ordering::Relaxed))
        .map(|(_, location)| {
            let (score, reasons) = rich_score(context, &location, uniqueness, &weights, &mut cache);
            Ranked {
                location,
                score,
                reasons,
            }
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.location.path.cmp(&b.location.path))
            .then(a.location.line.cmp(&b.location.line))
    });
    if std::env::var_os("GLIMCODE_RANK_DEBUG").is_some() {
        for (index, entry) in ranked.iter().take(10).enumerate() {
            eprintln!(
                "#{index} {:.3} {}:{} {:?}",
                entry.score,
                entry.location.path.display(),
                entry.location.line,
                entry.reasons
            );
        }
    }
    ranked
}

struct FileInfo<'a> {
    text: Cow<'a, str>,
    symbols: Vec<Symbol>,
    imports: Vec<Import>,
    package: Option<String>,
}

struct FileCache<'a> {
    snapshot: HashMap<&'a Path, &'a str>,
    files: HashMap<PathBuf, Option<FileInfo<'a>>>,
    reads: usize,
    bytes: usize,
}

impl<'a> FileCache<'a> {
    fn new(snapshot: HashMap<&'a Path, &'a str>) -> Self {
        Self {
            snapshot,
            files: HashMap::new(),
            reads: 0,
            bytes: 0,
        }
    }

    fn info(&mut self, path: &Path) -> Option<&FileInfo<'a>> {
        if self.files.contains_key(path) {
            return self.files.get(path).and_then(|info| info.as_ref());
        }
        let info = if let Some(source) = self.snapshot.get(path) {
            // The editor already holds this text, so it is borrowed.
            Some(FileInfo::new(path, Cow::Borrowed(source)))
        } else if self.reads >= MAX_FILE_READS || self.bytes >= MAX_CACHE_BYTES {
            None
        } else {
            self.reads += 1;
            match navigation::read_text(path) {
                Ok(source) if source.len() <= MAX_FILE_BYTES => {
                    self.bytes += source.len();
                    Some(FileInfo::new(path, Cow::Owned(source)))
                }
                _ => None,
            }
        };
        self.files.insert(path.to_path_buf(), info);
        self.files.get(path).and_then(|info| info.as_ref())
    }
}

impl<'a> FileInfo<'a> {
    fn new(path: &Path, source: Cow<'a, str>) -> Self {
        let symbols = navigation::symbols(path, &source);
        let imports = parse_imports(&source, extension(path));
        let package = parse_package(&source, extension(path));
        Self {
            text: source,
            symbols,
            imports,
            package,
        }
    }

    /// The declaration at a position. The exact match wins; a symbol with the
    /// same name on the same line comes next; an unrelated symbol on that line
    /// is only used when it is the only one.
    fn symbol(&self, line: usize, column: usize, token: &str) -> Option<&Symbol> {
        self.symbols
            .iter()
            .find(|symbol| symbol.line == line && symbol.column == column)
            .or_else(|| {
                self.symbols
                    .iter()
                    .find(|symbol| symbol.line == line && symbol.name == token)
            })
            .or_else(|| {
                let mut on_line = self.symbols.iter().filter(|symbol| symbol.line == line);
                let first = on_line.next()?;
                on_line.next().is_none().then_some(first)
            })
    }

    fn line(&self, line: usize) -> &str {
        self.text.lines().nth(line.saturating_sub(1)).unwrap_or("")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum KindClass {
    Type,
    Function,
    Constant,
    Variable,
    Field,
    Local,
    Module,
    Unknown,
}

fn kind_class(kind: &str) -> KindClass {
    if kind.contains("function")
        || kind.contains("method")
        || kind.contains("constructor")
        || matches!(
            kind,
            "function_item" | "function_definition" | "function_declaration"
        )
    {
        KindClass::Function
    } else if kind.contains("class")
        || kind.contains("struct")
        || kind.contains("interface")
        || kind.contains("trait")
        || kind.contains("enum")
        || kind.contains("record")
        || kind.contains("type")
    {
        KindClass::Type
    } else if kind.contains("const") || kind.contains("static") || kind.contains("enum_constant") {
        KindClass::Constant
    } else if kind.contains("field") {
        KindClass::Field
    } else if kind.contains("parameter") {
        KindClass::Local
    } else if kind.contains("var") || kind.contains("declarator") || kind.contains("property") {
        KindClass::Variable
    } else if kind.contains("mod") || kind == "use_declaration" {
        KindClass::Module
    } else {
        KindClass::Unknown
    }
}

fn kind_match(shape: NameShape, called: bool, kind: KindClass) -> f32 {
    match (shape, called, kind) {
        // A type name points at a type.
        (NameShape::Type, _, KindClass::Type) => 1.0,
        (NameShape::Type, _, _) => 0.15,
        // A call points at a function.
        (_, true, KindClass::Function) => 1.0,
        (_, true, KindClass::Local | KindClass::Field) => 0.15,
        (_, true, _) => 0.35,
        // A constant name points at a constant or a variable.
        (NameShape::Constant, false, KindClass::Constant | KindClass::Variable) => 1.0,
        (NameShape::Constant, false, KindClass::Function) => 0.45,
        (NameShape::Constant, false, _) => 0.35,
        // A plain name that is not called is usually a variable.
        (NameShape::Other, false, KindClass::Variable | KindClass::Local | KindClass::Field) => {
            0.95
        }
        (NameShape::Other, false, KindClass::Function) => 0.60,
        (NameShape::Other, false, KindClass::Constant) => 0.85,
        (NameShape::Other, false, KindClass::Type) => 0.50,
        _ => 0.50,
    }
}

fn kind_prior(kind: KindClass) -> f32 {
    match kind {
        KindClass::Type => 0.9,
        KindClass::Function => 0.8,
        KindClass::Constant => 0.7,
        KindClass::Variable => 0.55,
        KindClass::Field => 0.45,
        KindClass::Local => 0.3,
        KindClass::Module => 0.35,
        KindClass::Unknown => 0.5,
    }
}

fn uniqueness(_token: &str, locations: &[Location]) -> f32 {
    let mut seen = HashSet::new();
    for location in locations {
        seen.insert((location.path.clone(), location.line, location.column));
    }
    match seen.len() {
        0 | 1 => 1.0,
        2..=3 => 0.6,
        _ => 0.2,
    }
}

fn cheap_score(context: &ClickContext, location: &Location) -> f32 {
    let mut score = 0.0;
    if location.path == context.path {
        score += 1.0;
    }
    let same_package = location.path.parent() == context.path.parent();
    if same_package {
        score += 0.4;
    }
    let imported = context
        .imports
        .iter()
        .any(|import| import_matches_path(import, &context.path, &location.path, &context.root));
    if imported {
        score += 1.0;
    }
    if is_noise(&location.path, &context.root) {
        score -= 1.2;
    }
    // The file that holds the receiver's type is the likely home of the
    // method, and this is cheap to check.
    if let Some(receiver) = &context.receiver_type
        && file_stem(&location.path).eq_ignore_ascii_case(receiver)
    {
        score += 0.6;
    }
    score += file_stem_affinity(context, &location.path);
    score += path_overlap(context, &location.path);
    score -= 0.02 * common_prefix(&context.path, &location.path) as f32;
    score
}

/// The stem of a path, without the extension.
fn file_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("")
        .to_owned()
}

fn rich_score(
    context: &ClickContext,
    location: &Location,
    uniqueness: f32,
    weights: &Weights,
    cache: &mut FileCache<'_>,
) -> (f32, Vec<(&'static str, f32)>) {
    let mut score = Score::default();
    let same_file = location.path == context.path;
    let imported = context
        .imports
        .iter()
        .any(|import| import_matches_path(import, &context.path, &location.path, &context.root));
    let info = cache.info(&location.path);
    let same_package = context.same_package(
        &location.path,
        info.and_then(|info| info.package.as_deref()),
    );
    let local = same_file || same_package;
    let symbol = info.and_then(|info| info.symbol(location.line, location.column, &context.token));
    let kind = kind_class(symbol.map_or("", |symbol| symbol.kind.as_str()));
    let line = info.map_or(location.preview.as_str(), |info| info.line(location.line));
    let indent = line.len() - line.trim_start().len();
    let top_level = indent <= 4;

    // A declaration is what the user is looking for. A mention inside a
    // comment or a string never is, so it never wins.
    let declaration = match location.category {
        MatchCategory::Declaration => 1.0,
        MatchCategory::Import => 0.45,
        MatchCategory::Usage => 0.20,
        MatchCategory::Other => 0.15,
        MatchCategory::String | MatchCategory::Comment => 0.0,
    };
    score.add("definition", weights.definition, declaration);
    score.add(
        "kind",
        weights.kind_match,
        kind_match(context.shape, context.called, kind),
    );

    // One compact "how close is this file" term, so same-file, import and
    // package evidence cannot stack into a false certainty.
    let mut file_scope: f32 = 0.0;
    if same_file {
        file_scope = 1.0;
    } else {
        if imported {
            file_scope = file_scope.max(1.0);
        }
        if context.imports.iter().any(|import| {
            import_matches_path(import, &context.path, &location.path, &context.root)
                && (import.alias.as_deref() == Some(context.token.as_str())
                    || context.qualifier.as_deref() == import.alias.as_deref())
        }) {
            file_scope = file_scope.max(0.95);
        }
        if let Some(receiver) = &context.receiver_type
            && info.is_some_and(|info| {
                info.symbols.iter().any(|symbol| {
                    symbol.name == *receiver && kind_class(&symbol.kind) == KindClass::Type
                })
            })
        {
            file_scope = file_scope.max(0.9);
        }
        if let Some(info) = info
            && info.imports.iter().any(|import| {
                import_matches_path(import, &location.path, &context.path, &context.root)
            })
        {
            file_scope = file_scope.max(0.75);
        }
        if same_package {
            file_scope = file_scope.max(0.4);
        }
    }
    score.add("file", weights.file_scope, file_scope);

    // The type the user is reaching through decides which class owns the
    // member: `store.get()` follows the type of `store`, `self.get()` the
    // type around the click.
    let wanted_owner = match (&context.qualifier, &context.receiver_type) {
        (Some(_), Some(receiver)) => Some(receiver.as_str()),
        (Some(_), None) => context.owner.as_deref(),
        _ => None,
    };
    let candidate_owner = symbol.and_then(|symbol| symbol.owner.as_deref());
    let owner_match = match (candidate_owner, wanted_owner) {
        (Some(candidate), Some(wanted)) if candidate == wanted => 1.0,
        (Some(_), Some(_)) => 0.0,
        (None, Some(_)) => 0.30,
        _ => 0.50,
    };
    score.add("owner", weights.owner, owner_match);

    let same_scope = same_file
        && context
            .scope
            .is_some_and(|(start, end)| context.byte >= start && context.byte <= end)
        && location_byte(context, location).is_some_and(|byte| {
            context
                .scope
                .is_some_and(|(start, end)| byte >= start && byte <= end)
        });
    score.add("scope", weights.scope, if same_scope { 1.0 } else { 0.0 });

    let exact = symbol.is_some_and(|symbol| symbol.name == context.token);
    score.add("name", weights.name_exact, if exact { 1.0 } else { 0.5 });
    score.add(
        "stem",
        weights.file_stem,
        file_stem_affinity(context, &location.path),
    );
    score.add(
        "path",
        weights.path_overlap,
        path_overlap(context, &location.path),
    );
    let exported = is_exported(line, extension(&location.path), &context.token);
    let visibility = if exported != local {
        1.0
    } else if exported {
        0.7
    } else {
        0.2
    };
    score.add("visible", weights.visibility, visibility);
    score.add("top", weights.top_level, if top_level { 1.0 } else { 0.0 });
    score.add(
        "doc",
        weights.doc_comment,
        if has_doc_comment(info.map(|info| info.text.as_ref()), location.line) {
            1.0
        } else {
            0.0
        },
    );
    score.add("prior", weights.kind_prior, kind_prior(kind));
    score.add("unique", weights.uniqueness, uniqueness);
    score.add("specific", weights.specificity, specificity(&context.token));
    let proximity = if same_file {
        let distance = location.line.abs_diff(context.line) as f32;
        1.0 / (1.0 + distance / 40.0)
    } else {
        let a = depth(&location.path);
        let b = depth(&context.path);
        if a.max(b) == 0 {
            0.0
        } else {
            common_prefix(&location.path, &context.path) as f32 / a.max(b) as f32
        }
    };
    score.add("near", weights.proximity, proximity);
    if same_file {
        let above = location.line < context.line
            || (location.line == context.line && location.column <= context.column);
        score.add("above", weights.anchored, if above { 1.0 } else { 0.0 });
    }
    score.add(
        "recent",
        weights.recent,
        if context.open_paths.contains(&location.path) {
            1.0
        } else if context.recent_paths.contains(&location.path) {
            0.5
        } else {
            0.0
        },
    );
    score.add(
        "focus",
        weights.focus,
        info.map_or(0.0, |info| {
            let count = info.symbols.len();
            if count <= 12 {
                1.0
            } else if count <= 40 {
                0.6
            } else {
                0.3
            }
        }),
    );
    score.add(
        "noise",
        -weights.noise,
        if is_noise(&location.path, &context.root) {
            1.0
        } else {
            0.0
        },
    );
    score.add(
        "size",
        -weights.huge_file,
        if info
            .is_some_and(|info| info.text.len() > 512 * 1024 || info.text.lines().count() > 5_000)
        {
            1.0
        } else {
            0.0
        },
    );
    let duplicate = info.is_some_and(|info| {
        info.symbols
            .iter()
            .filter(|symbol| symbol.name == context.token)
            .count()
            > 1
    });
    score.add("dup", -weights.duplicate, if duplicate { 1.0 } else { 0.0 });
    score
        .reasons
        .sort_by(|a, b| b.1.abs().total_cmp(&a.1.abs()));
    score.reasons.truncate(3);
    (score.total, score.reasons)
}

#[derive(Default)]
struct Score {
    total: f32,
    reasons: Vec<(&'static str, f32)>,
}

impl Score {
    fn add(&mut self, label: &'static str, weight: f32, value: f32) {
        let contribution = weight * value;
        if contribution.abs() >= 0.02 {
            self.reasons.push((label, contribution));
        }
        self.total += contribution;
    }
}

fn specificity(token: &str) -> f32 {
    let length = token.chars().count();
    match length {
        0..=2 => 0.1,
        3..=4 => 0.3,
        5..=7 => 0.55,
        8..=11 => 0.8,
        _ => 1.0,
    }
}

fn file_stem_affinity(context: &ClickContext, candidate: &Path) -> f32 {
    let stem = normalized(
        candidate
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(""),
    );
    let token = normalized(&context.token);
    if stem.is_empty() || token.is_empty() {
        0.0
    } else if stem == token {
        1.0
    } else if stem.contains(&token) || token.contains(&stem) {
        0.5
    } else {
        0.0
    }
}

fn path_overlap(context: &ClickContext, candidate: &Path) -> f32 {
    let words = words(&context.token);
    if words.is_empty() {
        return 0.0;
    }
    let path = candidate
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_lowercase())
        .collect::<Vec<_>>()
        .join("/");
    let hits = words
        .iter()
        .filter(|word| path.contains(word.as_str()))
        .count();
    hits as f32 / words.len() as f32
}

fn normalized(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn words(value: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for character in value.chars() {
        if character == '_' || character == '-' || character == '.' {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            previous_lower = false;
        } else if character.is_uppercase() && previous_lower {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            current.extend(character.to_lowercase());
            previous_lower = false;
        } else {
            current.extend(character.to_lowercase());
            previous_lower = character.is_lowercase() || character.is_numeric();
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn has_doc_comment(source: Option<&str>, line: usize) -> bool {
    let Some(source) = source else {
        return false;
    };
    let Some(previous) = line
        .checked_sub(2)
        .and_then(|line| source.lines().nth(line))
    else {
        return false;
    };
    let previous = previous.trim_start();
    previous.starts_with("//")
        || previous.starts_with("/*")
        || previous.starts_with('*')
        || previous.starts_with("#")
        || previous.starts_with("\"\"\"")
        || previous.starts_with("'''")
}

fn is_exported(line: &str, extension: &str, token: &str) -> bool {
    match extension {
        "go" => token.chars().next().is_some_and(char::is_uppercase),
        "rs" => line.contains("pub ") || line.contains("pub("),
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" => line.contains("export "),
        "py" => !token.starts_with('_'),
        "java" | "kt" | "kts" => {
            line.contains("public ") || !(line.contains("private ") || line.contains("protected "))
        }
        _ => true,
    }
}

fn is_noise(path: &Path, root: &Path) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    for component in relative.components() {
        let component = component.as_os_str().to_string_lossy().to_lowercase();
        if matches!(
            component.as_str(),
            "vendor"
                | "node_modules"
                | "target"
                | "dist"
                | "build"
                | "generated"
                | "gen"
                | "testdata"
                | "tests"
                | "test"
                | "spec"
                | "__tests__"
                | "docs"
                | "examples"
                | "third_party"
        ) {
            return true;
        }
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_lowercase();
    name.ends_with("_test.go")
        || name.ends_with("_test.rs")
        || name.ends_with(".test.js")
        || name.ends_with(".test.jsx")
        || name.ends_with(".test.ts")
        || name.ends_with(".test.tsx")
        || name.ends_with(".spec.js")
        || name.ends_with(".spec.ts")
        || name.ends_with(".min.js")
        || name.ends_with(".min.css")
        || name.ends_with(".generated.ts")
        || name == "cargo.lock"
        || name == "package-lock.json"
}

fn common_prefix(a: &Path, b: &Path) -> usize {
    a.components()
        .zip(b.components())
        .take_while(|(a, b)| a == b)
        .count()
}

fn depth(path: &Path) -> usize {
    path.components().count()
}

fn location_byte(context: &ClickContext, location: &Location) -> Option<usize> {
    if location.path != context.path {
        return None;
    }
    let start = *context.line_starts.get(location.line.checked_sub(1)?)?;
    let end = context
        .line_starts
        .get(location.line)
        .copied()
        .unwrap_or(context.source.len());
    // Columns from search are one-based UTF-8 byte offsets.
    let column = location
        .column
        .saturating_sub(1)
        .min(end.saturating_sub(start));
    Some(start + column)
}

/// The byte offset of the start of every line.
fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(source.match_indices('\n').map(|(index, _)| index + 1));
    starts
}

fn char_to_byte(source: &str, index: usize) -> usize {
    source
        .char_indices()
        .nth(index)
        .map_or(source.len(), |(byte, _)| byte)
}

fn previous_char(source: &str, byte: usize) -> Option<usize> {
    source[..byte]
        .char_indices()
        .next_back()
        .map(|(byte, _)| byte)
}

fn is_ident(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn shape(token: &str) -> NameShape {
    let mut characters = token.chars();
    let Some(first) = characters.next() else {
        return NameShape::Other;
    };
    let has_lowercase = token.chars().any(char::is_lowercase);
    if token.chars().any(char::is_alphabetic)
        && !has_lowercase
        && token
            .chars()
            .all(|character| !character.is_alphabetic() || character.is_uppercase())
    {
        NameShape::Constant
    } else if first.is_uppercase() {
        NameShape::Type
    } else {
        NameShape::Other
    }
}

fn is_called(source: &str, end: usize) -> bool {
    let rest = source[end..].trim_start();
    rest.starts_with('(')
        || (rest.starts_with('<') && rest.find('(').is_some_and(|index| index < 64))
}

fn qualifier_before(source: &str, start: usize) -> Option<String> {
    let before = source[..start].trim_end();
    let prefix = before
        .strip_suffix("::")
        .or_else(|| before.strip_suffix('.'))?;
    let qualifier: String = prefix
        .chars()
        .rev()
        .take_while(|character| is_ident(*character))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    (!qualifier.is_empty()).then_some(qualifier)
}

fn receiver_type(source: &str, qualifier: &str, before: usize, extension: &str) -> Option<String> {
    if extension == "py" {
        // Python is dynamically typed; declarations rarely name the receiver type.
        return None;
    }
    let window_start = source[..before]
        .rmatch_indices('\n')
        .nth(400)
        .map_or(0, |(index, _)| index + 1);
    let window = &source[window_start..before];
    let escaped = regex::escape(qualifier);
    let patterns = [
        format!(r"\b(?:let|const|var|mut|val)\s+{escaped}\s*:\s*&?(?:mut\s+)?([A-Za-z_]\w*)"),
        format!(r"\b{escaped}\s*(?::=|:)\s*&?(?:\*)?([A-Za-z_]\w*)\s*[\({{]"),
        format!(r"\b{escaped}\s+(?:\*)?([A-Za-z_]\w*)\s*(?:[=,;)])"),
        format!(r"\b{escaped}\s*=\s*&?([A-Za-z_]\w*)\("),
        // `const store = new Store()` and `let store = Store::new()`.
        format!(r"\b{escaped}\s*=\s*new\s+([A-Za-z_]\w*)"),
        format!(r"\b{escaped}\s*=\s*([A-Za-z_]\w*)\s*::"),
    ];
    // The declaration closest to the click wins, so an unrelated mention of
    // the same name further up does not take over.
    let mut found: Option<(usize, String)> = None;
    for pattern in patterns {
        let Ok(regex) = regex::Regex::new(&pattern) else {
            continue;
        };
        for capture in regex.captures_iter(window) {
            let Some(candidate) = capture.get(1) else {
                continue;
            };
            if candidate.as_str() == qualifier
                || matches!(
                    candidate.as_str(),
                    "if" | "for" | "while" | "match" | "return" | "new" | "func" | "fn"
                )
            {
                continue;
            }
            if found
                .as_ref()
                .is_none_or(|(best, _)| candidate.start() > *best)
            {
                found = Some((candidate.start(), candidate.as_str().to_owned()));
            }
        }
    }
    found.map(|(_, candidate)| candidate)
}

fn extension(path: &Path) -> &str {
    path.extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
}

fn parse_package(source: &str, extension: &str) -> Option<String> {
    if extension != "go" {
        return None;
    }
    source.lines().take(40).find_map(|line| {
        line.trim()
            .strip_prefix("package ")
            .map(|package| package.trim().to_owned())
    })
}

#[derive(Clone, Debug)]
pub struct Import {
    pub module: String,
    pub alias: Option<String>,
}

fn parse_imports(source: &str, extension: &str) -> Vec<Import> {
    let mut imports = Vec::new();
    let mut go_block = false;
    for (index, raw) in source.lines().enumerate() {
        if index > 800 && !go_block {
            break;
        }
        let line = raw.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if extension == "go" {
            if go_block {
                if line.starts_with(')') {
                    go_block = false;
                    continue;
                }
                if let Some((alias, module)) = parse_go_spec(line) {
                    imports.push(Import { module, alias });
                }
                continue;
            }
            if line.starts_with("import (") {
                go_block = true;
                continue;
            }
            if let Some(rest) = line.strip_prefix("import ")
                && let Some((alias, module)) = parse_go_spec(rest.trim())
            {
                imports.push(Import { module, alias });
            }
            continue;
        }
        match extension {
            "py" => {
                if let Some(rest) = line.strip_prefix("from ") {
                    if let Some((module, names)) = rest.split_once(" import ") {
                        let module = module.trim().to_owned();
                        for name in names.trim_matches(['(', ')']).split(',') {
                            let name = name.trim();
                            if name.is_empty() || name == "*" {
                                continue;
                            }
                            let (original, alias) = match name.split_once(" as ") {
                                Some((original, alias)) => {
                                    (original.trim(), Some(alias.trim().to_owned()))
                                }
                                None => (name, None),
                            };
                            imports.push(Import {
                                module: module.clone(),
                                alias: Some(alias.unwrap_or_else(|| original.to_owned())),
                            });
                        }
                    }
                } else if let Some(rest) = line.strip_prefix("import ")
                    && !rest.starts_with('(')
                {
                    for part in rest.split(',') {
                        let part = part.trim();
                        if part.is_empty() {
                            continue;
                        }
                        let (module, alias) = match part.split_once(" as ") {
                            Some((module, alias)) => (module.trim(), Some(alias.trim().to_owned())),
                            None => (part, None),
                        };
                        imports.push(Import {
                            module: module.to_owned(),
                            alias,
                        });
                    }
                }
            }
            "rs" => {
                if let Some(rest) = line.strip_prefix("use ") {
                    parse_rust_use(rest.trim_end_matches(';'), &mut imports);
                }
            }
            "java" | "kt" | "kts" => {
                if let Some(rest) = line.strip_prefix("import ") {
                    let module = rest.trim_end_matches(';').trim();
                    if !module.is_empty() && !module.starts_with("static ") {
                        let alias = module
                            .rsplit('.')
                            .next()
                            .filter(|part| part.chars().next().is_some_and(char::is_uppercase))
                            .map(str::to_owned);
                        imports.push(Import {
                            module: module.to_owned(),
                            alias,
                        });
                    }
                }
            }
            _ => {
                if let Some(rest) = line.strip_prefix("import ") {
                    parse_js_import(rest, &mut imports);
                }
            }
        }
    }
    imports
}

fn parse_go_spec(spec: &str) -> Option<(Option<String>, String)> {
    let spec = spec.trim().trim_end_matches(',');
    if spec.is_empty() {
        return None;
    }
    if let Some(module) = spec
        .strip_prefix('"')
        .and_then(|rest| rest.split('"').next())
    {
        return Some((None, module.to_owned()));
    }
    let mut parts = spec.split_whitespace();
    let alias = parts.next()?.trim_matches('"');
    let module = parts.next()?.trim_matches('"');
    if alias.is_empty() || module.is_empty() {
        return None;
    }
    Some((
        (!matches!(alias, "_" | ".")).then(|| alias.to_owned()),
        module.to_owned(),
    ))
}

fn parse_js_import(rest: &str, imports: &mut Vec<Import>) {
    let rest = rest.trim();
    let (head, module) = match rest.rsplit_once(" from ") {
        Some((head, module)) => (head.trim(), module.trim()),
        None => ("", rest),
    };
    let module = module
        .trim_end_matches(';')
        .trim()
        .trim_matches(['"', '\'', '`']);
    if module.is_empty() {
        return;
    }
    let mut push = |alias: Option<String>| {
        imports.push(Import {
            module: module.to_owned(),
            alias,
        });
    };
    let head = head.strip_prefix("type ").unwrap_or(head);
    if let Some(index) = head.find("* as ") {
        let alias = head[index + 5..]
            .split(|character: char| character.is_whitespace() || character == ',')
            .next()
            .unwrap_or("")
            .to_owned();
        if !alias.is_empty() {
            push(Some(alias));
        }
        return;
    }
    if let Some(open) = head.find('{')
        && let Some(close) = head.rfind('}')
    {
        for item in head[open + 1..close].split(',') {
            let item = item.trim();
            if item.is_empty() || item == "type" {
                continue;
            }
            let alias = item.split_whitespace().last().unwrap_or(item);
            push(Some(alias.to_owned()));
        }
        return;
    }
    let alias = head.split_whitespace().next().unwrap_or("");
    if !alias.is_empty() && alias != "default" {
        push(Some(alias.to_owned()));
    } else {
        push(None);
    }
}

fn parse_rust_use(rest: &str, imports: &mut Vec<Import>) {
    let (path, alias) = match rest.rsplit_once(" as ") {
        Some((path, alias)) => (path.trim(), Some(alias.trim().to_owned())),
        None => (rest, None),
    };
    if let Some(open) = path.find('{')
        && let Some(close) = path.rfind('}')
    {
        let module = path[..open].trim_end_matches("::").trim();
        for item in path[open + 1..close].split(',') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            let (name, alias) = match item.split_once(" as ") {
                Some((name, alias)) => (name.trim(), Some(alias.trim().to_owned())),
                None => (item, None),
            };
            imports.push(Import {
                module: module.to_owned(),
                alias: Some(alias.unwrap_or_else(|| name.to_owned())),
            });
        }
        return;
    }
    let module = path.trim_end_matches("::").trim();
    let alias = alias.or_else(|| {
        module
            .rsplit("::")
            .next()
            .filter(|part| part.chars().next().is_some_and(char::is_uppercase))
            .map(str::to_owned)
    });
    imports.push(Import {
        module: module.to_owned(),
        alias,
    });
}

fn import_matches_path(import: &Import, importer: &Path, candidate: &Path, root: &Path) -> bool {
    let module = import
        .module
        .trim()
        .trim_matches(['"', '\'', '`'])
        .trim_end_matches(';');
    if module.is_empty() {
        return false;
    }
    if module.starts_with("./") || module.starts_with("../") {
        let base = normalize(&importer.parent().unwrap_or(Path::new("")).join(module));
        let candidate = normalize(candidate);
        if candidate == base {
            return true;
        }
        if let Ok(rest) = candidate.strip_prefix(&base)
            && rest.components().count() <= 2
        {
            return true;
        }
        return candidate.file_stem() == base.file_stem()
            && candidate.parent().map(normalize) == base.parent().map(normalize);
    }
    let cleaned = module
        .trim_start_matches("crate::")
        .trim_start_matches("self::")
        .trim_start_matches("super::");
    let raw: Vec<&str> = cleaned
        .split(['.', '/', ':'])
        .filter(|part| !part.is_empty() && *part != "crate" && *part != "self" && *part != "super")
        .collect();
    if raw.is_empty() {
        return false;
    }
    let last = *raw.last().unwrap();
    let last_is_type = last.chars().next().is_some_and(char::is_uppercase);
    let directories = &raw[..raw.len() - usize::from(last_is_type)];
    let relative = candidate.strip_prefix(root).unwrap_or(candidate);
    let components: Vec<String> = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_lowercase())
        .collect();
    if directories.is_empty() {
        return candidate
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.eq_ignore_ascii_case(last));
    }
    let directories = directories
        .iter()
        .map(|part| part.to_lowercase())
        .collect::<Vec<_>>();
    for take in (1..=directories.len().min(3)).rev() {
        let suffix = &directories[directories.len() - take..];
        let directory_components = &components[..components.len().saturating_sub(1)];
        if directory_components.ends_with(suffix) {
            return true;
        }
    }
    false
}

fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                result.pop();
            }
            std::path::Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, relative: &str, source: &str) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, source).unwrap();
        path
    }

    fn location(path: &Path, line: usize, column: usize, preview: &str) -> Location {
        Location {
            path: path.to_path_buf(),
            line,
            column,
            preview: preview.to_owned(),
            category: MatchCategory::Declaration,
        }
    }

    fn click(root: &Path, path: &Path, source: &str, token: &str) -> ClickContext {
        let byte = source.find(token).unwrap();
        let index = source[..byte].chars().count();
        ClickContext::new(root, path, source, index, HashSet::new(), HashSet::new()).unwrap()
    }

    #[test]
    fn same_file_definition_beats_a_distant_namesake() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let a = write(
            root,
            "src/user.go",
            "package user\n\ntype User struct{}\n\nfunc New() User { return User{} }\n",
        );
        let b = write(
            root,
            "src/order.go",
            "package order\n\ntype User struct{}\n",
        );
        let source = fs::read_to_string(&a).unwrap();
        let context = click(root, &a, &source, "User");
        let ranked = rank(
            &context,
            vec![
                location(&b, 3, 6, "type User struct{}"),
                location(&a, 3, 6, "type User struct{}"),
            ],
            &[],
            &AtomicBool::new(false),
        );
        assert_eq!(ranked[0].location.path, a);
        assert!(ranked[0].score > ranked[1].score);
    }

    #[test]
    fn imported_package_beats_an_unrelated_namesake() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let app = write(
            root,
            "cmd/app.go",
            "package main\n\nimport \"example.com/store\"\n\nfunc run() { _ = store.Store{} }\n",
        );
        let store = write(
            root,
            "store/store.go",
            "package store\n\ntype Store struct{}\n",
        );
        let other = write(
            root,
            "other/store.go",
            "package other\n\ntype Store struct{}\n",
        );
        let source = fs::read_to_string(&app).unwrap();
        let context = click(root, &app, &source, "Store");
        let ranked = rank(
            &context,
            vec![
                location(&other, 3, 6, "type Store struct{}"),
                location(&store, 3, 6, "type Store struct{}"),
            ],
            &[],
            &AtomicBool::new(false),
        );
        assert_eq!(ranked[0].location.path, store);
    }

    #[test]
    fn pointer_receiver_types_drop_the_star() {
        let source =
            "package main\n\nfunc (g *Generator) Run() {}\n\nfunc main() {\n\tg.Run()\n}\n";
        let before = source.find("g.Run").unwrap() + "g.".len();
        assert_eq!(
            receiver_type(source, "g", before, "go").as_deref(),
            Some("Generator")
        );
    }

    #[test]
    fn receiver_type_file_wins_for_method_clicks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let app = write(
            root,
            "main.go",
            "package main\n\nfunc run() {\n\tstore := Store{}\n\tstore.Open()\n}\n",
        );
        let model = write(
            root,
            "model/store.go",
            "package model\n\ntype Store struct{}\n\nfunc (s Store) Open() {}\n",
        );
        let other = write(
            root,
            "other/open.go",
            "package other\n\ntype Other struct{}\n\nfunc (o Other) Open() {}\n",
        );
        let source = fs::read_to_string(&app).unwrap();
        let byte = source.find("store.Open").unwrap() + "store.".len();
        let index = source[..byte].chars().count();
        let context =
            ClickContext::new(root, &app, &source, index, HashSet::new(), HashSet::new()).unwrap();
        assert_eq!(context.receiver_type.as_deref(), Some("Store"));
        let ranked = rank(
            &context,
            vec![
                location(&other, 5, 1, "func (o Other) Open() {}"),
                location(&model, 5, 1, "func (s Store) Open() {}"),
            ],
            &[],
            &AtomicBool::new(false),
        );
        assert_eq!(ranked[0].location.path, model);
    }

    #[test]
    fn tests_and_vendor_are_penalized() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let source_path = write(root, "src/thing.ts", "export class Thing {}\n");
        let test_path = write(root, "src/thing.test.ts", "export class Thing {}\n");
        let vendor_path = write(root, "vendor/thing.ts", "export class Thing {}\n");
        let source = fs::read_to_string(&source_path).unwrap();
        let context = click(root, &source_path, &source, "Thing");
        let ranked = rank(
            &context,
            vec![
                location(&test_path, 1, 14, "export class Thing {}"),
                location(&vendor_path, 1, 14, "export class Thing {}"),
                location(&source_path, 1, 14, "export class Thing {}"),
            ],
            &[],
            &AtomicBool::new(false),
        );
        assert_eq!(ranked[0].location.path, source_path);
        assert!(ranked[1].score < ranked[0].score);
        assert!(ranked[2].score < ranked[0].score);
    }

    #[test]
    fn comments_are_not_treated_as_code_clicks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let path = write(
            root,
            "lib.rs",
            "// Store is documented here\npub struct Store;\n",
        );
        let source = fs::read_to_string(&path).unwrap();
        let context = click(root, &path, &source, "Store");
        assert_eq!(context.category, MatchCategory::Comment);
    }

    #[test]
    fn unsaved_buffer_text_is_used_for_scoring() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let path = write(root, "src/lib.ts", "export class Old {}\n");
        let unsaved = "export class Fresh {}\n";
        let context = click(root, &path, unsaved, "Fresh");
        let ranked = rank(
            &context,
            vec![location(&path, 1, 14, "export class Fresh {}")],
            &[(path.clone(), unsaved.to_owned())],
            &AtomicBool::new(false),
        );
        assert_eq!(ranked.len(), 1);
    }

    #[test]
    fn a_method_of_the_receiver_type_beats_the_same_name_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let app = write(
            root,
            "src/app.ts",
            "class Store {}\n\nfunction run() {\n  const store = new Store();\n  store.open();\n}\n",
        );
        let model = write(
            root,
            "src/model.ts",
            "export class Store {\n  open() {}\n}\n",
        );
        let other = write(
            root,
            "src/other.ts",
            "export class Cart {\n  open() {}\n}\n",
        );
        let source = fs::read_to_string(&app).unwrap();
        let byte = source.find("store.open").unwrap() + "store.".len();
        let index = source[..byte].chars().count();
        let context =
            ClickContext::new(root, &app, &source, index, HashSet::new(), HashSet::new()).unwrap();
        assert_eq!(context.receiver_type.as_deref(), Some("Store"));
        let ranked = rank(
            &context,
            vec![
                location(&other, 2, 3, "  open() {}"),
                location(&model, 2, 3, "  open() {}"),
            ],
            &[],
            &AtomicBool::new(false),
        );
        assert_eq!(ranked[0].location.path, model);
    }

    #[test]
    fn a_method_of_the_enclosing_type_wins_for_a_self_call() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let app = write(
            root,
            "src/app.rs",
            "struct Store {\n}\n\nimpl Store {\n    fn run(&self) {\n        self.open();\n    }\n}\n",
        );
        let model = write(
            root,
            "src/model.rs",
            "struct Cart;\n\nimpl Cart {\n    fn open(&self) {}\n}\n",
        );
        let source = fs::read_to_string(&app).unwrap();
        let byte = source.find("self.open").unwrap() + "self.".len();
        let index = source[..byte].chars().count();
        let context =
            ClickContext::new(root, &app, &source, index, HashSet::new(), HashSet::new()).unwrap();
        assert_eq!(context.owner.as_deref(), Some("Store"));
        let ranked = rank(
            &context,
            vec![
                location(&model, 4, 9, "    fn open(&self) {}"),
                location(&app, 6, 9, "    fn open(&self) {}"),
            ],
            &[],
            &AtomicBool::new(false),
        );
        assert_eq!(ranked[0].location.path, app);
    }

    #[test]
    fn go_methods_know_their_receiver_type() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let app = write(
            root,
            "main.go",
            "package main\n\nfunc run() {\n\tstore := Store{}\n\tstore.Open()\n}\n",
        );
        let model = write(
            root,
            "model/store.go",
            "package model\n\ntype Store struct{}\n\nfunc (s Store) Open() {}\n",
        );
        let other = write(
            root,
            "other/open.go",
            "package other\n\ntype Other struct{}\n\nfunc (o Other) Open() {}\n",
        );
        let source = fs::read_to_string(&app).unwrap();
        let byte = source.find("store.Open").unwrap() + "store.".len();
        let index = source[..byte].chars().count();
        let context =
            ClickContext::new(root, &app, &source, index, HashSet::new(), HashSet::new()).unwrap();
        let ranked = rank(
            &context,
            vec![
                location(&other, 5, 1, "func (o Other) Open() {}"),
                location(&model, 5, 1, "func (s Store) Open() {}"),
            ],
            &[],
            &AtomicBool::new(false),
        );
        assert_eq!(ranked[0].location.path, model);
    }

    #[test]
    fn a_match_in_a_comment_never_outranks_a_declaration() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let path = write(root, "src/lib.rs", "pub struct Store;\n");
        let source = fs::read_to_string(&path).unwrap();
        let context = click(root, &path, &source, "Store");
        let mut comment = location(&path, 1, 1, "// Store is documented");
        comment.category = MatchCategory::Comment;
        let ranked = rank(
            &context,
            vec![comment, location(&path, 1, 12, "pub struct Store;")],
            &[],
            &AtomicBool::new(false),
        );
        assert_eq!(ranked[0].location.column, 12);
    }

    #[test]
    fn a_cancelled_click_stops_ranking() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let path = write(root, "src/lib.rs", "pub struct Store;\n");
        let source = fs::read_to_string(&path).unwrap();
        let context = click(root, &path, &source, "Store");
        let cancelled = AtomicBool::new(true);
        let ranked = rank(
            &context,
            vec![location(&path, 1, 12, "pub struct Store;")],
            &[],
            &cancelled,
        );
        assert!(ranked.is_empty());
    }
}
