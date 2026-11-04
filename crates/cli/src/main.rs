//! disklens-scan: scan a folder and print totals and the biggest files.
//!
//! Phase-0 tool for checking the engine against `du` and the Python version;
//! later the headless scanner for remote (SSH) scans.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use disklens_core::scan::{Msg, ScanOptions, Scanner};
use disklens_core::util::{fmt_int, human};
use disklens_core::{stats, Tree};

const USAGE: &str = "usage: disklens-scan PATH|USER@HOST:/PATH [--threads N] [--top N] [--all-fs] [--ondisk]";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut path: Option<PathBuf> = None;
    let mut opts = ScanOptions::default();
    let mut top_n = 20usize;
    let mut ondisk = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--threads" => opts.threads = args.next().and_then(|v| v.parse().ok()).unwrap_or(16),
            "--top" => top_n = args.next().and_then(|v| v.parse().ok()).unwrap_or(20),
            "--all-fs" => opts.one_fs = false,
            "--ondisk" => ondisk = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            _ if path.is_none() => path = Some(PathBuf::from(a)),
            _ => {
                eprintln!("{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    }
    let Some(path) = path else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    // user@host:/path scans a NAS over SSH
    let remote = disklens_core::remote::RemoteSpec::parse(&path.to_string_lossy()).filter(|_| !path.exists());
    let path = match &remote {
        Some(spec) => PathBuf::from(&spec.path),
        None => match std::fs::canonicalize(&path) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{}: {e}", path.display());
                return ExitCode::FAILURE;
            }
        },
    };

    let t0 = Instant::now();
    let mut tree = Tree::new(&path);
    let threads = opts.threads;
    let started = match remote {
        Some(spec) => disklens_core::remote::start(Tree::ROOT, tree.next_id(), spec, opts),
        None => Scanner::start(Tree::ROOT, path.clone(), tree.next_id(), None, opts),
    };
    let scanner = match started {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let mut errors = Vec::new();
    for msg in scanner.rx.iter() {
        match msg {
            Msg::Dir(r) => tree.apply(r),
            Msg::Error(p, e) => errors.push((p, e)),
            Msg::Fatal(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
            Msg::Done => break,
        }
    }
    let scan_time = t0.elapsed();

    let root = tree.get(Tree::ROOT);
    println!("{}", path.display());
    println!("  size        {} ({} bytes)", human(root.size), root.size);
    println!("  on disk     {} ({} bytes)", human(root.alloc), root.alloc);
    println!("  files       {}", fmt_int(root.nfiles));
    println!("  folders     {}", fmt_int(root.ndirs));
    println!("  unreadable  {}", errors.len());
    println!("  scan time   {:.2}s ({threads} threads)", scan_time.as_secs_f64());
    println!("  tree memory {}", human(tree.heap_bytes() as u64));
    if let Some(rss) = peak_rss() {
        println!("  peak RSS    {}", human(rss));
    }

    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let t1 = Instant::now();
    let st = stats::compute(&tree, Tree::ROOT, ondisk, top_n, true, now);
    println!("\nBiggest files (stats in {:.2}s):", t1.elapsed().as_secs_f64());
    for r in &st.top {
        let d = tree.get(r.dir);
        let f = &d.files[r.idx as usize];
        println!("  {:>9}  {}", human(f.value(ondisk)), tree.file_path(r.dir, f).display());
    }

    let mut children: Vec<u32> = root.dirs.clone();
    children.sort_by_key(|&c| std::cmp::Reverse(tree.get(c).value(ondisk)));
    println!("\nBiggest folders:");
    for c in children.iter().take(10) {
        let d = tree.get(*c);
        println!("  {:>9}  {}", human(d.value(ondisk)), d.name_str());
    }
    for (p, e) in errors.iter().take(10) {
        eprintln!("unreadable: {}: {e}", p.display());
    }
    ExitCode::SUCCESS
}

fn peak_rss() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = s.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}
