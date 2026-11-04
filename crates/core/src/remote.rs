//! Scanning a folder on another machine (a NAS) over SSH.
//!
//! One `ssh` command runs GNU `find` on the remote machine, which prints one NUL-terminated
//! record per file and folder (type, size, blocks, mtime, device, inode, link count, path).
//! The walk happens on the NAS at disk speed; only a few dozen bytes per file cross the network.
//!
//! The output has two parts: first the scanned folder's direct entries (so its subfolders get
//! ids and show up at once), then everything deeper. `find` walks depth-first, so all entries
//! under one top-level folder arrive together; when the next top-level folder starts, the previous
//! one is complete and is sent as [`DirResult`]s, parents before children, exactly like the local
//! scanner. The system `ssh` is used, so keys, the agent and `~/.ssh/config` all apply.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::{unbounded, Sender};

use crate::scan::{DirResult, Msg, Progress, ScanOptions, Scanner};
use crate::tree::FileEntry;

/// Where to scan. `host` is anything `ssh` accepts (`user@192.168.1.50`, an alias from
/// `~/.ssh/config`); `None` runs the same command locally, which the tests use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteSpec {
    pub host: Option<String>,
    /// Absolute folder on the remote machine, e.g. `/volume1/video`.
    pub path: String,
}

impl RemoteSpec {
    /// Understands `user@host:/path`, `host:/path` and `ssh://user@host/path`.
    /// Returns None for anything else (a local path).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Some(rest) = s.strip_prefix("ssh://") {
            let (host, path) = rest.split_once('/')?;
            return (!host.is_empty()).then(|| RemoteSpec { host: Some(host.to_string()), path: format!("/{path}") });
        }
        let (host, path) = s.split_once(':')?;
        if host.is_empty() || host.contains('/') || !path.starts_with('/') {
            return None;
        }
        Some(RemoteSpec { host: Some(host.to_string()), path: path.to_string() })
    }

    /// "user@host:/path" for titles.
    pub fn label(&self) -> String {
        match &self.host {
            Some(h) => format!("{h}:{}", self.path),
            None => self.path.clone(),
        }
    }
}

const FMT: &str = r"%y\t%s\t%b\t%T@\t%D\t%i\t%n\t%P\0";
/// Printed between the two parts; can't be confused with a record (records have 7 tabs).
const SPLIT: &[u8] = b"--disklens-split--";

/// Quote for a POSIX shell.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The shell command run on the remote machine.
pub fn remote_command(path: &str, one_fs: bool) -> String {
    let xdev = if one_fs { " -xdev" } else { "" };
    format!(
        "cd -- {p} || exit 3; \
         LC_ALL=C find .{xdev} -mindepth 1 -maxdepth 1 -printf '{FMT}'; \
         printf '%s\\0' '{split}'; \
         LC_ALL=C find .{xdev} -mindepth 2 -printf '{FMT}'",
        p = sh_quote(path),
        split = std::str::from_utf8(SPLIT).unwrap(),
    )
}

fn command(spec: &RemoteSpec, one_fs: bool) -> Command {
    let script = remote_command(&spec.path, one_fs);
    match &spec.host {
        Some(host) => {
            let mut c = Command::new("ssh");
            // BatchMode: never prompt for a password (there's no terminal to type it into)
            c.args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "--", host, &script]);
            c
        }
        None => {
            let mut c = Command::new("sh");
            c.args(["-c", &script]);
            c
        }
    }
}

struct Record<'a> {
    kind: u8,
    size: u64,
    blocks: u64,
    mtime: i64,
    dev: u64,
    ino: u64,
    nlink: u64,
    path: &'a [u8],
}

fn parse(rec: &[u8]) -> Option<Record<'_>> {
    let mut it = rec.splitn(8, |&b| b == b'\t');
    let mut field = || it.next();
    let kind = *field()?.first()?;
    let num = |b: &[u8]| std::str::from_utf8(b).ok()?.parse::<u64>().ok();
    let size = num(field()?)?;
    let blocks = num(field()?)?;
    let mtime = std::str::from_utf8(field()?).ok()?.parse::<f64>().ok()? as i64;
    let dev = num(field()?)?;
    let ino = num(field()?)?;
    let nlink = num(field()?)?;
    let path = field()?;
    Some(Record { kind, size, blocks, mtime, dev, ino, nlink, path })
}

