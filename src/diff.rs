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

#[cfg(test)]
mod tests {
    use super::*;
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
