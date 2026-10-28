use crate::{
    navigation::{self, Symbol},
    sheet::Sheet,
};
use anyhow::{Result, bail};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

/// The editable text and saved baseline share their allocation until an edit.
/// TextEdit only requests mutable storage when it inserts or removes text.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SharedText(Arc<String>);

impl From<String> for SharedText {
    fn from(text: String) -> Self {
        Self(Arc::new(text))
    }
}

impl From<&str> for SharedText {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

impl std::ops::Deref for SharedText {
    type Target = String;
    fn deref(&self) -> &String {
        &self.0
    }
}

impl std::ops::DerefMut for SharedText {
    fn deref_mut(&mut self) -> &mut String {
        Arc::make_mut(&mut self.0)
    }
}

impl eframe::egui::TextBuffer for SharedText {
    fn is_mutable(&self) -> bool {
        true
    }
    fn as_str(&self) -> &str {
        self.0.as_str()
    }
    fn insert_text(&mut self, text: &str, char_index: usize) -> usize {
        if text.is_empty() {
            return 0;
        }
        eframe::egui::TextBuffer::insert_text(Arc::make_mut(&mut self.0), text, char_index)
    }
    fn delete_char_range(&mut self, range: std::ops::Range<usize>) {
        if !range.is_empty() {
            eframe::egui::TextBuffer::delete_char_range(Arc::make_mut(&mut self.0), range);
        }
    }
    fn type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<Self>()
    }
}

// A revision also identifies a newly opened or reloaded document at the same path.
fn next_revision() -> u64 {
    static REVISION: AtomicU64 = AtomicU64::new(1);
    REVISION.fetch_add(1, Ordering::Relaxed)
}

pub enum Content {
    Text(SharedText),
    Csv(Box<Sheet>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MarkdownMode {
    Edit,
    Split,
    Preview,
}

struct CsvState {
    delimiter: u8,
    has_headers: bool,
    source: String,
    grid_snapshot: String,
}

pub struct Document {
    pub path: PathBuf,
    pub content: Content,
    pub dirty: bool,
    csv: Option<CsvState>,
    pub csv_error: Option<String>,
    pub outline: Vec<Symbol>,
    pub outline_refresh: Option<Instant>,
    outline_worker: Option<(u64, Receiver<Vec<Symbol>>)>,
    pub markdown: MarkdownMode,
    pub cursor: usize,
    pub jump: Option<(usize, usize)>,
    pub editor_id: Option<eframe::egui::Id>,
    /// The line ending of the file, shown in the status bar.
    pub line_ending: &'static str,
    pub(crate) original: SharedText,
    /// Bumped whenever the text changes, so the cached cursor position is
    /// refreshed.
    revision: u64,
    /// Cached cursor position: (revision, cursor, byte, line, line start).
    cursor_position: std::cell::Cell<Option<CursorPosition>>,
    /// Fingerprint of the file on disk, used to skip needless reads.
    disk_stamp: Option<DiskStamp>,
    /// Hash of the file on disk. The text itself is not kept, so an open
    /// document holds one copy less.
    disk_hash: Option<u64>,
    disk_checked: Instant,
    /// Avoid retrying an invalid disk revision on every UI frame.
    auto_reload_stamp: Option<DiskStamp>,
    /// Hash of the buffer as it is now: (revision, hash). The status bar asks
    /// whether the file changed on disk every frame, and hashing the whole
    /// text again each time is expensive in a large file.
    buffer_hash: std::cell::Cell<Option<(u64, u64)>>,
    /// Hash of the text as it was loaded or saved.
    original_hash: std::cell::Cell<Option<u64>>,
}

/// Cheap fingerprint of a file: its size and modification time.
type DiskStamp = (u64, u64);

/// The cached cursor position: the revision it belongs to, the cursor, the
/// byte offset of the cursor, the line and the start of the line.
type CursorPosition = (u64, usize, usize, usize, usize);

fn content_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn disk_stamp(path: &Path) -> Option<DiskStamp> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let nanos = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos() as u64;
    Some((nanos, meta.len()))
}

impl Document {
    pub fn open(path: PathBuf, headers: bool) -> Result<Self> {
        // Resolve symlinks so atomic saves update the target, not the symlink itself.
        let path = path.canonicalize()?;
        let source: SharedText = navigation::read_text(&path)?.into();
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        let source_path = path.clone();
        let content = match extension.as_str() {
            "csv" => Content::Csv(Box::new(Sheet::parse(
                &source,
                crate::sheet::detect_delimiter(&source),
                headers,
            )?)),
            "tsv" => Content::Csv(Box::new(Sheet::parse(&source, b'\t', headers)?)),
            _ => Content::Text(source.clone()),
        };
        let csv = if let Content::Csv(sheet) = &content {
            Some(CsvState {
                delimiter: sheet.delimiter(),
                has_headers: headers,
                source: source.to_string(),
                grid_snapshot: sheet.serialize()?,
            })
        } else {
            None
        };
        let large = crate::editor_analysis::is_large(&source);
        let outline = if large {
            Vec::new()
        } else {
            navigation::symbols(&path, &source)
        };
        Ok(Self {
            path,
            content,
            dirty: false,
            csv,
            csv_error: None,
            outline,
            outline_refresh: large.then(Instant::now),
            outline_worker: None,
            markdown: MarkdownMode::Split,
            cursor: 0,
            jump: None,
            editor_id: None,
            line_ending: if source.contains("\r\n") {
                "CRLF"
            } else {
                "LF"
            },
            revision: next_revision(),
            cursor_position: std::cell::Cell::new(None),
            disk_stamp: disk_stamp(&source_path),
            disk_hash: Some(content_hash(&source)),
            disk_checked: Instant::now(),
            auto_reload_stamp: None,
            buffer_hash: std::cell::Cell::new(None),
            original_hash: std::cell::Cell::new(Some(content_hash(&source))),
            original: source,
        })
    }

