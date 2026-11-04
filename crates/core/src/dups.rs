//! Duplicate finder: same size → same first and last 64 KB → same full contents (BLAKE3).
//! Hard links never show up as duplicates: the scan already marks 2nd+ names of an inode
//! as not counted, and those are skipped here. Work is spread over threads and can be stopped.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;

use crate::stats::FileRef;
use crate::tree::Tree;

/// Bytes hashed at the start and at the end of each file in the quick pass.
pub const PARTIAL: u64 = 64 * 1024;

pub const PHASES: [&str; 3] = ["Collecting files", "Comparing start and end of files", "Comparing full contents"];

/// Live progress, shared with the UI thread.
#[derive(Default)]
pub struct Progress {
    pub phase: AtomicU8,
    pub done_files: AtomicU64,
    pub total_files: AtomicU64,
    pub done_bytes: AtomicU64,
    pub total_bytes: AtomicU64,
    pub errors: AtomicU64,
    pub cancel: AtomicBool,
}

impl Progress {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// A file to consider, collected from the tree on the UI thread.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub r: FileRef,
    pub size: u64,
    pub path: PathBuf,
}

/// Files with identical contents.
#[derive(Clone, Debug)]
pub struct Group {
    pub size: u64,
    pub members: Vec<FileRef>,
}

impl Group {
    /// Space freed by keeping just one copy.
    pub fn reclaimable(&self) -> u64 {
        self.size * (self.members.len() as u64 - 1)
    }
}

/// Files under `root` of at least `min_size` bytes (and > 0) that share their size with another.
pub fn collect(tree: &Tree, root: u32, min_size: u64, show_hidden: bool) -> Vec<Candidate> {
    let mut by_size: HashMap<u64, Vec<FileRef>> = HashMap::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let d = tree.get(id);
        stack.extend_from_slice(&d.dirs);
        for (i, f) in d.files.iter().enumerate() {
            if f.removed || !f.counted || f.size == 0 || f.size < min_size {
                continue;
            }
            if !show_hidden && d.file_name_bytes(f).first() == Some(&b'.') {
                continue;
            }
            by_size.entry(f.size).or_default().push(FileRef { dir: id, idx: i as u32 });
        }
    }
    let mut out = Vec::new();
    for (size, refs) in by_size {
        if refs.len() > 1 {
            for r in refs {
                let d = tree.get(r.dir);
                out.push(Candidate { r, size, path: tree.file_path(r.dir, &d.files[r.idx as usize]) });
            }
        }
    }
    out
}

fn partial_hash(c: &Candidate) -> std::io::Result<[u8; 32]> {
    let mut f = File::open(&c.path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; PARTIAL as usize];
    let n = read_full(&mut f, &mut buf)?;
    h.update(&buf[..n]);
    if c.size > 2 * PARTIAL {
        f.seek(SeekFrom::Start(c.size - PARTIAL))?;
        let n = read_full(&mut f, &mut buf)?;
        h.update(&buf[..n]);
    }
    Ok(*h.finalize().as_bytes())
}

fn full_hash(c: &Candidate, p: &Progress) -> std::io::Result<Option<[u8; 32]>> {
    let mut f = File::open(&c.path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if p.cancelled() {
            return Ok(None);
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        p.done_bytes.fetch_add(n as u64, Ordering::Relaxed);
    }
    Ok(Some(*h.finalize().as_bytes()))
}

fn read_full(f: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Split every group by `key` (computed on `threads` threads); keep sub-groups of 2+.
fn regroup<K, F>(groups: Vec<Vec<Candidate>>, threads: usize, p: &Progress, key: F) -> Vec<Vec<Candidate>>
where
    K: std::hash::Hash + Eq + Send,
    F: Fn(&Candidate) -> Option<K> + Sync,
{
    let items: Vec<(usize, Candidate)> = groups.into_iter().enumerate().flat_map(|(g, v)| v.into_iter().map(move |c| (g, c))).collect();
    p.total_files.store(items.len() as u64, Ordering::Relaxed);
    p.done_files.store(0, Ordering::Relaxed);
    let next = AtomicU64::new(0);
    let keys: Mutex<Vec<Option<K>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..threads.max(1) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed) as usize;
                if i >= items.len() || p.cancelled() {
                    break;
                }
                let k = key(&items[i].1);
                keys.lock().unwrap()[i] = k;
                p.done_files.fetch_add(1, Ordering::Relaxed);
            });
        }
    });
    let keys = keys.into_inner().unwrap();
    let mut out: HashMap<(usize, K), Vec<Candidate>> = HashMap::new();
    for ((g, c), k) in items.into_iter().zip(keys) {
        if let Some(k) = k {
            out.entry((g, k)).or_default().push(c);
        }
    }
    out.into_values().filter(|v| v.len() > 1).collect()
}

/// Find groups of identical files among `cands`, biggest reclaimable space first.
/// Returns an empty list if cancelled.
pub fn find(cands: Vec<Candidate>, threads: usize, p: &Progress) -> Vec<Group> {
    p.phase.store(0, Ordering::Relaxed);
    let mut by_size: HashMap<u64, Vec<Candidate>> = HashMap::new();
    for c in cands {
        by_size.entry(c.size).or_default().push(c);
    }
    let groups: Vec<Vec<Candidate>> = by_size.into_values().filter(|v| v.len() > 1).collect();

    p.phase.store(1, Ordering::Relaxed);
    let err = |e: std::io::Error| {
        let _ = e;
        p.errors.fetch_add(1, Ordering::Relaxed);
    };
    let groups = regroup(groups, threads, p, |c| partial_hash(c).map_err(err).ok());
    if p.cancelled() {
        return Vec::new();
    }

    p.phase.store(2, Ordering::Relaxed);
    // small files were covered completely by the partial hash
    let (small, big): (Vec<_>, Vec<_>) = groups.into_iter().partition(|g| g[0].size <= 2 * PARTIAL);
    p.total_bytes.store(big.iter().flatten().map(|c| c.size).sum(), Ordering::Relaxed);
    p.done_bytes.store(0, Ordering::Relaxed);
    let big = regroup(big, threads, p, |c| full_hash(c, p).map_err(err).ok().flatten());
    if p.cancelled() {
        return Vec::new();
    }

    let mut out: Vec<Group> = small
        .into_iter()
        .chain(big)
        .map(|g| {
            let mut members: Vec<FileRef> = g.iter().map(|c| c.r).collect();
            members.sort();
            Group { size: g[0].size, members }
        })
        .collect();
    out.sort_by(|a, b| b.reclaimable().cmp(&a.reclaimable()).then_with(|| a.members.cmp(&b.members)));
    out
}
