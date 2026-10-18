//! Folding changes row geometry only: the editor retains the complete source buffer.
use crate::editor_structure::Structure;
use eframe::egui::{self, Galley, Rect};
use std::{collections::BTreeSet, sync::Arc};

#[derive(Clone, Debug)]
pub struct Region {
    pub start: usize,
    pub end: usize,
    pub hidden_start: usize,
    pub first: usize,
    pub last: usize,
    pub preview: String,
}
#[derive(Clone, Default)]
pub struct State {
    fingerprint: u64,
    pub collapsed: BTreeSet<usize>,
}
impl State {
    pub fn sync(&mut self, text: &str) {
        let fingerprint = egui::util::hash(text);
        if fingerprint != self.fingerprint {
            self.collapsed.clear();
            self.fingerprint = fingerprint;
        }
    }
    pub fn reveal(&mut self, regions: &[Region], range: std::ops::RangeInclusive<usize>) -> bool {
        let before = self.collapsed.len();
        self.collapsed.retain(|start| {
            !regions.iter().any(|r| {
                r.start == *start && *range.end() >= r.hidden_start && *range.start() <= r.end
            })
        });
        before != self.collapsed.len()
    }
    pub fn active<'a>(&self, regions: &'a [Region]) -> Vec<&'a Region> {
        let mut result: Vec<&Region> = vec![];
        for region in regions {
            if self.collapsed.contains(&region.start)
                && !result
                    .last()
                    .is_some_and(|parent| region.first <= parent.last)
            {
                result.push(region);
            }
        }
        result
    }
}

#[derive(Default)]
pub struct Computer;
impl egui::cache::ComputerMut<(&egui::text::LayoutJob, &str), Vec<Region>> for Computer {
    fn compute(&mut self, (job, extension): (&egui::text::LayoutJob, &str)) -> Vec<Region> {
        regions(&job.text, &Structure::build(job), extension)
    }
}

pub fn regions(text: &str, structure: &Structure, extension: &str) -> Vec<Region> {
    let chars: Vec<_> = text.chars().collect();
    let mut lines = vec![0];
    for (index, ch) in chars.iter().enumerate() {
        if *ch == '\n' {
            lines.push(index + 1);
        }
    }
    let line_at = |offset| {
        lines
            .partition_point(|start| *start <= offset)
            .saturating_sub(1)
    };
    let mut ranges: Vec<_> = structure.pairs.clone();
    if matches!(extension, "py" | "yaml" | "yml") {
        for guide in &structure.guides {
            let first = guide.start;
            let indent = |line: usize| {
                chars[lines[line]..]
                    .iter()
                    .take_while(|c| **c == ' ' || **c == '\t')
                    .count()
            };
            let mut last = if indent(guide.end) > indent(first) {
                guide.end
            } else {
                guide.end.saturating_sub(1)
            };
            while last > first
                && chars[lines[last]..lines.get(last + 1).copied().unwrap_or(chars.len())]
                    .iter()
                    .all(|c| c.is_whitespace())
            {
                last -= 1;
            }
            if last > first {
                let start = lines[first]
                    + chars[lines[first]..]
                        .iter()
                        .take_while(|c| **c != '\n')
                        .count();
                let end = lines
                    .get(last + 1)
                    .copied()
                    .unwrap_or(chars.len())
                    .saturating_sub(1);
                ranges.push((start.saturating_sub(1), end));
            }
        }
    }
    let mut result = vec![];
    for (start, end) in ranges {
        let first = line_at(start);
        let last = line_at(end);
        if last <= first {
            continue;
        }
        let mut sample = String::new();
        let mut whitespace = false;
        for ch in chars[start + 1..end].iter().copied() {
            if ch.is_whitespace() {
                whitespace = !sample.is_empty();
                continue;
            }
            if whitespace {
                sample.push(' ');
                whitespace = false;
            }
            sample.push(ch);
            if sample.chars().count() >= 56 {
                break;
            }
        }
        let suffix: String = if matches!(chars[end], '}' | ']' | ')') {
            chars[end..]
                .iter()
                .take_while(|c| **c != '\n')
                .take(40)
                .collect()
        } else {
            String::new()
        };
        result.push(Region {
            start,
            end,
            hidden_start: lines[first + 1],
            first,
            last,
            preview: format!(" {sample} …{suffix}  ({} lines) ", last - first),
        });
    }
    result.sort_by_key(|r| (r.first, std::cmp::Reverse(r.last)));
    result.dedup_by_key(|r| r.first);
    result
}

