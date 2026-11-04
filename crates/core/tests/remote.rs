//! The SSH scanner's `find` pipeline, run locally (`host: None`), must build exactly the same
//! tree as the local scanner: same totals per folder, hard links counted once, odd names intact.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use disklens_core::remote::{self, RemoteSpec};
use disklens_core::scan::{scan_blocking, Msg, ScanOptions};
use disklens_core::Tree;

fn fixture(r: &Path) {
    for d in ["a/deep/deeper", "b", "empty", "c d/x\ty", "nl\nname/sub"] {
        fs::create_dir_all(r.join(d)).unwrap();
    }
    fs::write(r.join("top.txt"), vec![1u8; 10]).unwrap();
    fs::write(r.join("a/one"), vec![1u8; 1000]).unwrap();
    fs::write(r.join("a/deep/two"), vec![2u8; 2000]).unwrap();
    fs::hard_link(r.join("a/deep/two"), r.join("b/two-link")).unwrap();
    fs::write(r.join("a/deep/deeper/three"), vec![3u8; 30_000]).unwrap();
    fs::write(r.join("c d/x\ty/tab file"), vec![4u8; 77]).unwrap();
    fs::write(r.join("nl\nname/sub/new\nline"), vec![5u8; 5]).unwrap();
    std::os::unix::fs::symlink("a/one", r.join("link-to-one")).unwrap();
}

fn remote_tree(path: &Path) -> (Tree, Vec<String>) {
    let mut tree = Tree::new(path);
    let spec = RemoteSpec { host: None, path: path.display().to_string() };
    let s = remote::start(Tree::ROOT, tree.next_id(), spec, ScanOptions::default()).unwrap();
    let mut errors = Vec::new();
    for m in s.rx.iter() {
        match m {
            Msg::Dir(r) => tree.apply(r),
            Msg::Error(p, e) => errors.push(format!("{}: {e}", p.display())),
            Msg::Fatal(e) => errors.push(e),
            Msg::Done => break,
        }
    }
    (tree, errors)
}

/// path -> (size, alloc, files, folders, number of direct files, number of counted direct files)
fn summary(tree: &Tree) -> BTreeMap<String, (u64, u64, u64, u64, usize, usize)> {
    let root = tree.path(Tree::ROOT);
    let mut m = BTreeMap::new();
    let mut stack = vec![Tree::ROOT];
    while let Some(id) = stack.pop() {
        let d = tree.get(id);
        stack.extend_from_slice(&d.dirs);
        let rel = tree.path(id).strip_prefix(&root).unwrap().display().to_string();
        m.insert(rel, (d.size, d.alloc, d.nfiles, d.ndirs, d.files.len(), d.files.iter().filter(|f| f.counted).count()));
    }
    m
}

#[test]
fn same_tree_as_local_scan() {
    let t = tempfile::tempdir().unwrap();
    fixture(t.path());
    let (local, lerr) = scan_blocking(t.path(), ScanOptions::default()).unwrap();
    let (remote, rerr) = remote_tree(t.path());
    assert!(lerr.is_empty() && rerr.is_empty(), "{lerr:?} {rerr:?}");
    assert_eq!(summary(&remote), summary(&local));
    let r = remote.get(Tree::ROOT);
    assert_eq!(r.size, 10 + 1000 + 2000 + 30_000 + 77 + 5 + "a/one".len() as u64);
    assert!(remote.get(Tree::ROOT).dirs.iter().all(|&c| remote.get(c).scanned), "empty folders are marked scanned");
}

#[test]
fn missing_folder_is_a_clear_error() {
    let (tree, errors) = remote_tree(Path::new("/definitely/not/here"));
    assert_eq!(tree.get(Tree::ROOT).nfiles, 0);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("doesn't exist"), "{errors:?}");
}
