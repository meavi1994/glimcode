//! Bounded line comparison. Large unrelated regions fall back to delete/insert blocks.

#[derive(Debug, PartialEq)]
pub struct Row {
    pub disk: Option<usize>,
    pub editor: Option<usize>,
    pub changed: bool,
}

pub fn compare(disk: &str, editor: &str) -> Vec<Row> {
    let a: Vec<_> = disk.split_inclusive('\n').collect();
    let b: Vec<_> = editor.split_inclusive('\n').collect();
    let prefix = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (n, m) = (a.len() - prefix - suffix, b.len() - prefix - suffix);
    let mut rows: Vec<_> = (0..prefix)
        .map(|i| Row {
            disk: Some(i),
            editor: Some(i),
            changed: false,
        })
        .collect();
    if (n + 1).saturating_mul(m + 1) <= 1_000_000 {
        let mut lcs = vec![0u32; (n + 1) * (m + 1)];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i * (m + 1) + j] = if a[prefix + i] == b[prefix + j] {
                    1 + lcs[(i + 1) * (m + 1) + j + 1]
                } else {
                    lcs[(i + 1) * (m + 1) + j].max(lcs[i * (m + 1) + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && a[prefix + i] == b[prefix + j] {
                rows.push(Row {
                    disk: Some(prefix + i),
                    editor: Some(prefix + j),
                    changed: false,
                });
                i += 1;
                j += 1;
            } else if i < n && (j == m || lcs[(i + 1) * (m + 1) + j] >= lcs[i * (m + 1) + j + 1]) {
                rows.push(Row {
                    disk: Some(prefix + i),
                    editor: None,
                    changed: true,
                });
                i += 1;
            } else {
                rows.push(Row {
                    disk: None,
                    editor: Some(prefix + j),
                    changed: true,
                });
                j += 1;
            }
        }
    } else {
        rows.extend((prefix..prefix + n).map(|i| Row {
            disk: Some(i),
            editor: None,
            changed: true,
        }));
        rows.extend((prefix..prefix + m).map(|i| Row {
            disk: None,
            editor: Some(i),
            changed: true,
        }));
    }
    rows.extend((0..suffix).map(|i| Row {
        disk: Some(a.len() - suffix + i),
        editor: Some(b.len() - suffix + i),
        changed: false,
    }));
    rows
}

/// First changed row in each hunk with three lines of surrounding context.
pub fn hunk_targets(rows: &[Row]) -> Vec<usize> {
    let mut targets = Vec::new();
    let mut previous = None;
    for (index, _) in rows.iter().enumerate().filter(|(_, row)| row.changed) {
        if previous.is_none_or(|last| index > last + 7) {
            targets.push(index);
        }
        previous = Some(index);
    }
    targets
}

pub fn change_lines(base: &str, current: &str) -> Vec<usize> {
    let rows = compare(base, current);
    let targets = hunk_targets(&rows);
    let last_line = current.split('\n').count().saturating_sub(1);
    targets
        .into_iter()
        .map(|index| {
            // Deletions anchor to the next surviving line, including the empty EOF line.
            rows[index..]
                .iter()
                .find_map(|row| row.editor)
                .unwrap_or(last_line)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hunk_lines_group_context_and_anchor_deletions_in_the_editor() {
        let before = (0..30).map(|i| format!("line {i}\n")).collect::<String>();
        let after = before
            .replace("line 2\n", "猫\n")
            .replace("line 20\n", "new\n");
        assert_eq!(change_lines(&before, &after), vec![2, 20]);
        let nearby = before
            .replace("line 2\n", "猫\n")
            .replace("line 6\n", "new\n");
        assert_eq!(change_lines(&before, &nearby), vec![2]);
        assert_eq!(change_lines("a\nb\nc\n", "a\nc\n"), vec![1]);
        assert_eq!(change_lines("a\nb", "a"), vec![0]);
        assert_eq!(change_lines("a\nb\n", "a\n"), vec![1]);
        assert_eq!(change_lines("a", ""), vec![0]);
        assert_eq!(change_lines("", "猫\n"), vec![0]);
        assert!(change_lines(&before, &before).is_empty());
    }

    #[test]
    fn changes_align_and_preserve_unicode_and_newlines() {
        let rows = compare("α\nb\nc\n", "α\nnew\nb\nc");
        assert_eq!(rows.iter().filter(|r| !r.changed).count(), 2);
        assert_eq!(rows.iter().filter(|r| r.disk.is_some()).count(), 3);
        assert_eq!(rows.iter().filter(|r| r.editor.is_some()).count(), 4);
        assert!(compare("", "").is_empty());
        assert!(compare("same\n", "same\n").iter().all(|r| !r.changed));
    }
    #[test]
    fn large_replacements_are_bounded() {
        let rows = compare(&"old\n".repeat(2000), &"new\n".repeat(2000));
        assert_eq!(rows.len(), 4000);
    }
}
