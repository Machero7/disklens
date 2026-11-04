//! Parallel folder walk.
//!
//! Workers pull folders from a shared queue, read them with `read_dir` + `lstat`,
//! reserve ids for the subfolders they find, send a [`DirResult`] and only then queue
//! the subfolders. That ordering guarantees a parent's result always arrives before
//! any of its children's.

use std::collections::HashSet;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
pub use crossbeam_channel::TryRecvError;

use crate::tree::FileEntry;

const VIRTUAL_FS: &[&str] = &[
    "proc", "sysfs", "devtmpfs", "devpts", "cgroup", "cgroup2", "securityfs", "debugfs",
    "tracefs", "bpf", "pstore", "configfs", "fusectl", "mqueue", "hugetlbfs", "autofs",
    "binfmt_misc", "efivarfs", "nsfs", "ramfs",
];

#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Don't descend into other mounted filesystems.
    pub one_fs: bool,
    pub threads: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions { one_fs: true, threads: 16 }
    }
}

/// Everything found in one folder.
#[derive(Debug)]
pub struct DirResult {
    pub id: u32,
    pub files: Vec<FileEntry>,
    pub names: Vec<u8>,
    /// (reserved id, name, mtime) per subfolder.
    pub subdirs: Vec<(u32, Box<[u8]>, i64)>,
    pub error: Option<Box<str>>,
}

#[derive(Debug)]
pub enum Msg {
    Dir(DirResult),
    /// A file or folder that couldn't be read.
    Error(PathBuf, String),
    /// The whole scan failed (for example, the NAS couldn't be reached); `Done` follows.
    Fatal(String),
    Done,
}

/// Live counters, readable from any thread while scanning.
#[derive(Default, Debug)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
}

struct Shared {
    work_tx: Sender<Option<(u32, PathBuf)>>,
    result_tx: Sender<Msg>,
    pending: AtomicUsize,
    next_id: AtomicU32,
    cancel: Arc<AtomicBool>,
    progress: Arc<Progress>,
    seen_inodes: Mutex<HashSet<(u64, u64)>>,
    root_dev: u64,
    virtual_devs: HashSet<u64>,
    opts: ScanOptions,
}

pub struct Scanner {
    pub rx: Receiver<Msg>,
    pub progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

impl Scanner {
    /// Scan `path` into folder `root_id`; new folders get ids from `next_id` upwards.
    /// `root_dev` is the device the scan must stay on (defaults to `path`'s).
    pub fn start(
        root_id: u32,
        path: PathBuf,
        next_id: u32,
        root_dev: Option<u64>,
        opts: ScanOptions,
    ) -> std::io::Result<Scanner> {
        let root_dev = match root_dev {
            Some(d) => d,
            None => fs::metadata(&path)?.dev(),
        };
        let (work_tx, work_rx) = unbounded();
        let (result_tx, rx) = unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Progress::default());
        let threads = opts.threads.max(1);
        let shared = Arc::new(Shared {
            work_tx,
            result_tx,
            pending: AtomicUsize::new(1),
            next_id: AtomicU32::new(next_id),
            cancel: cancel.clone(),
            progress: progress.clone(),
            seen_inodes: Mutex::new(HashSet::new()),
            root_dev,
            virtual_devs: virtual_devs(),
            opts,
        });
        shared.work_tx.send(Some((root_id, path))).expect("queue open");
        for i in 0..threads {
            let shared = shared.clone();
            let work_rx = work_rx.clone();
            thread::Builder::new()
                .name(format!("disklens-scan-{i}"))
                .spawn(move || worker(shared, work_rx, threads))?;
        }
        Ok(Scanner { rx, progress, cancel, started: Instant::now() })
    }

