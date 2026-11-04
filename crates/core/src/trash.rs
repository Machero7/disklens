//! Restoring items from the freedesktop.org Trash (the layout GIO's `g_file_trash` writes):
//! `<trash>/files/<name>` holds the item and `<trash>/info/<name>.trashinfo` records its
//! original `Path=` (percent-encoded; relative to the drive's top folder for per-drive trashes).
//! Implemented directly on the files so it works without a GVfs trash service.

use std::fs;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

/// A trash folder, and for per-drive trashes the folder its relative paths start from.
#[derive(Debug, Clone)]
pub struct TrashDir {
    pub dir: PathBuf,
    pub topdir: Option<PathBuf>,
}

/// Every trash that might hold `orig`: the home trash, plus `.Trash-<uid>` and `.Trash/<uid>`
/// in each folder above it (that's where per-drive trashes live).
pub fn trash_dirs_for(orig: &Path) -> Vec<TrashDir> {
    let mut v = Vec::new();
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    if let Some(d) = data {
        v.push(TrashDir { dir: d.join("Trash"), topdir: None });
    }
    let uid = unsafe { libc_getuid() };
    for a in orig.ancestors().skip(1) {
        for dir in [a.join(format!(".Trash-{uid}")), a.join(".Trash").join(uid.to_string())] {
            if dir.join("info").is_dir() {
                v.push(TrashDir { dir, topdir: Some(a.to_path_buf()) });
            }
        }
    }
    v
}

extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

/// Decode `%XX` escapes (trashinfo paths are URI-escaped).
pub fn unescape(s: &str) -> PathBuf {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(v) = hex {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    PathBuf::from(std::ffi::OsString::from_vec(out))
}

/// Find the most recently trashed copy of `orig` and move it back. Fails if nothing matches
/// or something already exists at `orig`.
pub fn restore(orig: &Path, trashes: &[TrashDir]) -> io::Result<()> {
    if fs::symlink_metadata(orig).is_ok() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} already exists", orig.display())));
    }
    // (deletion date, trash dir, entry name)
    let mut best: Option<(String, &TrashDir, String)> = None;
    for t in trashes {
        let Ok(rd) = fs::read_dir(t.dir.join("info")) else { continue };
        for e in rd.flatten() {
            let fname = e.file_name().to_string_lossy().into_owned();
            let Some(name) = fname.strip_suffix(".trashinfo") else { continue };
            let Ok(text) = fs::read_to_string(e.path()) else { continue };
            let (mut path, mut date) = (None, String::new());
            for line in text.lines() {
                if let Some(p) = line.strip_prefix("Path=") {
                    path = Some(unescape(p));
                } else if let Some(d) = line.strip_prefix("DeletionDate=") {
                    date = d.to_string();
                }
            }
            let Some(p) = path else { continue };
            let full = match (&t.topdir, p.is_absolute()) {
                (Some(top), false) => top.join(p),
                _ => p,
            };
            if full == orig && best.as_ref().map_or(true, |b| date > b.0) {
                best = Some((date, t, name.to_string()));
            }
        }
    }
    let Some((_, t, name)) = best else {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("{} isn't in the Trash", orig.display())));
    };
    fs::rename(t.dir.join("files").join(&name), orig)?;
    let _ = fs::remove_file(t.dir.join("info").join(format!("{name}.trashinfo")));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_percent() {
        assert_eq!(unescape("/home/a%20b/%C3%A5.txt"), PathBuf::from("/home/a b/å.txt"));
        assert_eq!(unescape("/x/100%"), PathBuf::from("/x/100%"));
        assert_eq!(unescape("/x/%zz%å"), PathBuf::from("/x/%zz%å"));
    }

    #[test]
    fn restores_newest_match() {
        let t = tempfile::tempdir().unwrap();
        let trash = t.path().join("Trash");
        fs::create_dir_all(trash.join("files")).unwrap();
        fs::create_dir_all(trash.join("info")).unwrap();
        let orig = t.path().join("my file.txt");
        for (name, date, body) in [("my file.txt", "2026-01-01T10:00:00", "old"), ("my file.2.txt", "2026-06-01T10:00:00", "new")] {
            fs::write(trash.join("files").join(name), body).unwrap();
            fs::write(
                trash.join("info").join(format!("{name}.trashinfo")),
                format!("[Trash Info]\nPath={}\nDeletionDate={date}\n", orig.display().to_string().replace(' ', "%20")),
            )
            .unwrap();
        }
        let dirs = [TrashDir { dir: trash.clone(), topdir: None }];
        restore(&orig, &dirs).unwrap();
        assert_eq!(fs::read_to_string(&orig).unwrap(), "new");
        assert!(!trash.join("info/my file.2.txt.trashinfo").exists());
        // the older copy is still there; restoring again fails because the file exists
        assert!(restore(&orig, &dirs).is_err());
    }

    #[test]
    fn relative_paths_in_drive_trash() {
        let t = tempfile::tempdir().unwrap();
        let trash = t.path().join(".Trash-1000");
        fs::create_dir_all(trash.join("files/photos")).unwrap();
        fs::create_dir_all(trash.join("info")).unwrap();
        fs::write(trash.join("info/photos.trashinfo"), "[Trash Info]\nPath=photos\nDeletionDate=2026-01-01T00:00:00\n").unwrap();
        let orig = t.path().join("photos");
        restore(&orig, &[TrashDir { dir: trash, topdir: Some(t.path().to_path_buf()) }]).unwrap();
        assert!(orig.is_dir());
    }
}
