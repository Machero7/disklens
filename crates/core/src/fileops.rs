//! Copying and moving files and folders into a folder (the Paste in Cut/Copy/Paste).
//!
//! Nothing is ever overwritten: when the name is taken, the item gets a free name such as
//! `report (2).pdf`. A move is a rename when source and destination are on the same drive;
//! across drives the item is copied first and the original deleted only after the copy worked.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// What a paste did with one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pasted {
    pub from: PathBuf,
    pub to: PathBuf,
    /// The original name was taken, so `to` has a different one.
    pub renamed: bool,
}

/// `name` if it's free in `dir`, otherwise `stem (2).ext`, `stem (3).ext`, … The extension
/// stays at the end; folders and dot-files keep their whole name as the stem.
pub fn free_name(dir: &Path, name: &std::ffi::OsStr, is_dir: bool) -> PathBuf {
    let first = dir.join(name);
    if fs::symlink_metadata(&first).is_err() {
        return first;
    }
    let s = name.to_string_lossy();
    let (stem, ext) = match s.rfind('.') {
        Some(i) if !is_dir && i > 0 => (&s[..i], &s[i..]),
        _ => (&s[..], ""),
    };
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| fs::symlink_metadata(p).is_err())
        .expect("some number is free")
}

/// Refuse to put a folder inside itself.
fn check_not_inside(src: &Path, dest_dir: &Path) -> io::Result<()> {
    let (src, dest) = (fs::canonicalize(src)?, fs::canonicalize(dest_dir)?);
    if dest.starts_with(&src) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "a folder can't go inside itself"));
    }
    Ok(())
}

/// Copy a file, folder (recursively) or symlink to `to`, keeping modification times.
pub fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let md = fs::symlink_metadata(from)?;
    let ft = md.file_type();
    if ft.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(from)?, to)?;
    } else if ft.is_dir() {
        fs::create_dir(to)?;
        for e in fs::read_dir(from)? {
            let e = e?;
            copy_tree(&e.path(), &to.join(e.file_name()))?;
        }
        fs::set_permissions(to, md.permissions())?;
        let _ = fs::File::open(to).and_then(|f| f.set_modified(md.modified()?));
    } else {
        fs::copy(from, to)?; // copies the permissions too
        let _ = fs::File::options().write(true).open(to).and_then(|f| f.set_modified(md.modified()?));
    }
    Ok(())
}

fn remove_tree(p: &Path) -> io::Result<()> {
    if fs::symlink_metadata(p)?.is_dir() {
        fs::remove_dir_all(p)
    } else {
        fs::remove_file(p)
    }
}

/// Copy `src` into the folder `dest_dir`.
pub fn copy_into(src: &Path, dest_dir: &Path) -> io::Result<Pasted> {
    let is_dir = fs::symlink_metadata(src)?.is_dir();
    if is_dir {
        check_not_inside(src, dest_dir)?;
    }
    let name = src.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let to = free_name(dest_dir, name, is_dir);
    if let Err(e) = copy_tree(src, &to) {
        let _ = remove_tree(&to); // don't leave half a copy behind
        return Err(e);
    }
    Ok(Pasted { from: src.to_path_buf(), renamed: to.file_name() != Some(name), to })
}

/// Move `src` into the folder `dest_dir`. Moving an item to the folder it's already in does nothing.
pub fn move_into(src: &Path, dest_dir: &Path) -> io::Result<Pasted> {
    let is_dir = fs::symlink_metadata(src)?.is_dir();
    let name = src.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    if let (Ok(a), Ok(b)) = (fs::canonicalize(src.parent().unwrap_or(src)), fs::canonicalize(dest_dir)) {
        if a == b {
            return Ok(Pasted { from: src.to_path_buf(), to: src.to_path_buf(), renamed: false });
        }
    }
    if is_dir {
        check_not_inside(src, dest_dir)?;
    }
    let to = free_name(dest_dir, name, is_dir);
    move_path(src, &to)?;
    Ok(Pasted { from: src.to_path_buf(), renamed: to.file_name() != Some(name), to })
}

/// Move `src` to exactly `to` (which must not exist): a rename on the same drive; on another
/// drive, copy and delete the original only once the copy is complete.
fn move_path(src: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(src, to) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(18) => {
            if let Err(e) = copy_tree(src, to) {
                let _ = remove_tree(to);
                return Err(e);
            }
            remove_tree(src)
        }
        Err(e) => Err(e),
    }
}

