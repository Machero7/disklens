//! The size tree: an arena of folders indexed by `u32`, files stored compactly per folder.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::scan::DirResult;

/// "No folder" (parent of the root).
pub const NONE: u32 = u32::MAX;

/// One file. Its name lives in the owning [`Dir::names`] buffer.
#[derive(Clone, Copy, Debug)]
pub struct FileEntry {
    pub name_off: u32,
    pub name_len: u16,
    /// false for the 2nd+ name of a hard-linked inode: listed, but not added to totals.
    pub counted: bool,
    pub size: u64,
    pub alloc: u64,
    pub mtime: i64,
}

impl FileEntry {
    pub fn value(&self, ondisk: bool) -> u64 {
        if ondisk { self.alloc } else { self.size }
    }
}

/// One folder. `size`, `alloc`, `nfiles`, `ndirs` and `mtime` cover the whole subtree.
#[derive(Debug)]
pub struct Dir {
    pub parent: u32,
    pub name: Box<[u8]>,
    pub dirs: Vec<u32>,
    pub files: Vec<FileEntry>,
    pub names: Vec<u8>,
    pub size: u64,
    pub alloc: u64,
    pub nfiles: u64,
    pub ndirs: u64,
    /// Newest modification time in the subtree (seconds since the epoch).
    pub mtime: i64,
    pub scanned: bool,
    pub error: Option<Box<str>>,
}

impl Default for Dir {
    fn default() -> Self {
        Dir {
            parent: NONE,
            name: Box::default(),
            dirs: Vec::new(),
            files: Vec::new(),
            names: Vec::new(),
            size: 0,
            alloc: 0,
            nfiles: 0,
            ndirs: 0,
            mtime: 0,
            scanned: false,
            error: None,
        }
    }
}

impl Dir {
    pub fn value(&self, ondisk: bool) -> u64 {
        if ondisk { self.alloc } else { self.size }
    }

    pub fn file_name_bytes(&self, f: &FileEntry) -> &[u8] {
        let start = f.name_off as usize;
        &self.names[start..start + f.name_len as usize]
    }

    pub fn file_name(&self, f: &FileEntry) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(self.file_name_bytes(f))
    }

    pub fn name_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.name)
    }
}

pub struct Tree {
    pub dirs: Vec<Dir>,
}

impl Tree {
    /// A tree whose root (id 0) is `path`. The root's name is the full path.
    pub fn new(path: &Path) -> Self {
        let root = Dir {
            name: path.as_os_str().as_bytes().into(),
            ..Dir::default()
        };
        Tree { dirs: vec![root] }
    }

    pub const ROOT: u32 = 0;

    pub fn get(&self, id: u32) -> &Dir {
        &self.dirs[id as usize]
    }

    /// Next free id, for starting a scanner on this tree.
    pub fn next_id(&self) -> u32 {
        self.dirs.len() as u32
    }

    pub fn path(&self, id: u32) -> PathBuf {
        let mut parts = Vec::new();
        let mut n = id;
        while n != NONE {
            let d = self.get(n);
            parts.push(&*d.name);
            n = d.parent;
        }
        let mut p = PathBuf::new();
        for part in parts.iter().rev() {
            p.push(OsStr::from_bytes(part));
        }
        p
    }

    pub fn file_path(&self, dir: u32, f: &FileEntry) -> PathBuf {
        let mut p = self.path(dir);
        p.push(OsStr::from_bytes(self.get(dir).file_name_bytes(f)));
        p
    }

    pub fn is_inside(&self, id: u32, ancestor: u32) -> bool {
        let mut n = id;
        while n != NONE {
            if n == ancestor {
                return true;
            }
            n = self.get(n).parent;
        }
        false
    }

    /// Integrate one scanned folder and add its files to every ancestor's totals.
    pub fn apply(&mut self, r: DirResult) {
        if let Some(max) = r.subdirs.iter().map(|s| s.0).max() {
            if max as usize >= self.dirs.len() {
                self.dirs.resize_with(max as usize + 1, Dir::default);
            }
        }
        for (cid, name, mtime) in r.subdirs.iter() {
            let c = &mut self.dirs[*cid as usize];
            c.parent = r.id;
            c.name = name.clone();
            c.mtime = *mtime;
        }

        let (mut size, mut alloc, mut newest) = (0u64, 0u64, 0i64);
        for f in r.files.iter().filter(|f| f.counted) {
            size += f.size;
            alloc += f.alloc;
            newest = newest.max(f.mtime);
        }
        let nfiles = r.files.len() as u64;
        let ndirs = r.subdirs.len() as u64;

        let d = &mut self.dirs[r.id as usize];
        d.dirs.extend(r.subdirs.iter().map(|s| s.0));
        d.files = r.files;
        d.names = r.names;
        d.scanned = true;
        d.error = r.error;

        let mut p = r.id;
        while p != NONE {
            let d = &mut self.dirs[p as usize];
            d.size += size;
            d.alloc += alloc;
            d.nfiles += nfiles;
            d.ndirs += ndirs;
            d.mtime = d.mtime.max(newest);
            p = d.parent;
        }
    }

    /// Subtract a folder's totals from its ancestors and empty it, ready for a rescan.
    pub fn reset(&mut self, id: u32) {
        let (size, alloc, nfiles, ndirs) = {
            let d = self.get(id);
            (d.size, d.alloc, d.nfiles, d.ndirs)
        };
        let mut p = self.get(id).parent;
        while p != NONE {
            let d = &mut self.dirs[p as usize];
            d.size -= size;
            d.alloc -= alloc;
            d.nfiles -= nfiles;
            d.ndirs -= ndirs;
            p = d.parent;
        }
        let d = &mut self.dirs[id as usize];
        d.dirs.clear();
        d.files.clear();
        d.names.clear();
        d.size = 0;
        d.alloc = 0;
        d.nfiles = 0;
        d.ndirs = 0;
        d.scanned = false;
        d.error = None;
    }

    /// Approximate heap memory held by the tree, in bytes.
    pub fn heap_bytes(&self) -> usize {
        let mut n = self.dirs.capacity() * std::mem::size_of::<Dir>();
        for d in &self.dirs {
            n += d.name.len()
                + d.dirs.capacity() * 4
                + d.files.capacity() * std::mem::size_of::<FileEntry>()
                + d.names.capacity();
        }
        n
    }
}