pub fn layout(original: Arc<Galley>, active: &[&Region]) -> Arc<Galley> {
    if active.is_empty() {
        return original;
    }
    let mut galley = (*original).clone();
    let mut removed = 0.0;
    let mut region = 0;
    galley.mesh_bounds = Rect::NOTHING;
    galley.num_vertices = 0;
    galley.num_indices = 0;
    for (index, row) in galley.rows.iter_mut().enumerate() {
        while region < active.len() && index > active[region].last {
            region += 1;
        }
        row.pos.y -= removed;
        if active
            .get(region)
            .is_some_and(|r| index > r.first && index <= r.last)
        {
            removed += row.size.y;
            let row = Arc::make_mut(&mut row.row);
            row.size.y = 0.0;
            row.visuals = Default::default();
        } else {
            galley.mesh_bounds |= row.visuals.mesh_bounds.translate(row.pos.to_vec2());
            galley.num_vertices += row.visuals.mesh.vertices.len();
            galley.num_indices += row.visuals.mesh.indices.len();
        }
    }
    galley.rect.max.y -= removed;
    for r in active {
        if let Some(row) = galley.rows.get(r.first) {
            galley.rect.max.x = galley.rect.max.x.max(
                row.size.x
                    + r.preview.chars().count() as f32
                        * galley
                            .job
                            .sections
                            .first()
                            .map_or(8.0, |section| section.format.font_id.size * 8.0 / 13.0),
            );
        }
    }
    Arc::new(galley)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_regions_ignore_strings_and_reveal_parent_and_child() {
        let source =
            "{\n  \"猫\": {\n    \"text\": \"[ignore]\"\n  },\n  \"list\": [\n    1\n  ]\n}\n";
        let job = crate::highlighting::background(source, "json", true, &|| false).unwrap();
        let regions = regions(source, &Structure::build(&job), "json");
        assert_eq!(
            regions
                .iter()
                .map(|r| (r.first, r.last))
                .collect::<Vec<_>>(),
            vec![(0, 7), (1, 3), (4, 6)]
        );
        let mut state = State::default();
        state.sync(source);
        state.collapsed.extend(regions.iter().map(|r| r.start));
        assert_eq!(state.active(&regions).len(), 1);
        assert!(state.reveal(&regions, regions[1].end..=regions[1].end));
        assert_eq!(state.active(&regions).len(), 1);
        assert_eq!(state.active(&regions)[0].first, 4);
    }
    #[test]
    fn indentation_folds_include_the_last_body_line_and_exclude_dedents() {
        for source in [
            "def run():\n    first()\n    second()",
            "def run():\n    first()\n    second()\n\nnext()\n",
        ] {
            let job = crate::highlighting::background(source, "py", true, &|| false).unwrap();
            let regions = regions(source, &Structure::build(&job), "py");
            assert_eq!((regions[0].first, regions[0].last), (0, 2));
        }
    }
    #[test]
    fn braces_fold_across_supported_languages_but_comment_braces_do_not() {
        for (extension, source) in [
            (
                "java",
                "class App {\n  void run() {\n    // } ignored\n  }\n}\n",
            ),
            (
                "kt",
                "class App {\n  fun run() {\n    val text = \"}\"\n  }\n}\n",
            ),
            ("go", "package main\nfunc run() {\n  // } ignored\n}\n"),
            ("ts", "function run() {\n  const text = \"}\";\n}\n"),
        ] {
            let job = crate::highlighting::background(source, extension, true, &|| false).unwrap();
            let regions = regions(source, &Structure::build(&job), extension);
            assert!(!regions.is_empty(), "{extension}");
            assert_eq!(regions[0].last, source.lines().count() - 1, "{extension}");
        }
    }

    #[test]
    fn folding_preserves_text_and_character_offsets() {
        let ctx = egui::Context::default();
        let source = "{\n  \"猫\": [\n    1\n  ]\n}\nnext";
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let job = crate::highlighting::highlight(ui, source, "json");
                let regions = regions(source, &Structure::build(&job), "json");
                let original = ui.fonts_mut(|f| f.layout_job(job));
                let folded = layout(original.clone(), &[&regions[0]]);
                assert_eq!(original.job.text, folded.job.text);
                assert_eq!(original.end().index, folded.end().index);
                assert!(folded.rect.height() < original.rect.height());
                assert_eq!(folded.rows[1].size.y, 0.0);
                let offset = source.chars().count() - 4;
                let pos = folded.pos_from_cursor(egui::text::CCursor::new(offset));
                assert_eq!(
                    folded.cursor_from_pos(pos.center().to_vec2()).index,
                    offset,
                    "pos={pos:?}, rows={:?}",
                    folded
                        .rows
                        .iter()
                        .map(|r| (r.pos, r.size))
                        .collect::<Vec<_>>()
                );
            });
        });
    }
}
