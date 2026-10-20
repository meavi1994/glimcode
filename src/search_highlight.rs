//! Syntax highlighting for the Find in Files result list.
//!
//! A result row shows a single line of a matched file. Coloring that line on
//! its own is unreliable - a keyword inside a block comment or a multi-line
//! string would be mis-colored - so the whole file is highlighted once in the
//! background and every row slices the sections it needs.
//!
//! The cache is bounded by file count and total bytes, so a search over a
//! large repository cannot grow the footprint without limit.

use crate::highlighting;
use eframe::egui::text::LayoutJob;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
};

/// Files larger than this are shown without syntax colors.
const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
/// How many files stay highlighted at once.
const MAX_FILES: usize = 12;
/// How much highlighted source may be retained.
const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;

/// The source of one file plus the syntax sections of every line.
pub struct FileLines {
    source: String,
    lines: Vec<Range<usize>>,
    syntax: LayoutJob,
}

impl FileLines {
    fn build(
        path: &Path,
        source: String,
        dark: bool,
        cancelled: &impl Fn() -> bool,
    ) -> Option<Self> {
        if source.len() > MAX_FILE_BYTES {
            return None;
        }
        let mut lines = vec![];
        let mut offset = 0;
        for line in source.split_inclusive('\n') {
            let content = line.trim_end_matches(['\r', '\n']);
            lines.push(offset..offset + content.len());
            offset += line.len();
        }
        if source.is_empty() || source.ends_with('\n') {
            lines.push(source.len()..source.len());
        }
        let syntax = highlighting::background(
            &source,
            path.extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or("txt"),
            dark,
            cancelled,
        )?;
        Some(Self {
            source,
            lines,
            syntax,
        })
    }

    fn bytes(&self) -> usize {
        self.source.len()
            + self.syntax.text.len()
            + self.lines.len() * size_of::<Range<usize>>()
            + self.syntax.sections.len() * size_of::<eframe::egui::text::LayoutSection>()
    }

    /// The highlighted text of a one-based line, or `None` when the file has
    /// no such line.
    pub fn line(&self, line: usize) -> Option<LayoutJob> {
        let range = self.lines.get(line.checked_sub(1)?)?.clone();
        let mut job = LayoutJob {
            text: self.source[range.clone()].to_owned(),
            ..Default::default()
        };
        let first = self
            .syntax
            .sections
            .partition_point(|section| section.byte_range.end <= range.start);
        for section in &self.syntax.sections[first..] {
            if section.byte_range.start >= range.end {
                break;
            }
            let mut part = section.clone();
            part.byte_range = part.byte_range.start.max(range.start) - range.start
                ..part.byte_range.end.min(range.end) - range.start;
            job.sections.push(part);
        }
        Some(job)
    }
}

struct Request {
    version: u64,
    path: PathBuf,
    source: Option<String>,
    dark: bool,
}

/// Highlights the files behind the visible search rows, off the UI thread.
pub struct SearchHighlight {
    tx: Option<Sender<Request>>,
    rx: Receiver<(u64, PathBuf, Arc<FileLines>)>,
    version: Arc<AtomicU64>,
    files: HashMap<PathBuf, Arc<FileLines>>,
    order: VecDeque<PathBuf>,
    bytes: usize,
    pending: HashSet<PathBuf>,
    dark: bool,
}