    /// Keep full-file symbol parsing out of the render loop, including after edits.
    pub fn poll_outline(&mut self, ctx: &eframe::egui::Context) {
        if let Some((key, rx)) = &self.outline_worker {
            match rx.try_recv() {
                Ok(outline) => {
                    if let Content::Text(text) = &self.content
                        && eframe::egui::util::hash((&self.path, text.as_str())) == *key
                    {
                        self.outline = outline;
                    }
                    self.outline_worker = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.outline_worker = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.outline_worker.is_none()
            && self
                .outline_refresh
                .is_some_and(|time| time.elapsed() >= Duration::from_millis(300))
        {
            self.outline_refresh = None;
            if let Content::Text(text) = &self.content {
                let key = eframe::egui::util::hash((&self.path, text.as_str()));
                let text = text.to_string();
                let path = self.path.clone();
                let ctx = ctx.clone();
                let (tx, rx) = mpsc::channel();
                self.outline_worker = Some((key, rx));
                let _ = std::thread::Builder::new()
                    .name("file-outline".into())
                    .spawn(move || {
                        let _ = tx.send(navigation::symbols(&path, &text));
                        ctx.request_repaint();
                    });
            }
        }
        if self.outline_refresh.is_some() || self.outline_worker.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// Note that the text changed, so cached positions are dropped.
    pub fn mark_changed(&mut self) {
        self.revision = next_revision();
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The one-based line and column of the cursor.
    ///
    /// The result is cached, because the status bar asks for it every frame
    /// and walking the document is expensive in a large file. A cursor that
    /// moved without the text changing is found by walking the short
    /// distance to it.
    pub fn cursor_line_column(&self, cursor: usize) -> (usize, usize) {
        let Content::Text(text) = &self.content else {
            return (1, 1);
        };
        if let Some((revision, cached, byte, line, line_start)) = self.cursor_position.get() {
            if revision == self.revision && cached == cursor {
                return (line, byte - line_start + 1);
            }
            if revision == self.revision
                && let Some((byte, line, line_start)) =
                    self.walk_to(text, cached, byte, line, line_start, cursor)
            {
                self.cursor_position
                    .set(Some((self.revision, cursor, byte, line, line_start)));
                return (line, byte - line_start + 1);
            }
        }
        let (byte, line, line_start) = self.scan_to(text, cursor);
        self.cursor_position
            .set(Some((self.revision, cursor, byte, line, line_start)));
        (line, byte - line_start + 1)
    }

    /// Walk from a known cursor position to another one.
    ///
    /// Only a short distance is walked, so a long move falls back to a scan
    /// of the document.
    fn walk_to(
        &self,
        text: &str,
        from: usize,
        from_byte: usize,
        from_line: usize,
        from_start: usize,
        to: usize,
    ) -> Option<(usize, usize, usize)> {
        let distance = to.checked_sub(from)?;
        if distance > 256 {
            return None;
        }
        let mut byte = from_byte;
        let mut line = from_line;
        let mut line_start = from_start;
        for (offset, character) in text[from_byte..].char_indices().take(distance) {
            byte = from_byte + offset + character.len_utf8();
            if character == '\n' {
                line += 1;
                line_start = byte;
            }
        }
        Some((byte, line, line_start))
    }

    /// The byte offset, the line and the start of the line of a cursor.
    fn scan_to(&self, text: &str, cursor: usize) -> (usize, usize, usize) {
        let mut byte = 0;
        let mut line = 1;
        let mut line_start = 0;
        for (index, (offset, character)) in text.char_indices().enumerate() {
            if index == cursor {
                byte = offset;
                break;
            }
            if character == '\n' {
                line += 1;
                line_start = offset + 1;
            }
        }
        (byte, line, line_start)
    }

    /// Whether the file on disk changed since it was opened or saved.
    ///
    /// The file is only read again when its size or modification time moved,
    /// so an idle editor does not re-read the same file twice a second. The
    /// hashes are kept as well, because the answer is asked every frame and
    /// hashing a large text every frame is wasteful.
    pub fn can_reload(&mut self) -> bool {
        if self.disk_checked.elapsed() >= Duration::from_millis(500) {
            self.check_disk();
        }
        let Some(disk) = self.disk_hash else {
            return false;
        };
        if !self.dirty {
            return self.original_hash() != disk;
        }
        self.buffer_hash() != disk
    }

    fn check_disk(&mut self) {
        self.disk_checked = Instant::now();
        let stamp = disk_stamp(&self.path);
        if stamp != self.disk_stamp {
            self.disk_stamp = stamp;
            self.disk_hash = navigation::read_text(&self.path)
                .ok()
                .map(|text| content_hash(&text));
        }
    }

    /// Navigation must see external changes even inside the idle polling interval.
    pub fn refresh_if_clean(&mut self, headers: bool) -> Result<bool> {
        if self.dirty {
            return Ok(false);
        }
        self.check_disk();
        self.reload_if_clean(headers)
    }

    /// Adopt external edits only while the editor buffer is clean. Failed loads
    /// retain the existing document and are retried when the disk changes again.
    pub fn reload_if_clean(&mut self, headers: bool) -> Result<bool> {
        if self.dirty || !self.can_reload() || self.auto_reload_stamp == self.disk_stamp {
            return Ok(false);
        }
        self.auto_reload_stamp = self.disk_stamp;
        self.reload(headers)?;
        Ok(true)
    }

    /// The hash of the text as it was loaded or saved.
    fn original_hash(&self) -> u64 {
        if let Some(hash) = self.original_hash.get() {
            return hash;
        }
        let hash = content_hash(&self.original);
        self.original_hash.set(Some(hash));
        hash
    }

    /// The hash of the buffer as it is now.
    ///
    /// The result is cached until the text changes, so the answer costs
    /// nothing on the frames where nothing was typed.
    fn buffer_hash(&self) -> u64 {
        if let Some((revision, hash)) = self.buffer_hash.get()
            && revision == self.revision
        {
            return hash;
        }
        let hash = match &self.content {
            Content::Text(text) => content_hash(text),
            Content::Csv(sheet) => match sheet.serialize() {
                Ok(text) => {
                    // The exact source is kept until the grid changes, so the
                    // file on disk is compared with the same bytes.
                    match self.csv.as_ref() {
                        Some(csv) if csv.grid_snapshot == text => content_hash(&csv.source),
                        _ => content_hash(&text),
                    }
                }
                Err(_) => 0,
            },
        };
        self.buffer_hash.set(Some((self.revision, hash)));
        hash
    }

    pub fn reload(&mut self, headers: bool) -> Result<()> {
        let mut fresh = Self::open(self.path.clone(), headers)?;
        fresh.set_csv_source(self.csv_is_source())?;
        fresh.markdown = self.markdown;
        fresh.cursor = self.cursor;
        fresh.editor_id = self.editor_id;
        *self = fresh;
        Ok(())
    }

    pub fn is_csv(&self) -> bool {
        self.csv.is_some()
    }

    pub fn csv_is_source(&self) -> bool {
        self.csv.is_some() && matches!(self.content, Content::Text(_))
    }

    /// Use the exact source until the grid has actually changed its field values.
    pub fn source_text(&self) -> Result<String> {
        match &self.content {
            Content::Text(text) => Ok(text.to_string()),
            Content::Csv(sheet) => {
                let text = sheet.serialize()?;
                match self.csv.as_ref() {
                    Some(csv) if csv.grid_snapshot == text => Ok(csv.source.clone()),
                    _ => Ok(text),
                }
            }
        }
    }

    pub fn set_csv_source(&mut self, source: bool) -> Result<()> {
        if !self.is_csv() || source == self.csv_is_source() {
            return Ok(());
        }
        if source {
            let text = self.source_text()?;
            self.content = Content::Text(text.into());
        } else if let Content::Text(text) = &self.content {
            let csv = self.csv.as_ref().unwrap();
            // Parse before replacing anything: malformed source remains fully editable.
            let sheet = Sheet::parse(text, csv.delimiter, csv.has_headers)?;
            let grid_snapshot = sheet.serialize()?;
            let csv = self.csv.as_mut().unwrap();
            csv.source = text.to_string();
            csv.grid_snapshot = grid_snapshot;
            self.content = Content::Csv(Box::new(sheet));
        }
        self.csv_error = None;
        self.dirty = self.source_text()?.as_str() != self.original.as_str();
        self.mark_changed();
        Ok(())
    }

    pub fn is_markdown(&self) -> bool {
        self.path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|s| matches!(s.to_lowercase().as_str(), "md" | "markdown"))
    }

    pub fn save(&mut self) -> Result<()> {
        let disk = navigation::read_text(&self.path)?;
        if disk.as_str() != self.original.as_str() {
            bail!(
                "The file changed on disk. Save was stopped to protect those changes. Open the disk version separately or copy your edits before reloading."
            );
        }
        let text = self.source_text()?;
        atomic_write(&self.path, text.as_bytes())?;
        self.disk_stamp = disk_stamp(&self.path);
        self.disk_hash = Some(content_hash(&text));
        self.disk_checked = Instant::now();
        self.original_hash.set(Some(content_hash(&text)));
        self.original = match &self.content {
            Content::Text(buffer) => buffer.clone(),
            Content::Csv(_) => text.into(),
        };
        self.dirty = false;
        Ok(())
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))?;
    temp.as_file()
        .set_permissions(std::fs::metadata(path)?.permissions())?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saved_text_is_shared_until_an_actual_edit() -> Result<()> {
        use eframe::egui::TextBuffer;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("shared.txt");
        std::fs::write(&path, "猫 hello")?;
        let mut doc = Document::open(path.clone(), true)?;
        let Content::Text(text) = &mut doc.content else {
            unreachable!()
        };
        assert!(Arc::ptr_eq(&text.0, &doc.original.0));
        let ctx = eframe::egui::Context::default();
        for _ in 0..3 {
            let _ = ctx.run(Default::default(), |ctx| {
                eframe::egui::CentralPanel::default().show(ctx, |ui| {
                    ui.add(eframe::egui::TextEdit::multiline(text));
                });
            });
            assert!(
                Arc::ptr_eq(&text.0, &doc.original.0),
                "idle UI retains shared storage"
            );
        }
        text.insert_text("", 0);
        text.delete_char_range(0..0);
        assert!(Arc::ptr_eq(&text.0, &doc.original.0));
        text.insert_text("!", 1);
        assert_eq!(text.as_str(), "猫! hello");
        assert_eq!(doc.original.as_str(), "猫 hello");
        assert!(!Arc::ptr_eq(&text.0, &doc.original.0));
        text.delete_char_range(0..1);
        assert_eq!(text.as_str(), "! hello");
        doc.save()?;
        let Content::Text(text) = &doc.content else {
            unreachable!()
        };
        assert!(
            Arc::ptr_eq(&text.0, &doc.original.0),
            "saving shares the new baseline"
        );
        assert_eq!(std::fs::read_to_string(path)?, "! hello");
        Ok(())
    }

    #[test]
    fn reopening_and_reloading_have_distinct_revisions() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "first")?;
        let mut doc = Document::open(path.clone(), true)?;
        let initial = doc.revision();
        assert_ne!(Document::open(path.clone(), true)?.revision(), initial);
        std::fs::write(&path, "second")?;
        doc.reload(true)?;
        assert_ne!(doc.revision(), initial);
        Ok(())
    }

    #[test]
    fn the_status_bar_reports_the_line_ending_of_the_file() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let crlf = dir.path().join("crlf.txt");
        std::fs::write(&crlf, "one\r\ntwo\r\n")?;
        assert_eq!(Document::open(crlf, true)?.line_ending, "CRLF");
        let lf = dir.path().join("lf.txt");
        std::fs::write(&lf, "one\ntwo\n")?;
        assert_eq!(Document::open(lf, true)?.line_ending, "LF");
        Ok(())
    }

