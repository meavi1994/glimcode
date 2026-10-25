use eframe::egui::{self, text::LayoutJob};

#[derive(Clone, Default)]
pub struct Structure {
    pub pairs: Vec<(usize, usize)>,
    pub guides: Vec<Guide>,
}
#[derive(Clone)]
pub struct Guide {
    pub start: usize,
    pub end: usize,
    pub anchor: usize,
    pub inclusive_end: bool,
}
#[derive(Default)]
pub struct Computer;
impl egui::cache::ComputerMut<&LayoutJob, Structure> for Computer {
    fn compute(&mut self, job: &LayoutJob) -> Structure {
        Structure::build(job)
    }
}
impl Structure {
    pub fn build(job: &LayoutJob) -> Self {
        let mut result = Self::default();
        let mut stack = Vec::new();
        let mut section = 0;
        for (index, (byte, ch)) in job.text.char_indices().enumerate() {
            while section < job.sections.len() && job.sections[section].byte_range.end <= byte {
                section += 1;
            }
            let ignored = job.sections.get(section).is_some_and(|s| {
                let c = s.format.color;
                s.format.italics
                    || [
                        0x6AAB73, 0x008000, 0x7A7E85, 0x808080, 0xCC7832, 0x000080, 0xC77DBB,
                        0x660E7A,
                    ]
                    .iter()
                    .any(|hex| {
                        c == egui::Color32::from_rgb(
                            (hex >> 16) as u8,
                            (hex >> 8) as u8,
                            *hex as u8,
                        )
                    })
            });
            if ignored {
                continue;
            }
            match ch {
                '{' | '[' | '(' => stack.push((ch, index)),
                '}' | ']' | ')' => {
                    if let Some((open, start)) = stack.pop() {
                        if matches!((open, ch), ('{', '}') | ('[', ']') | ('(', ')')) {
                            result.pairs.push((start, index));
                        } else {
                            stack.clear();
                        }
                    }
                }
                _ => {}
            }
        }
        // Derive continuous indentation guides, carrying scopes across blank lines.
        let mut scopes: Vec<(usize, usize, usize)> = vec![];
        let mut previous = (0, 0, 0);
        let mut offset = 0;
        let mut last_line = 0;
        for (line, content) in job.text.split('\n').enumerate() {
            last_line = line;
            let indent = content
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .count();
            if !content.trim().is_empty() {
                while scopes
                    .last()
                    .is_some_and(|(column, _, _)| *column >= indent)
                {
                    let (_, start, anchor) = scopes.pop().unwrap();
                    result.guides.push(Guide {
                        start,
                        end: line,
                        anchor,
                        inclusive_end: false,
                    });
                }
                if indent > previous.0 {
                    scopes.push((previous.0, previous.1, previous.2));
                }
                previous = (indent, line, offset + indent);
            }
            offset += content.chars().count() + 1;
        }
        for (_, start, anchor) in scopes {
            result.guides.push(Guide {
                start,
                end: last_line,
                anchor,
                inclusive_end: true,
            });
        }
        result
    }
    /// Highlight only the innermost scope containing the caret line.
    /// A dedent belongs to its parent scope, so the end line is exclusive.
    pub fn active_guide(&self, line: usize) -> Option<&Guide> {
        self.guides
            .iter()
            .filter(|guide| {
                guide.start <= line
                    && (line < guide.end || guide.inclusive_end && line == guide.end)
            })
            .min_by_key(|guide| (guide.end - guide.start, std::cmp::Reverse(guide.start)))
    }

    pub fn matching(&self, caret: usize) -> Option<(usize, usize)> {
        self.pairs
            .iter()
            .copied()
            .find(|(a, b)| *a == caret || *b == caret)
            .or_else(|| {
                caret.checked_sub(1).and_then(|previous| {
                    self.pairs
                        .iter()
                        .copied()
                        .find(|(a, b)| *a == previous || *b == previous)
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pairs_ignore_strings_comments_and_unicode_uses_character_offsets() {
        let source =
            "fn main() {\n    let 猫 = [call(1)]; // } ]\n    let s = \"(ignore { [\";\n\n}\n";
        let job = crate::highlighting::background(source, "rs", true, &|| false).unwrap();
        let structure = Structure::build(&job);
        let brace = source[..source.find('{').unwrap()].chars().count();
        let end = source[..source.rfind('}').unwrap()].chars().count();
        assert_eq!(structure.matching(brace), Some((brace, end)));
        assert_eq!(structure.matching(end + 1), Some((brace, end)));
        assert_eq!(structure.pairs.len(), 4);
        assert!(structure.guides.iter().any(|g| g.start == 0 && g.end == 4));
    }
    #[test]
    fn active_guide_tracks_nested_scopes_blank_lines_and_dedents() {
        let source = "fn main() {\n    if ready {\n        run();\n\n    }\n    finish();\n}\n";
        let job = crate::highlighting::background(source, "rs", true, &|| false).unwrap();
        let structure = Structure::build(&job);
        assert_eq!(structure.active_guide(0).map(|g| g.start), Some(0));
        assert_eq!(structure.active_guide(2).map(|g| g.start), Some(1));
        assert_eq!(structure.active_guide(3).map(|g| g.start), Some(1));
        assert_eq!(structure.active_guide(4).map(|g| g.start), Some(0));
        assert_eq!(structure.active_guide(5).map(|g| g.start), Some(0));
        assert!(structure.active_guide(6).is_none());
    }
    #[test]
    fn active_guide_includes_the_final_indented_line_without_a_newline() {
        let source = "def run():\n    return 1";
        let job = crate::highlighting::background(source, "py", true, &|| false).unwrap();
        let structure = Structure::build(&job);
        assert_eq!(structure.active_guide(1).map(|guide| guide.start), Some(0));
        assert!(structure.active_guide(2).is_none());
    }
}
