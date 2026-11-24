//! Duplicate finder for a folder on another machine (a NAS): the same three steps as
//! [`crate::dups`] (same size → same first and last 64 KB → same full contents), but the
//! checksums are computed on the NAS with its own `sha256sum`. DiskLens sends the candidate
//! paths (NUL-separated) over one `ssh` connection per step and gets back one checksum line
//! per file, in the same order. No file contents cross the network.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::dups::{Candidate, Group, Progress, PARTIAL};

/// Run by `xargs -0 sh -c '…' _` on the NAS: one line per file, `-` if it can't be read.
/// Must not contain a single quote.
const HEAD_TAIL: &str = r#"for f; do if [ -r "$f" ]; then { head -c 65536 "$f"; tail -c 65536 "$f"; } | sha256sum | cut -c1-64; else echo -; fi; done"#;
const FULL: &str = r#"for f; do if [ -r "$f" ]; then sha256sum < "$f" | cut -c1-64; else echo -; fi; done"#;

fn command(host: Option<&str>, script: &str) -> Command {
    let remote = format!("xargs -0 sh -c '{script}' _");
    match host {
        Some(h) => {
            let mut c = Command::new("ssh");
            c.args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "--", h, &remote]);
            c
        }
        None => {
            let mut c = Command::new("sh");
            c.args(["-c", &remote]);
            c
        }
    }
}

/// One checksum per candidate, in order (`None` = unreadable).
fn checksums(host: Option<&str>, script: &str, items: &[Candidate], count_bytes: bool, p: &Progress) -> Result<Vec<Option<String>>, String> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let mut child = command(host, script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Couldn't start ssh: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let child = Arc::new(Mutex::new(child));
    let done = Arc::new(AtomicBool::new(false));

    std::thread::scope(|s| {
        // feed the paths
        s.spawn(move || {
            use std::os::unix::ffi::OsStrExt;
            for c in items {
                if stdin.write_all(c.path.as_os_str().as_bytes()).is_err() || stdin.write_all(&[0]).is_err() {
                    break;
                }
            }
        });
        let err_text = s.spawn(move || {
            let mut t = String::new();
            let _ = stderr.read_to_string(&mut t);
            t
        });
        // Stop: kill ssh (a single big file can take minutes to checksum)
        let (watch_child, watch_done) = (child.clone(), done.clone());
        s.spawn(move || {
            while !watch_done.load(Ordering::Relaxed) {
                if p.cancelled() {
                    let _ = watch_child.lock().unwrap().kill();
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
        });

        let mut out = Vec::with_capacity(items.len());
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let line = line.trim();
            let i = out.len();
            if i >= items.len() {
                break;
            }
            let ok = line.len() == 64 && line.bytes().all(|b| b.is_ascii_hexdigit());
            if !ok {
                p.errors.fetch_add(1, Ordering::Relaxed);
            }
            out.push(ok.then(|| line.to_string()));
            p.done_files.fetch_add(1, Ordering::Relaxed);
            if count_bytes {
                p.done_bytes.fetch_add(items[i].size, Ordering::Relaxed);
            }
        }
        done.store(true, Ordering::Relaxed);
        let status = child.lock().unwrap().wait();
        let err = err_text.join().unwrap_or_default();
        if p.cancelled() {
            return Ok(Vec::new());
        }
        if out.len() != items.len() {
            let first: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).take(3).collect();
            let why = if first.is_empty() {
                match status {
                    Ok(s) if !s.success() => format!("the command on the NAS stopped early ({s})"),
                    _ => "the NAS answered for fewer files than were asked".to_string(),
                }
            } else {
                first.join("\n")
            };
            return Err(format!(
                "Couldn't compare files on {}.\n\n{why}\n\nIt needs sha256sum, head, tail and xargs on the NAS.",
                host.unwrap_or("this machine")
            ));
        }
        // every line is "-": sha256sum is probably missing
        if out.iter().all(Option::is_none) && err.contains("sha256sum") {
            return Err(format!("sha256sum isn't available on {}.", host.unwrap_or("this machine")));
        }
        Ok(out)
    })
}

/// Split every group by checksum; keep sub-groups of 2+.
fn regroup(groups: Vec<Vec<Candidate>>, host: Option<&str>, script: &str, count_bytes: bool, p: &Progress) -> Result<Vec<Vec<Candidate>>, String> {
    let items: Vec<(usize, Candidate)> = groups.into_iter().enumerate().flat_map(|(g, v)| v.into_iter().map(move |c| (g, c))).collect();
    p.total_files.store(items.len() as u64, Ordering::Relaxed);
    p.done_files.store(0, Ordering::Relaxed);
    let cands: Vec<Candidate> = items.iter().map(|(_, c)| c.clone()).collect();
    let keys = checksums(host, script, &cands, count_bytes, p)?;
    let mut out: HashMap<(usize, String), Vec<Candidate>> = HashMap::new();
    for ((g, c), k) in items.into_iter().zip(keys) {
        if let Some(k) = k {
            out.entry((g, k)).or_default().push(c);
        }
    }
    Ok(out.into_values().filter(|v| v.len() > 1).collect())
}

/// Like [`crate::dups::find`], with the reading done on `host` (`None` = this machine, for tests).
/// `cands` hold paths as they are on that machine. Returns an empty list if cancelled.
pub fn find(host: Option<&str>, cands: Vec<Candidate>, p: &Progress) -> Result<Vec<Group>, String> {
    p.phase.store(0, Ordering::Relaxed);
    let mut by_size: HashMap<u64, Vec<Candidate>> = HashMap::new();
    for c in cands {
        by_size.entry(c.size).or_default().push(c);
    }
    let groups: Vec<Vec<Candidate>> = by_size.into_values().filter(|v| v.len() > 1).collect();

    p.phase.store(1, Ordering::Relaxed);
    let groups = regroup(groups, host, HEAD_TAIL, false, p)?;
    if p.cancelled() {
        return Ok(Vec::new());
    }

    p.phase.store(2, Ordering::Relaxed);
    // files of up to 128 KB were covered completely by the first and last 64 KB
    let (small, big): (Vec<_>, Vec<_>) = groups.into_iter().partition(|g| g[0].size <= 2 * PARTIAL);
    p.total_bytes.store(big.iter().flatten().map(|c| c.size).sum(), Ordering::Relaxed);
    p.done_bytes.store(0, Ordering::Relaxed);
    let big = regroup(big, host, FULL, true, p)?;
    if p.cancelled() {
        return Ok(Vec::new());
    }

    let mut out: Vec<Group> = small
        .into_iter()
        .chain(big)
        .map(|g| {
            let mut members: Vec<_> = g.iter().map(|c| c.r).collect();
            members.sort();
            Group { size: g[0].size, members }
        })
        .collect();
    out.sort_by(|a, b| b.reclaimable().cmp(&a.reclaimable()).then_with(|| a.members.cmp(&b.members)));
    Ok(out)
}