    /// For other walkers (the SSH scanner) that feed the same channel.
    pub(crate) fn from_parts(rx: Receiver<Msg>, progress: Arc<Progress>, cancel: Arc<AtomicBool>) -> Self {
        Scanner { rx, progress, cancel, started: Instant::now() }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

fn worker(sh: Arc<Shared>, work_rx: Receiver<Option<(u32, PathBuf)>>, threads: usize) {
    while let Ok(Some((id, path))) = work_rx.recv() {
        if !sh.cancel.load(Ordering::Relaxed) {
            scan_dir(&sh, id, &path);
        }
        if sh.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            let _ = sh.result_tx.send(Msg::Done);
            for _ in 0..threads {
                let _ = sh.work_tx.send(None);
            }
        }
    }
}

fn scan_dir(sh: &Shared, id: u32, path: &Path) {
    let rd = match fs::read_dir(path) {
        Ok(rd) => rd,
        Err(e) => {
            let msg = io_msg(&e);
            let _ = sh.result_tx.send(Msg::Error(path.to_path_buf(), msg.clone()));
            let _ = sh.result_tx.send(Msg::Dir(DirResult {
                id,
                files: Vec::new(),
                names: Vec::new(),
                subdirs: Vec::new(),
                error: Some(msg.into()),
            }));
            return;
        }
    };
    let mut files = Vec::new();
    let mut names: Vec<u8> = Vec::new();
    let mut subdirs = Vec::new();
    let mut work = Vec::new();
    let mut bytes = 0u64;

    for entry in rd {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                let _ = sh.result_tx.send(Msg::Error(path.to_path_buf(), io_msg(&e)));
                continue;
            }
        };
        // DirEntry::metadata does not follow symlinks.
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                let _ = sh.result_tx.send(Msg::Error(entry.path(), io_msg(&e)));
                continue;
            }
        };
        let name = entry.file_name();
        let name = name.as_bytes();
        if md.is_dir() {
            let dev = md.dev();
            if dev != sh.root_dev && (sh.opts.one_fs || sh.virtual_devs.contains(&dev)) {
                continue;
            }
            let cid = sh.next_id.fetch_add(1, Ordering::Relaxed);
            subdirs.push((cid, Box::<[u8]>::from(name), md.mtime()));
            work.push((cid, entry.path()));
            continue;
        }
        let mut counted = true;
        if md.nlink() > 1 {
            let key = (md.dev(), md.ino());
            counted = sh.seen_inodes.lock().unwrap().insert(key);
        }
        let name_len = name.len().min(u16::MAX as usize);
        files.push(FileEntry {
            name_off: names.len() as u32,
            name_len: name_len as u16,
            counted,
            removed: false,
            size: md.size(),
            alloc: md.blocks() * 512,
            mtime: md.mtime(),
        });
        names.extend_from_slice(&name[..name_len]);
        if counted {
            bytes += md.size();
        }
    }

    sh.progress.files.fetch_add(files.len() as u64, Ordering::Relaxed);
    sh.progress.dirs.fetch_add(subdirs.len() as u64, Ordering::Relaxed);
    sh.progress.bytes.fetch_add(bytes, Ordering::Relaxed);
    files.shrink_to_fit();
    names.shrink_to_fit();

    // Result first, then queue the children: parents always arrive before children.
    let _ = sh.result_tx.send(Msg::Dir(DirResult { id, files, names, subdirs, error: None }));
    sh.pending.fetch_add(work.len(), Ordering::AcqRel);
    for item in work {
        let _ = sh.work_tx.send(Some(item));
    }
}

fn io_msg(e: &std::io::Error) -> String {
    let s = e.to_string();
    // "Permission denied (os error 13)" -> "Permission denied"
    match s.find(" (os error") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

fn virtual_devs() -> HashSet<u64> {
    let mut devs = HashSet::new();
    if let Ok(mounts) = fs::read_to_string("/proc/self/mounts") {
        for line in mounts.lines() {
            let mut parts = line.split_whitespace();
            let (_, Some(mnt), Some(fstype)) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            if VIRTUAL_FS.contains(&fstype) {
                if let Ok(md) = fs::metadata(mnt.replace("\\040", " ")) {
                    devs.insert(md.dev());
                }
            }
        }
    }
    devs
}

/// Convenience: scan `path` to completion on the current thread.
pub fn scan_blocking(path: &Path, opts: ScanOptions) -> std::io::Result<(crate::Tree, Vec<(PathBuf, String)>)> {
    let mut tree = crate::Tree::new(path);
    let scanner = Scanner::start(crate::Tree::ROOT, path.to_path_buf(), tree.next_id(), None, opts)?;
    let mut errors = Vec::new();
    for msg in scanner.rx.iter() {
        match msg {
            Msg::Dir(r) => tree.apply(r),
            Msg::Error(p, e) => errors.push((p, e)),
            Msg::Fatal(e) => errors.push((path.to_path_buf(), e)),
            Msg::Done => break,
        }
    }
    Ok((tree, errors))
}