/// One folder being collected.
#[derive(Default)]
struct Pending {
    id: u32,
    files: Vec<FileEntry>,
    names: Vec<u8>,
    subdirs: Vec<(u32, Box<[u8]>, i64)>,
}

impl Pending {
    fn add_file(&mut self, name: &[u8], r: &Record, counted: bool) {
        let len = name.len().min(u16::MAX as usize);
        self.files.push(FileEntry {
            name_off: self.names.len() as u32,
            name_len: len as u16,
            counted,
            removed: false,
            size: r.size,
            alloc: r.blocks * 512,
            mtime: r.mtime,
        });
        self.names.extend_from_slice(&name[..len]);
    }

    fn into_result(self) -> Msg {
        Msg::Dir(DirResult { id: self.id, files: self.files, names: self.names, subdirs: self.subdirs, error: None })
    }
}

fn split_parent(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|&b| b == b'/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => (&[], path),
    }
}

/// Turns the record stream into folder results.
struct Builder {
    tx: Sender<Msg>,
    progress: Arc<Progress>,
    next_id: u32,
    root: Pending,
    /// Top-level folder name → id (from part one).
    top_ids: HashMap<Vec<u8>, u32>,
    /// Top-level folders already sent.
    top_done: HashSet<u32>,
    /// The top-level folder being collected, its folders in walk order, and path → index.
    current_top: Option<Vec<u8>>,
    dirs: Vec<Pending>,
    index: HashMap<Vec<u8>, usize>,
    seen_inodes: HashSet<(u64, u64)>,
}

impl Builder {
    fn counted(&mut self, r: &Record) -> bool {
        r.nlink <= 1 || self.seen_inodes.insert((r.dev, r.ino))
    }