/// Undo a move: put `p.to` back at `p.from`. If something new took that name meanwhile,
/// it goes next to it as `name (2)` instead. Returns where it ended up.
pub fn undo_move(p: &Pasted) -> io::Result<PathBuf> {
    if p.from == p.to {
        return Ok(p.to.clone()); // it never moved
    }
    let is_dir = fs::symlink_metadata(&p.to)?.is_dir();
    let dir = p.from.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent folder"))?;
    if !dir.is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("{} no longer exists", dir.display())));
    }
    let name = p.from.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let back = free_name(dir, name, is_dir);
    move_path(&p.to, &back)?;
    Ok(back)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        fs::create_dir_all(a.join("album/disc 1")).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("report.pdf"), b"pdf").unwrap();
        fs::write(a.join("album/disc 1/01.flac"), b"music").unwrap();
        std::os::unix::fs::symlink("disc 1/01.flac", a.join("album/first")).unwrap();
        (t, a, b)
    }

    #[test]
    fn copy_file_and_folder() {
        let (_t, a, b) = setup();
        let p = copy_into(&a.join("report.pdf"), &b).unwrap();
        assert_eq!((p.to.clone(), p.renamed), (b.join("report.pdf"), false));
        assert_eq!(fs::read(&p.to).unwrap(), b"pdf");
        assert!(a.join("report.pdf").exists(), "copy keeps the original");

        let p = copy_into(&a.join("album"), &b).unwrap();
        assert_eq!(fs::read(p.to.join("disc 1/01.flac")).unwrap(), b"music");
        assert_eq!(fs::read_link(p.to.join("first")).unwrap(), PathBuf::from("disc 1/01.flac"), "symlinks stay links");
        let (m1, m2) = (fs::metadata(a.join("report.pdf")).unwrap().modified().unwrap(), fs::metadata(b.join("report.pdf")).unwrap().modified().unwrap());
        assert_eq!(m1, m2, "modification time kept");
    }

    #[test]
    fn never_overwrites() {
        let (_t, a, b) = setup();
        fs::write(b.join("report.pdf"), b"other").unwrap();
        let p = copy_into(&a.join("report.pdf"), &b).unwrap();
        assert_eq!((p.to.clone(), p.renamed), (b.join("report (2).pdf"), true));
        assert_eq!(fs::read(b.join("report.pdf")).unwrap(), b"other", "existing file untouched");
        let p = copy_into(&a.join("report.pdf"), &b).unwrap();
        assert_eq!(p.to, b.join("report (3).pdf"));
        // copying into the same folder makes a numbered copy
        let p = copy_into(&a.join("album"), &a).unwrap();
        assert_eq!(p.to, a.join("album (2)"));
        assert_eq!(free_name(&a, ".bashrc".as_ref(), false), a.join(".bashrc"));
    }

    #[test]
    fn move_file_and_folder() {
        let (_t, a, b) = setup();
        let p = move_into(&a.join("album"), &b).unwrap();
        assert_eq!(p.to, b.join("album"));
        assert!(!a.join("album").exists() && b.join("album/disc 1/01.flac").exists());
        // moving to where it already is changes nothing
        let p = move_into(&b.join("album"), &b).unwrap();
        assert_eq!(p.to, b.join("album"));
        assert!(b.join("album").exists() && !b.join("album (2)").exists());
    }

    #[test]
    fn undo_move_puts_things_back() {
        let (_t, a, b) = setup();
        let p = move_into(&a.join("album"), &b).unwrap();
        assert_eq!(undo_move(&p).unwrap(), a.join("album"));
        assert!(a.join("album/disc 1/01.flac").exists() && !b.join("album").exists());

        // something new took the name meanwhile: nothing is overwritten
        let p = move_into(&a.join("report.pdf"), &b).unwrap();
        fs::write(a.join("report.pdf"), b"new").unwrap();
        assert_eq!(undo_move(&p).unwrap(), a.join("report (2).pdf"));
        assert_eq!(fs::read(a.join("report.pdf")).unwrap(), b"new");
        assert_eq!(fs::read(a.join("report (2).pdf")).unwrap(), b"pdf");

        // the original folder is gone: a clear error, the item stays put
        let p = move_into(&a.join("report (2).pdf"), &b).unwrap();
        fs::remove_dir_all(&a).unwrap();
        assert!(undo_move(&p).is_err());
        assert!(p.to.exists());
    }

    #[test]
    fn refuses_folder_into_itself() {
        let (_t, a, _b) = setup();
        assert!(move_into(&a.join("album"), &a.join("album/disc 1")).is_err());
        assert!(copy_into(&a.join("album"), &a.join("album/disc 1")).is_err());
        assert!(a.join("album/disc 1/01.flac").exists(), "nothing touched");
        assert!(!a.join("album/disc 1/album").exists(), "no half copy left");
    }
}