    #[test]
    fn the_cursor_position_follows_the_text_and_its_changes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("lines.txt");
        std::fs::write(&path, "one\ntwo\nthree\n")?;
        let mut doc = Document::open(path, true)?;
        // The cursor sits in front of "three".
        let cursor = "one\ntwo\n".chars().count();
        assert_eq!(doc.cursor_line_column(cursor), (3, 1));
        // The same answer comes from the cache.
        assert_eq!(doc.cursor_line_column(cursor), (3, 1));
        assert_eq!(doc.cursor_line_column(0), (1, 1));
        // A change in front of the cursor moves it.
        if let Content::Text(text) = &mut doc.content {
            text.insert_str(0, "zero\n");
        }
        doc.mark_changed();
        // The cursor index still points at the same offset in the text.
        assert_eq!(doc.cursor_line_column(cursor), (2, 4));
        // When the cursor moves along with the text, the line follows.
        assert_eq!(doc.cursor_line_column(cursor + 5), (4, 1));
        Ok(())
    }

    #[test]
    fn the_file_on_disk_is_only_read_again_when_it_changed() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("watched.txt");
        std::fs::write(&path, "one\n")?;
        let mut doc = Document::open(path.clone(), true)?;
        assert!(!doc.can_reload());
        std::fs::write(&path, "two\n")?;
        // The file is polled at most twice a second.
        std::thread::sleep(Duration::from_millis(600));
        assert!(doc.can_reload());
        Ok(())
    }

    #[test]
    fn large_file_open_defers_outline_and_discards_results_for_edited_buffers() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("large.ts");
        std::fs::write(&path, "export const value = 1;\n".repeat(10_001))?;
        let mut doc = Document::open(path, true)?;
        assert!(doc.outline.is_empty());
        assert!(doc.outline_refresh.is_some());
        let old_key = match &doc.content {
            Content::Text(text) => eframe::egui::util::hash((&doc.path, text.as_str())),
            _ => unreachable!(),
        };
        let (tx, rx) = mpsc::channel();
        tx.send(navigation::symbols(&doc.path, "export const obsolete = 1;"))?;
        doc.outline_worker = Some((old_key, rx));
        doc.content = Content::Text("export const current = 2;".into());
        doc.poll_outline(&eframe::egui::Context::default());
        assert!(
            doc.outline.is_empty(),
            "the old buffer's symbols must not be shown"
        );
        Ok(())
    }

    #[test]
    fn csv_modes_preserve_exact_source_and_sync_edits_in_both_directions() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("modes.csv");
        let original = "name,note\r\n\"Alice\",\"hello, world\"\r\n";
        std::fs::write(&path, original)?;
        let mut doc = Document::open(path.clone(), true)?;
        doc.set_csv_source(true)?;
        assert!(matches!(&doc.content, Content::Text(text) if text.as_str() == original));
        doc.set_csv_source(false)?;
        assert!(!doc.dirty);
        doc.save()?;
        assert_eq!(std::fs::read_to_string(&path)?, original);
        if let Content::Csv(sheet) = &mut doc.content {
            sheet.table[0][0] = "Bob".into();
        }
        doc.set_csv_source(true)?;
        assert!(matches!(&doc.content, Content::Text(text) if text.contains("Bob")));
        assert!(doc.dirty);
        let edited = "name,note\nCara,\"line one\nline two\"\n";
        doc.content = Content::Text(edited.into());
        doc.set_csv_source(false)?;
        assert!(
            matches!(&doc.content, Content::Csv(sheet) if sheet.table[0] == ["Cara", "line one\nline two"])
        );
        doc.save()?;
        assert_eq!(std::fs::read_to_string(&path)?, edited);
        doc.set_csv_source(true)?;
        assert!(matches!(&doc.content, Content::Text(text) if text.as_str() == edited));
        assert!(!doc.dirty);
        Ok(())
    }

    #[test]
    fn invalid_csv_source_stays_editable_and_tsv_keeps_headerless_import() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("modes.tsv");
        std::fs::write(&path, "Alice\t10\nBob\t2\n")?;
        let mut doc = Document::open(path.clone(), false)?;
        doc.set_csv_source(true)?;
        doc.content = Content::Text("Alice\t10\nBob\t2\textra\n".into());
        assert!(doc.set_csv_source(false).is_err());
        assert!(matches!(&doc.content, Content::Text(text) if text.contains("extra")));
        doc.content = Content::Text("Cara\t30\nBob\t2\n".into());
        doc.set_csv_source(false)?;
        assert!(
            matches!(&doc.content, Content::Csv(sheet) if sheet.table.len() == 2 && sheet.table[0][0] == "Cara")
        );
        doc.save()?;
        assert_eq!(std::fs::read_to_string(&path)?, "Cara\t30\nBob\t2\n");
        doc.set_csv_source(true)?;
        doc.reload(false)?;
        assert!(doc.csv_is_source());
        assert!(!doc.dirty);
        Ok(())
    }

    #[test]
    fn automatic_reload_updates_clean_text_and_preserves_position() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("auto.md");
        std::fs::write(&path, "old\n")?;
        let mut doc = Document::open(path.clone(), true)?;
        doc.cursor = 2;
        doc.markdown = MarkdownMode::Preview;
        doc.editor_id = Some(eframe::egui::Id::new("editor"));
        let revision = doc.revision();
        std::fs::write(&path, "updated from outside\n")?;
        doc.disk_checked = Instant::now() - Duration::from_secs(1);
        assert!(doc.reload_if_clean(true)?);
        assert_eq!(doc.source_text()?, "updated from outside\n");
        assert_eq!(doc.cursor, 2);
        assert!(matches!(doc.markdown, MarkdownMode::Preview));
        assert_eq!(doc.editor_id, Some(eframe::egui::Id::new("editor")));
        assert_ne!(doc.revision(), revision);
        assert!(!doc.dirty);
        assert!(!doc.reload_if_clean(true)?);
        Ok(())
    }

    #[test]
    fn automatic_reload_preserves_unsaved_edits_and_missing_files() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("auto.rs");
        std::fs::write(&path, "original")?;
        let mut doc = Document::open(path.clone(), true)?;
        doc.content = Content::Text("my unsaved edits".into());
        doc.mark_changed();
        doc.dirty = true;
        std::fs::write(&path, "external changes")?;
        doc.disk_checked = Instant::now() - Duration::from_secs(1);
        assert!(!doc.reload_if_clean(true)?);
        assert_eq!(doc.source_text()?, "my unsaved edits");
        assert_eq!(std::fs::read_to_string(&path)?, "external changes");
        doc.reload(true)?;
        std::fs::remove_file(path)?;
        doc.disk_checked = Instant::now() - Duration::from_secs(1);
        assert!(!doc.reload_if_clean(true)?);
        assert_eq!(doc.source_text()?, "external changes");
        Ok(())
    }

    #[test]
    fn automatic_csv_reload_keeps_source_mode_and_recovers_from_invalid_disk_data() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("auto.csv");
        std::fs::write(&path, "name,value\nAlice,1\n")?;
        let mut doc = Document::open(path.clone(), true)?;
        doc.set_csv_source(true)?;
        std::fs::write(&path, "name,value\nBob,2\n")?;
        doc.disk_checked = Instant::now() - Duration::from_secs(1);
        assert!(doc.reload_if_clean(true)?);
        assert!(doc.csv_is_source());
        assert_eq!(doc.source_text()?, "name,value\nBob,2\n");
        std::fs::write(&path, "name,value\ninvalid,row,extra\n")?;
        doc.disk_checked = Instant::now() - Duration::from_secs(1);
        assert!(doc.reload_if_clean(true).is_err());
        assert_eq!(doc.source_text()?, "name,value\nBob,2\n");
        assert!(
            !doc.reload_if_clean(true)?,
            "do not retry the same bad disk revision every frame"
        );
        std::fs::write(&path, "name,value\nCarol,30\n")?;
        doc.disk_checked = Instant::now() - Duration::from_secs(1);
        assert!(doc.reload_if_clean(true)?);
        assert_eq!(doc.source_text()?, "name,value\nCarol,30\n");
        assert!(doc.csv_is_source());
        Ok(())
    }

    #[test]
    fn reload_tracks_disk_edits_and_resets_dirty_buffer() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn old() {}\n")?;
        let mut doc = Document::open(path.clone(), true)?;
        assert!(!doc.can_reload());
        doc.content = Content::Text("unsaved".into());
        doc.dirty = true;
        assert!(doc.can_reload());
        std::fs::write(&path, "fn fresh() {}\n")?;
        doc.disk_checked = Instant::now() - Duration::from_secs(1);
        assert!(doc.can_reload());
        doc.reload(true)?;
        assert!(!doc.dirty);
        assert!(!doc.can_reload());
        assert!(matches!(&doc.content, Content::Text(text) if text.as_str() == "fn fresh() {}\n"));
        assert_eq!(doc.outline[0].name, "fresh");
        doc.save()?;
        Ok(())
    }

    #[test]
    fn unchanged_csv_does_not_offer_reload_for_serialization_differences() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("a.csv");
        std::fs::write(&path, "x,y\r\n\"1\",2\r\n")?;
        let mut doc = Document::open(path, true)?;
        assert!(!doc.can_reload());
        if let Content::Csv(sheet) = &mut doc.content {
            sheet.table[0][1] = "edited".into();
        }
        doc.dirty = true;
        assert!(doc.can_reload());
        doc.reload(true)?;
        assert!(!doc.can_reload());
        Ok(())
    }

    #[test]
    fn a_moving_cursor_is_found_without_walking_the_whole_document() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("lines.txt");
        std::fs::write(&path, "one\ntwo\nthree\n")?;
        let doc = Document::open(path, true)?;
        // The cursor sits in front of "three".
        let cursor = "one\ntwo\n".chars().count();
        assert_eq!(doc.cursor_line_column(cursor), (3, 1));
        // Moving the cursor forward keeps the answer.
        assert_eq!(doc.cursor_line_column(cursor + 2), (3, 3));
        // Moving it back as well.
        assert_eq!(doc.cursor_line_column(cursor - 1), (2, 4));
        // A long move is found as well.
        assert_eq!(doc.cursor_line_column(0), (1, 1));
        Ok(())
    }

    #[test]
    fn the_cached_buffer_hash_follows_every_edit() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("watched.txt");
        std::fs::write(&path, "one\n")?;
        let mut doc = Document::open(path, true)?;
        assert!(!doc.can_reload());
        // The answer is asked again on the same revision and stays the same.
        assert!(!doc.can_reload());
        if let Content::Text(text) = &mut doc.content {
            text.push_str("two\n");
        }
        doc.mark_changed();
        doc.dirty = true;
        assert!(doc.can_reload(), "the buffer no longer matches the disk");
        if let Content::Text(text) = &mut doc.content {
            text.truncate(4);
        }
        doc.mark_changed();
        assert!(!doc.can_reload(), "the buffer matches the disk again");
        Ok(())
    }

    #[test]
    fn save_preserves_external_edits() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "original")?;
        let mut doc = Document::open(path.clone(), true)?;
        doc.content = Content::Text("ours".into());
        std::fs::write(&path, "theirs")?;
        assert!(doc.save().is_err());
        assert_eq!(std::fs::read_to_string(&path)?, "theirs");
        Ok(())
    }
    #[test]
    fn saving_csv_keeps_quoted_fields() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("a.csv");
        std::fs::write(&path, "x,y\n1,2\n")?;
        let mut doc = Document::open(path.clone(), true)?;
        if let Content::Csv(sheet) = &mut doc.content {
            sheet.table[0][1] = "a,b\nc".into();
        }
        doc.save()?;
        let restored = Document::open(path, true)?;
        if let Content::Csv(sheet) = restored.content {
            assert_eq!(sheet.table[0][1], "a,b\nc");
        }
        Ok(())
    }
}