    fn reserve(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Part one: the scanned folder's direct entries.
    fn top_level(&mut self, r: Record) {
        if r.kind == b'd' {
            let id = self.reserve();
            self.root.subdirs.push((id, r.path.into(), r.mtime));
            self.top_ids.insert(r.path.to_vec(), id);
        } else {
            let counted = self.counted(&r);
            if counted {
                self.progress.bytes.fetch_add(r.size, Ordering::Relaxed);
            }
            self.progress.files.fetch_add(1, Ordering::Relaxed);
            self.root.add_file(r.path, &r, counted);
        }
    }

    /// End of part one: send the scanned folder itself.
    fn send_root(&mut self) {
        self.progress.dirs.fetch_add(self.root.subdirs.len() as u64, Ordering::Relaxed);
        let root = std::mem::take(&mut self.root);
        let _ = self.tx.send(root.into_result());
    }

    /// Part two: everything at depth 2 and below.
    fn deep(&mut self, r: Record) {
        let top_len = r.path.iter().position(|&b| b == b'/').unwrap_or(r.path.len());
        let top = &r.path[..top_len];
        if self.current_top.as_deref() != Some(top) {
            self.flush();
            let Some(&id) = self.top_ids.get(top) else { return };
            self.current_top = Some(top.to_vec());
            self.dirs.push(Pending { id, ..Default::default() });
            self.index.insert(top.to_vec(), 0);
        }
        let (parent, name) = split_parent(r.path);
        let Some(&pi) = self.index.get(parent) else { return };
        if r.kind == b'd' {
            let id = self.reserve();
            self.dirs[pi].subdirs.push((id, name.into(), r.mtime));
            self.index.insert(r.path.to_vec(), self.dirs.len());
            self.dirs.push(Pending { id, ..Default::default() });
            self.progress.dirs.fetch_add(1, Ordering::Relaxed);
        } else {
            let counted = self.counted(&r);
            if counted {
                self.progress.bytes.fetch_add(r.size, Ordering::Relaxed);
            }
            self.progress.files.fetch_add(1, Ordering::Relaxed);
            self.dirs[pi].add_file(name, &r, counted);
        }
    }

    /// Send the finished top-level folder, parents first (walk order).
    fn flush(&mut self) {
        for d in self.dirs.drain(..) {
            self.top_done.insert(d.id);
            let _ = self.tx.send(d.into_result());
        }
        self.index.clear();
        self.current_top = None;
    }

    /// Top-level folders with nothing below them never appear in part two.
    fn finish(&mut self) {
        self.flush();
        let ids: Vec<u32> = self.top_ids.values().copied().filter(|id| !self.top_done.contains(id)).collect();
        for id in ids {
            let _ = self.tx.send(Pending { id, ..Default::default() }.into_result());
        }
    }
}

fn first_lines(text: &str, n: usize) -> String {
    text.lines().filter(|l| !l.trim().is_empty()).take(n).collect::<Vec<_>>().join("\n")
}

/// A plain-language explanation for common SSH failures.
fn explain(stderr: &str, host: Option<&str>) -> String {
    let host = host.unwrap_or("the NAS");
    let raw = first_lines(stderr, 3);
    if stderr.contains("Permission denied") && stderr.contains("publickey") || stderr.contains("Too many authentication failures") {
        format!(
            "{host} refused the login. DiskLens needs SSH key login (it can't ask for a password).\n\n\
             Set it up once in a terminal:\n    ssh-copy-id {host}\n\nDetails: {raw}"
        )
    } else if stderr.contains("Could not resolve hostname") {
        format!("Couldn't find {host}. Check the name or use its IP address.\n\nDetails: {raw}")
    } else if stderr.contains("Connection refused") {
        format!("{host} refused the connection. Is SSH turned on? (Synology: Control Panel → Terminal & SNMP → Enable SSH service.)\n\nDetails: {raw}")
    } else if stderr.contains("timed out") || stderr.contains("No route to host") {
        format!("Couldn't reach {host}. Is it on and on the same network or VPN?\n\nDetails: {raw}")
    } else if stderr.contains("Host key verification failed") {
        format!("{host} isn't a known host yet. Connect once in a terminal to accept its key:\n    ssh {host}\n\nDetails: {raw}")
    } else if stderr.contains("unknown predicate") || stderr.contains("-printf") {
        format!("The `find` command on {host} doesn't support -printf (it needs GNU find).\n\nDetails: {raw}")
    } else if raw.is_empty() {
        format!("The scan on {host} failed.")
    } else {
        raw
    }
}

/// Start scanning `spec.path` into folder `root_id`; new folders get ids from `next_id`.
pub fn start(root_id: u32, next_id: u32, spec: RemoteSpec, opts: ScanOptions) -> std::io::Result<Scanner> {
    let mut child = command(&spec, opts.one_fs)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let (tx, rx) = unbounded();
    let progress = Arc::new(Progress::default());
    let cancel = Arc::new(AtomicBool::new(false));
    let child = Arc::new(Mutex::new(child));

    // stderr: per-file errors ("find: './x': Permission denied") go to the error list;
    // the full text is kept for explaining a failed connection.
    let err_text = Arc::new(Mutex::new(String::new()));
    let err_thread = {
        let (tx, err_text) = (tx.clone(), err_text.clone());
        let base = spec.path.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(rest) = line.strip_prefix("find: ") {
                    // "find: './a/b': Permission denied"
                    if let Some((p, msg)) = rest.strip_prefix('\'').and_then(|r| r.split_once("': ")) {
                        let rel = p.trim_start_matches("./");
                        let _ = tx.send(Msg::Error(PathBuf::from(&base).join(rel), msg.to_string()));
                        continue;
                    }
                }
                let mut t = err_text.lock().unwrap();
                t.push_str(&line);
                t.push('\n');
            }
        })
    };

    let reader_cancel = cancel.clone();
    let reader_progress = progress.clone();
    let reader_child = child.clone();
    let host = spec.host.clone();
    std::thread::Builder::new().name("disklens-ssh".into()).spawn(move || {
        let mut b = Builder {
            tx: tx.clone(),
            progress: reader_progress,
            next_id,
            root: Pending { id: root_id, ..Default::default() },
            top_ids: HashMap::new(),
            top_done: HashSet::new(),
            current_top: None,
            dirs: Vec::new(),
            index: HashMap::new(),
            seen_inodes: HashSet::new(),
        };
        let mut r = BufReader::with_capacity(1 << 20, stdout);
        let mut rec = Vec::with_capacity(512);
        let mut part_two = false;
        loop {
            if reader_cancel.load(Ordering::Relaxed) {
                let _ = reader_child.lock().unwrap().kill();
                break;
            }
            rec.clear();
            match r.read_until(0, &mut rec) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if rec.last() == Some(&0) {
                rec.pop();
            }
            if !part_two && rec == SPLIT {
                part_two = true;
                b.send_root();
                continue;
            }
            let Some(record) = parse(&rec) else { continue };
            if part_two {
                b.deep(record);
            } else {
                b.top_level(record);
            }
        }
        let status = reader_child.lock().unwrap().wait();
        let _ = err_thread.join();
        let cancelled = reader_cancel.load(Ordering::Relaxed);
        if part_two && !cancelled {
            b.finish();
        } else if !cancelled {
            // never got past part one: the connection or the command failed
            let text = err_text.lock().unwrap().clone();
            let code = status.ok().and_then(|s| s.code());
            let msg = if code == Some(3) {
                format!("The folder doesn't exist on {}, or can't be opened.\n\n{}", host.as_deref().unwrap_or("this machine"), first_lines(&text, 3))
            } else {
                explain(&text, host.as_deref())
            };
            let _ = tx.send(Msg::Fatal(msg));
        }
        let _ = tx.send(Msg::Done);
    })?;
    Ok(Scanner::from_parts(rx, progress, cancel))
}

