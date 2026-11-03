//! Mounted drives for the start page and the drives menu.

use std::ffi::CString;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const REAL_FS: &[&str] = &[
    "ext2", "ext3", "ext4", "btrfs", "xfs", "zfs", "f2fs", "bcachefs", "jfs", "reiserfs", "vfat",
    "exfat", "ntfs", "ntfs3", "fuseblk", "nfs", "nfs4", "cifs", "smb3", "fuse.sshfs", "fuse.rclone",
];

#[derive(Clone, Debug)]
pub struct Mount {
    pub path: String,
    pub fstype: String,
    pub total: u64,
    pub free: u64,
}

impl Mount {
    pub fn is_network(&self) -> bool {
        ["nfs", "cifs", "smb", "fuse."].iter().any(|p| self.fstype.starts_with(p))
    }
}

fn statvfs(path: &str) -> Option<(u64, u64)> {
    let c = CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize as u64;
    Some((st.f_blocks as u64 * frsize, st.f_bavail as u64 * frsize))
}

/// Real filesystems with their size. A mount that doesn't answer within 1.5 s
/// (a dead network share) is left out rather than freezing the UI.
pub fn list() -> Vec<Mount> {
    let Ok(text) = std::fs::read_to_string("/proc/self/mounts") else { return Vec::new() };
    let mut seen = std::collections::HashSet::new();
    let mut wanted = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (_, Some(mnt), Some(fs)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let mnt = mnt.replace("\\040", " ");
        if !REAL_FS.contains(&fs) || mnt.starts_with("/snap/") || mnt.starts_with("/var/snap/") {
            continue;
        }
        if seen.insert(mnt.clone()) {
            wanted.push((mnt, fs.to_string()));
        }
    }
    let (tx, rx) = mpsc::channel();
    for (i, (mnt, _)) in wanted.iter().enumerate() {
        let (tx, mnt) = (tx.clone(), mnt.clone());
        std::thread::spawn(move || {
            let _ = tx.send((i, statvfs(&mnt)));
        });
    }
    drop(tx);
    let deadline = Instant::now() + Duration::from_millis(1500);
    let mut sizes = vec![None; wanted.len()];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok((i, s)) => sizes[i] = s,
            Err(_) => break,
        }
    }
    wanted
        .into_iter()
        .zip(sizes)
        .filter_map(|((path, fstype), s)| s.map(|(total, free)| Mount { path, fstype, total, free }))
        .collect()
}
