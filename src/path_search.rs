//! Immutable substring index. Queries inspect candidate blocks, not the workspace.
use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};
const BLOCK: usize = 64;
struct Name {
    text: String,
    name: usize,
    stem: usize,
}
impl Name {
    fn name(&self) -> &str {
        &self.text[self.name..]
    }
    fn stem(&self) -> &str {
        &self.text[self.name..self.stem]
    }
}
#[derive(Default)]
struct Grams {
    postings: HashMap<[u8; 3], Vec<u32>>,
    short: Vec<[u64; 4]>,
}
fn short_bit(bytes: &[u8]) -> usize {
    bytes.iter().fold(17usize, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(*byte as usize)
    }) % 256
}
impl Grams {
    fn add(&mut self, block: usize, strings: impl Iterator<Item = impl AsRef<str>>) {
        let mut keys = HashSet::new();
        let mut short = [0; 4];
        for text in strings {
            let bytes = text.as_ref().as_bytes();
            for len in 1..=2 {
                for gram in bytes.windows(len) {
                    let bit = short_bit(gram);
                    short[bit / 64] |= 1 << (bit % 64);
                }
            }
            for gram in bytes.windows(3) {
                keys.insert([gram[0], gram[1], gram[2]]);
            }
        }
        self.short.push(short);
        for gram in keys {
            self.postings.entry(gram).or_default().push(block as u32);
        }
    }
    fn blocks(&self, query: &str) -> Vec<u32> {
        if query.len() < 3 {
            let bit = short_bit(query.as_bytes());
            return self
                .short
                .iter()
                .enumerate()
                .filter_map(|(i, bits)| {
                    (bits[bit / 64] & (1 << (bit % 64)) != 0).then_some(i as u32)
                })
                .collect();
        }
        let mut lists = Vec::new();
        for gram in query.as_bytes().windows(3) {
            let Some(list) = self.postings.get(&[gram[0], gram[1], gram[2]]) else {
                return Vec::new();
            };
            lists.push(list);
        }
        lists.sort_unstable_by_key(|list| list.len());
        lists[0]
            .iter()
            .copied()
            .filter(|block| {
                lists[1..]
                    .iter()
                    .all(|list| list.binary_search(block).is_ok())
            })
            .collect()
    }
}
struct Order {
    ids: Vec<u32>,
    minimum: Vec<u32>,
    leaves: usize,
}
impl Order {
    fn new(mut ids: Vec<u32>, compare: impl Fn(&u32, &u32) -> std::cmp::Ordering) -> Self {
        ids.sort_unstable_by(compare);
        let leaves = ids.len().div_ceil(BLOCK).max(1).next_power_of_two();
        let mut minimum = vec![u32::MAX; leaves * 2];
        for (block, slice) in ids.chunks(BLOCK).enumerate() {
            minimum[leaves + block] = *slice.iter().min().unwrap();
        }
        for node in (1..leaves).rev() {
            minimum[node] = minimum[node * 2].min(minimum[node * 2 + 1]);
        }
        Self {
            ids,
            minimum,
            leaves,
        }
    }
    fn first(&self, range: std::ops::Range<usize>, accept: impl Fn(u32) -> bool) -> Vec<u32> {
        if range.is_empty() {
            return Vec::new();
        }
        let mut pending = BinaryHeap::from([Reverse((self.minimum[1], 1, 0, self.leaves))]);
        let mut best = BinaryHeap::new();
        while let Some(Reverse((smallest, node, start, end))) = pending.pop() {
            if start * BLOCK >= range.end || end * BLOCK <= range.start {
                continue;
            }
            if best.len() == 100 && smallest >= *best.peek().unwrap() {
                continue;
            }
            if end - start == 1 {
                for id in &self.ids[(start * BLOCK).max(range.start)
                    ..((end * BLOCK).min(range.end)).min(self.ids.len())]
                {
                    if !accept(*id) {
                        continue;
                    }
                    best.push(*id);
                    if best.len() > 100 {
                        best.pop();
                    }
                }
            } else {
                let middle = (start + end) / 2;
                pending.push(Reverse((self.minimum[node * 2], node * 2, start, middle)));
                pending.push(Reverse((
                    self.minimum[node * 2 + 1],
                    node * 2 + 1,
                    middle,
                    end,
                )));
            }
        }
        best.into_sorted_vec()
    }
}
pub struct PathSearch {
    #[cfg(test)]
    examined: std::sync::atomic::AtomicUsize,
    files: Arc<Vec<PathBuf>>,
    folders: Arc<Vec<PathBuf>>,
    names: Vec<Name>,
    relative: Order,
    filename: Order,
    stem: Order,
    paths: Grams,
    basenames: Grams,
}
impl PathSearch {
    pub fn build(
        root: &Path,
        files: Arc<Vec<PathBuf>>,
        folders: Arc<Vec<PathBuf>>,
        cancel: impl Fn() -> bool,
    ) -> Option<Self> {
        let mut names = Vec::with_capacity(files.len() + folders.len());
        for path in folders.iter().chain(files.iter()) {
            if cancel() {
                return None;
            }
            let text = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_lowercase()
                .replace('\\', "/");
            let name = text.rfind('/').map_or(0, |i| i + 1);
            let stem = Path::new(&text[name..])
                .file_stem()
                .and_then(|s| s.to_str())
                .map_or(text.len(), |s| name + s.len());
            names.push(Name { text, name, stem });
        }
        let ids = || (0..names.len() as u32).collect::<Vec<_>>();
        let relative = Order::new(ids(), |a, b| {
            names[*a as usize]
                .text
                .cmp(&names[*b as usize].text)
                .then(a.cmp(b))
        });
        if cancel() {
            return None;
        }
        let filename = Order::new(ids(), |a, b| {
            names[*a as usize]
                .name()
                .cmp(names[*b as usize].name())
                .then(a.cmp(b))
        });
        if cancel() {
            return None;
        }
        let stem = Order::new(ids(), |a, b| {
            names[*a as usize]
                .stem()
                .cmp(names[*b as usize].stem())
                .then(a.cmp(b))
        });
        let mut index = Self {
            #[cfg(test)]
            examined: std::sync::atomic::AtomicUsize::new(0),
            relative,
            filename,
            stem,
            names,
            files,
            folders,
            paths: Grams::default(),
            basenames: Grams::default(),
        };
        for block in 0..index.names.len().div_ceil(BLOCK) {
            if cancel() {
                return None;
            }
            let range = block * BLOCK..((block + 1) * BLOCK).min(index.names.len());
            index.paths.add(
                block,
                index.names[range.clone()].iter().map(|n| n.text.as_str()),
            );
            index
                .basenames
                .add(block, index.names[range].iter().map(Name::name));
        }
        Some(index)
    }
    #[cfg(test)]
    pub fn examined(&self) -> usize {
        self.examined.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn path(&self, id: u32) -> &PathBuf {
        self.folders
            .get(id as usize)
            .unwrap_or_else(|| &self.files[id as usize - self.folders.len()])
    }
    fn matching(
        &self,
        order: &Order,
        query: &str,
        field: impl Fn(&Name) -> &str,
        prefix: bool,
        accept: &impl Fn(&Path) -> bool,
    ) -> Vec<u32> {
        let start = order
            .ids
            .partition_point(|id| field(&self.names[*id as usize]) < query);
        let end = order.ids.partition_point(|id| {
            let text = field(&self.names[*id as usize]);
            if prefix {
                text < query || text.starts_with(query)
            } else {
                text <= query
            }
        });
        order.first(start..end, |id| {
            #[cfg(test)]
            self.examined
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            accept(self.path(id))
        })
    }
    /// Results use the original folder/file priority order within each score.
    pub fn query(&self, query: &str, cancel: impl Fn() -> bool) -> Vec<PathBuf> {
        self.query_filtered(query, cancel, |_| true)
    }
    pub fn query_filtered(
        &self,
        query: &str,
        cancel: impl Fn() -> bool,
        accept: impl Fn(&Path) -> bool,
    ) -> Vec<PathBuf> {
        #[cfg(test)]
        self.examined.store(0, std::sync::atomic::Ordering::Relaxed);
        let query = query.to_lowercase().replace('\\', "/");
        if query.is_empty() {
            return (0..self.names.len() as u32)
                .filter(|id| accept(self.path(*id)))
                .take(100)
                .map(|id| self.path(id).clone())
                .collect();
        }
        let mut found = Vec::new();
        let mut seen = HashSet::new();
        let mut exact = self.matching(&self.filename, &query, Name::name, false, &accept);
        exact.extend(self.matching(&self.relative, &query, |n| &n.text, false, &accept));
        exact.sort_unstable();
        for phase in [
            exact,
            self.matching(&self.stem, &query, Name::stem, false, &accept),
            self.matching(&self.filename, &query, Name::name, true, &accept),
        ] {
            for id in phase {
                if cancel() {
                    return Vec::new();
                }
                if seen.insert(id) {
                    found.push(self.path(id).clone());
                    if found.len() == 100 {
                        return found;
                    }
                }
            }
        }
        for (grams, filename) in [(&self.basenames, true), (&self.paths, false)] {
            for block in grams.blocks(&query) {
                if cancel() {
                    return Vec::new();
                }
                for id in
                    block as usize * BLOCK..((block as usize + 1) * BLOCK).min(self.names.len())
                {
                    #[cfg(test)]
                    self.examined
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let name = &self.names[id];
                    let text = if filename { name.name() } else { &name.text };
                    if text.contains(&query)
                        && accept(self.path(id as u32))
                        && seen.insert(id as u32)
                    {
                        found.push(self.path(id as u32).clone());
                        if found.len() == 100 {
                            return found;
                        }
                    }
                }
            }
        }
        found
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn indexed_substrings_preserve_ranking_unicode_and_short_queries() {
        let root = Path::new("/p");
        let files = Arc::new(
            [
                "src/zapps.rs",
                "src/apps.ts",
                "src/apps/test.ts",
                "src/猫λ.rs",
                "lib/xa.rs",
                "a/a.rs",
            ]
            .iter()
            .map(|p| root.join(p))
            .collect(),
        );
        let index = PathSearch::build(root, files, Arc::default(), || false).unwrap();
        assert_eq!(
            index.query("apps", || false),
            ["src/apps.ts", "src/zapps.rs", "src/apps/test.ts"].map(|p| root.join(p))
        );
        assert_eq!(index.query("猫λ", || false), [root.join("src/猫λ.rs")]);
        assert_eq!(
            index.query("a", || false).first(),
            Some(&root.join("a/a.rs"))
        );
        assert!(index.query("does-not-exist", || false).is_empty());
        assert!(index.query("apps", || true).is_empty());
    }
}