/// Run a quick command over SSH and return its output (used to list a NAS's shares).
pub fn run(host: &str, script: &str) -> std::io::Result<String> {
    let out = Command::new("ssh")
        .args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "--", host, script])
        .stdin(Stdio::null())
        .output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let mut e = String::new();
        let _ = (&out.stderr[..]).read_to_string(&mut e);
        Err(std::io::Error::other(explain(&e, Some(host))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("/volume1/it's here"), r"'/volume1/it'\''s here'");
    }

    #[test]
    fn parse_record() {
        let r = parse(b"f\t1234\t8\t1700000000.5\t2049\t77\t2\ta/b c\td.txt").unwrap();
        assert_eq!((r.kind, r.size, r.blocks, r.mtime, r.dev, r.ino, r.nlink), (b'f', 1234, 8, 1_700_000_000, 2049, 77, 2));
        assert_eq!(r.path, b"a/b c\td.txt", "tabs in names survive");
        assert!(parse(b"garbage").is_none());
    }

    #[test]
    fn parse_spec() {
        let s = RemoteSpec::parse("user@192.168.1.50:/volume1/video").unwrap();
        assert_eq!((s.host.as_deref(), s.path.as_str()), (Some("user@192.168.1.50"), "/volume1/video"));
        let s = RemoteSpec::parse("ssh://mynas/volume1").unwrap();
        assert_eq!((s.host.as_deref(), s.path.as_str()), (Some("mynas"), "/volume1"));
        assert!(RemoteSpec::parse("~").is_none());
        assert!(RemoteSpec::parse("C:relative").is_none());
        assert!(RemoteSpec::parse("./a:/b").is_none());
    }

    #[test]
    fn explains_failures() {
        assert!(explain("user@nas: Permission denied (publickey,password).", Some("user@nas")).contains("ssh-copy-id user@nas"));
        assert!(explain("ssh: connect to host 1.2.3.4 port 22: Connection refused", Some("1.2.3.4")).contains("Enable SSH"));
    }
}