impl Default for SearchHighlight {
    fn default() -> Self {
        let (tx, requests) = mpsc::channel::<Request>();
        let (results, rx) = mpsc::channel();
        let version = Arc::new(AtomicU64::new(0));
        let latest = version.clone();
        std::thread::Builder::new()
            .name("search-highlight".into())
            .spawn(move || {
                while let Ok(request) = requests.recv() {
                    // A newer request (a cleared search or a theme change)
                    // invalidates this one.
                    let stopped = || latest.load(Ordering::Relaxed) != request.version;
                    if stopped() {
                        continue;
                    }
                    let source = match request.source {
                        Some(source) => source,
                        None => match crate::navigation::read_text(&request.path) {
                            Ok(source) => source,
                            Err(_) => continue,
                        },
                    };
                    let Some(lines) =
                        FileLines::build(&request.path, source, request.dark, &stopped)
                    else {
                        continue;
                    };
                    if stopped() {
                        continue;
                    }
                    if results
                        .send((request.version, request.path, Arc::new(lines)))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .ok();
        Self {
            tx: Some(tx),
            rx,
            version,
            files: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            pending: HashSet::new(),
            dark: true,
        }
    }
}

impl SearchHighlight {
    /// The highlighted line of a file, when that file has been highlighted.
    pub fn line(&self, path: &Path, line: usize) -> Option<LayoutJob> {
        self.files.get(path)?.line(line)
    }

    /// Ask for a file to be highlighted. Repeated requests for a file that is
    /// already cached or in flight are ignored, and the source is only
    /// produced when a request is actually sent.
    pub fn request(&mut self, path: &Path, source: impl FnOnce() -> Option<String>) {
        if self.files.contains_key(path) || self.pending.contains(path) {
            return;
        }
        let Some(tx) = &self.tx else {
            return;
        };
        let version = self.version.load(Ordering::Relaxed);
        let source = source();
        self.pending.insert(path.to_path_buf());
        let _ = tx.send(Request {
            version,
            path: path.to_path_buf(),
            source,
            dark: self.dark,
        });
    }

    /// Adopt the results that arrived in the background.
    pub fn poll(&mut self) {
        for (version, path, lines) in self.rx.try_iter() {
            self.pending.remove(&path);
            if version != self.version.load(Ordering::Relaxed) {
                continue;
            }
            if let Some(previous) = self.files.insert(path.clone(), lines.clone()) {
                self.bytes = self.bytes.saturating_sub(previous.bytes());
            }
            self.bytes += lines.bytes();
            if !self.order.contains(&path) {
                self.order.push_back(path);
            }
            while self.order.len() > MAX_FILES || self.bytes > MAX_TOTAL_BYTES {
                let Some(oldest) = self.order.pop_front() else {
                    break;
                };
                if let Some(previous) = self.files.remove(&oldest) {
                    self.bytes = self.bytes.saturating_sub(previous.bytes());
                }
            }
        }
    }

    /// Follow a theme change. Everything highlighted so far used the old
    /// colors, so it is dropped and requested again.
    pub fn set_dark(&mut self, dark: bool) {
        if dark == self.dark {
            return;
        }
        self.dark = dark;
        self.version.fetch_add(1, Ordering::Relaxed);
        self.files.clear();
        self.order.clear();
        self.bytes = 0;
        self.pending.clear();
    }

    /// Drop everything, for example when a new search replaces the results.
    pub fn clear(&mut self) {
        self.version.fetch_add(1, Ordering::Relaxed);
        self.files.clear();
        self.order.clear();
        self.bytes = 0;
        self.pending.clear();
    }
}

impl Drop for SearchHighlight {
    fn drop(&mut self) {
        self.version.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust_lines(source: &str) -> FileLines {
        FileLines::build(Path::new("a.rs"), source.to_owned(), true, &|| false).unwrap()
    }

    #[test]
    fn lines_are_sliced_out_of_the_full_file() {
        let lines = rust_lines("fn one() {}\nfn two() {}\n");
        assert_eq!(lines.line(1).unwrap().text, "fn one() {}");
        assert_eq!(lines.line(2).unwrap().text, "fn two() {}");
        // A trailing newline leaves an empty last line, as in the editor.
        assert_eq!(lines.line(3).unwrap().text, "");
        assert!(lines.line(4).is_none());
        assert!(lines.line(0).is_none());
    }

    #[test]
    fn a_line_keeps_the_syntax_colors_of_its_sections() {
        let lines = rust_lines("fn one() {}\nlet value = 1;\n");
        let job = lines.line(2).unwrap();
        assert!(job.sections.len() >= 2, "expected several styled sections");
        assert!(
            job.sections
                .iter()
                .all(|section| { section.byte_range.end <= job.text.len() })
        );
        // The keyword and the number must not share a color.
        let colors: Vec<_> = job
            .sections
            .iter()
            .map(|section| section.format.color)
            .collect();
        assert!(
            colors.windows(2).any(|pair| pair[0] != pair[1]),
            "expected more than one color on the line"
        );
    }

    #[test]
    fn oversized_files_are_not_highlighted() {
        let source = "x".repeat(MAX_FILE_BYTES + 1);
        assert!(FileLines::build(Path::new("a.rs"), source, true, &|| false).is_none());
    }
}
