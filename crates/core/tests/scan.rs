use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use disklens_core::scan::{scan_blocking, ScanOptions};
use disklens_core::{stats, Tree};

fn write(path: &Path, len: usize) {
    let mut f = fs::File::create(path).unwrap();
    f.write_all(&vec![7u8; len]).unwrap();
}

/// a/ (1000 + 2000 + hardlink of the 2000)   b/ (300)   b/c/ (50000)   d/ (empty)   top.txt (10)
fn fixture() -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    for d in ["a", "b/c", "d"] {
        fs::create_dir_all(r.join(d)).unwrap();
    }
    write(&r.join("a/one"), 1000);
    write(&r.join("a/two"), 2000);
    fs::hard_link(r.join("a/two"), r.join("a/two-link")).unwrap();
    write(&r.join("b/three"), 300);
    write(&r.join("b/c/big.bin"), 50_000);
    write(&r.join("top.txt"), 10);
    t
}

fn child(tree: &Tree, parent: u32, name: &str) -> u32 {
    *tree.get(parent).dirs.iter().find(|&&c| &*tree.get(c).name == name.as_bytes()).unwrap()
}

#[test]
fn totals_match_and_hardlinks_count_once() {
    let t = fixture();
    let (tree, errors) = scan_blocking(t.path(), ScanOptions::default()).unwrap();
    assert!(errors.is_empty());
    let root = tree.get(Tree::ROOT);
    assert_eq!(root.size, 1000 + 2000 + 300 + 50_000 + 10);
    assert_eq!(root.nfiles, 6, "the hard link is listed as a file");
    assert_eq!(root.ndirs, 4);
    let a = child(&tree, Tree::ROOT, "a");
    assert_eq!(tree.get(a).size, 3000);
    let b = child(&tree, Tree::ROOT, "b");
    assert_eq!(tree.get(b).size, 50_300);
    let c = child(&tree, b, "c");
    assert_eq!(tree.path(c), t.path().join("b/c"));
    assert_eq!(tree.get(child(&tree, Tree::ROOT, "d")).size, 0);
    // allocated size is block-based and at least the apparent size for these files
    assert!(root.alloc >= 50_000);
}

#[test]
fn single_thread_gives_same_result() {
    let t = fixture();
    let (a, _) = scan_blocking(t.path(), ScanOptions { threads: 1, ..Default::default() }).unwrap();
    let (b, _) = scan_blocking(t.path(), ScanOptions { threads: 32, ..Default::default() }).unwrap();
    let (ra, rb) = (a.get(Tree::ROOT), b.get(Tree::ROOT));
    assert_eq!((ra.size, ra.alloc, ra.nfiles, ra.ndirs), (rb.size, rb.alloc, rb.nfiles, rb.ndirs));
}

#[test]
fn reset_subtracts_from_ancestors() {
    let t = fixture();
    let (mut tree, _) = scan_blocking(t.path(), ScanOptions::default()).unwrap();
    let b = child(&tree, Tree::ROOT, "b");
    tree.reset(b);
    let root = tree.get(Tree::ROOT);
    assert_eq!(root.size, 3010);
    assert_eq!(root.nfiles, 4);
    assert_eq!(root.ndirs, 4 - 1, "b's subfolder c is gone, b itself stays");
}

#[test]
fn unreadable_folder_is_reported() {
    if fs::metadata("/").unwrap().uid() == unsafe_getuid() {
        return; // running as root: permissions don't apply
    }
    let t = fixture();
    let locked = t.path().join("locked");
    fs::create_dir(&locked).unwrap();
    write(&locked.join("secret"), 99);
    fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
    let (tree, errors) = scan_blocking(t.path(), ScanOptions::default()).unwrap();
    fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    assert_eq!(errors.len(), 1);
    let l = child(&tree, Tree::ROOT, "locked");
    assert!(tree.get(l).error.is_some());
    assert_eq!(tree.get(Tree::ROOT).size, 53_310);
}

fn unsafe_getuid() -> u32 {
    // uid of this process, via /proc to avoid a libc dependency
    let s = fs::read_to_string("/proc/self/status").unwrap();
    s.lines().find(|l| l.starts_with("Uid:")).unwrap().split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[test]
fn stats_top_exts_ages() {
    let t = fixture();
    let (tree, _) = scan_blocking(t.path(), ScanOptions::default()).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let st = stats::compute(&tree, Tree::ROOT, false, 3, true, now);
    let names: Vec<String> = st
        .top
        .iter()
        .map(|r| tree.get(r.dir).file_name(&tree.get(r.dir).files[r.idx as usize]).into_owned())
        .collect();
    assert_eq!(names[0], "big.bin");
    assert_eq!(st.top.len(), 3);
    assert_eq!(st.exts[".bin"].size, 50_000);
    assert_eq!(st.exts[".txt"].count, 1);
    assert_eq!(st.ages[0].1, 6, "everything was just written");
}
