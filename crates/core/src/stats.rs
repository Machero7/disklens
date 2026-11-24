//! Subtree statistics: biggest files, space per file type, space per file age.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use crate::tree::Tree;
use crate::util::ext_of;

/// Age buckets: (label, upper bound in days; None = open-ended).
pub const AGE_BUCKETS: &[(&str, Option<u32>)] = &[
    ("Less than 1 month", Some(30)),
    ("1 – 6 months", Some(182)),
    ("6 – 12 months", Some(365)),
    ("1 – 2 years", Some(730)),
    ("2 – 5 years", Some(1826)),
    ("More than 5 years", None),
];

/// A file located by (folder id, index into that folder's `files`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileRef {
    pub dir: u32,
    pub idx: u32,
}

#[derive(Debug, Default, Clone)]
pub struct ExtStat {
    pub size: u64,
    pub count: u64,
    /// One example file name, for looking up a type description and icon.
    pub sample: String,
}

#[derive(Debug, Default)]
pub struct Stats {
    /// Biggest files, largest first.
    pub top: Vec<FileRef>,
    pub exts: HashMap<String, ExtStat>,
    /// (size, count) per [`AGE_BUCKETS`] entry.
    pub ages: Vec<(u64, u64)>,
    pub total: u64,
}

/// Walk the subtree under `root`. `show_hidden=false` skips dot-files (not dot-folders' contents
/// in totals: hidden folders are still walked).
pub fn compute(tree: &Tree, root: u32, ondisk: bool, top_n: usize, show_hidden: bool, now: i64) -> Stats {
    compute_filtered(tree, root, ondisk, top_n, show_hidden, now, None)
}

/// Like [`compute`], but the biggest-files list only holds files of type `only_ext`
/// (a key from [`crate::util::ext_of`]); the per-type and per-age figures still cover everything.
pub fn compute_filtered(
    tree: &Tree,
    root: u32,
    ondisk: bool,
    top_n: usize,
    show_hidden: bool,
    now: i64,
    only_ext: Option<&str>,
) -> Stats {
    let mut heap: BinaryHeap<Reverse<(u64, FileRef)>> = BinaryHeap::with_capacity(top_n + 1);
    let mut exts: HashMap<String, ExtStat> = HashMap::new();
    let mut ages = vec![(0u64, 0u64); AGE_BUCKETS.len()];
    let mut total = 0u64;
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let d = tree.get(id);
        stack.extend_from_slice(&d.dirs);
        for (i, f) in d.files.iter().enumerate() {
            if f.removed {
                continue; // trashed after the scan
            }
            let name = d.file_name(f);
            if !show_hidden && name.starts_with('.') {
                continue;
            }
            let v = f.value(ondisk);
            // A 2nd+ name of a hard-linked file is still a file in the lists and the counts,
            // but its bytes are only added once, exactly like the folder totals.
            let add = if f.counted { v } else { 0 };
            total += add;
            let r = FileRef { dir: id, idx: i as u32 };
            let ext = ext_of(&name);
            let in_top = only_ext.is_none_or(|x| x == ext);
            if in_top {
                if heap.len() < top_n {
                    heap.push(Reverse((v, r)));
                } else if let Some(Reverse((min, _))) = heap.peek() {
                    if v > *min {
                        heap.pop();
                        heap.push(Reverse((v, r)));
                    }
                }
            }
            let e = exts.entry(ext).or_default();
            if e.count == 0 {
                e.sample = name.into_owned();
            }
            e.size += add;
            e.count += 1;
            let days = (now - f.mtime).max(0) / 86_400;
            let b = AGE_BUCKETS
                .iter()
                .position(|(_, lim)| lim.map_or(true, |l| days < l as i64))
                .unwrap();
            ages[b].0 += add;
            ages[b].1 += 1;
        }
    }
    let mut top: Vec<(u64, FileRef)> = heap.into_iter().map(|Reverse(x)| x).collect();
    top.sort_by(|a, b| b.cmp(a));
    Stats { top: top.into_iter().map(|(_, r)| r).collect(), exts, ages, total }
}

/// Every file of type `ext` (a key from [`crate::util::ext_of`]) under `root`, in walk order.
/// Skips removed files and, unless `show_hidden`, dot-files, matching what the lists show.
pub fn files_of_type(tree: &Tree, root: u32, ext: &str, show_hidden: bool) -> Vec<FileRef> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let d = tree.get(id);
        stack.extend_from_slice(&d.dirs);
        for (i, f) in d.files.iter().enumerate() {
            if f.removed {
                continue;
            }
            let name = d.file_name(f);
            if (!show_hidden && name.starts_with('.')) || ext_of(&name) != ext {
                continue;
            }
            out.push(FileRef { dir: id, idx: i as u32 });
        }
    }
    out
}

/// Ids of every folder that has a file of type `ext` somewhere inside it (itself included).
pub fn dirs_with_type(tree: &Tree, ext: &str, show_hidden: bool) -> std::collections::HashSet<u32> {
    let mut set = std::collections::HashSet::new();
    for (id, d) in tree.dirs.iter().enumerate() {
        let id = id as u32;
        let has = d.files.iter().any(|f| {
            let name = d.file_name(f);
            !f.removed && (show_hidden || !name.starts_with('.')) && ext_of(&name) == ext
        });
        if !has || !tree.is_attached(id) {
            continue;
        }
        let mut n = id;
        while n != crate::NONE && set.insert(n) {
            n = tree.get(n).parent;
        }
    }
    set
}
