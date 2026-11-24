//! The five cases from the port plan: copies, differs only in the middle, same size but
//! different, hard link, and files below the minimum size.

use std::fs;
use std::path::Path;

use disklens_core::dups::{self, Progress, PARTIAL};
use disklens_core::scan::{scan_blocking, ScanOptions};
use disklens_core::Tree;

fn names(tree: &Tree, g: &dups::Group) -> Vec<String> {
    let mut v: Vec<String> = g
        .members
        .iter()
        .map(|r| {
            let d = tree.get(r.dir);
            let rel = tree.file_path(r.dir, &d.files[r.idx as usize]);
            rel.strip_prefix(tree.path(Tree::ROOT)).unwrap().display().to_string()
        })
        .collect();
    v.sort();
    v
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

fn fixture(r: &Path) {
    fs::create_dir_all(r.join("a/deep")).unwrap();
    fs::create_dir_all(r.join("b")).unwrap();
    let big = 3 * PARTIAL as usize + 123;
    // 1. three copies of a big file
    let data = pattern(big, 1);
    for p in ["a/copy1.bin", "b/copy2.bin", "a/deep/copy3.bin"] {
        fs::write(r.join(p), &data).unwrap();
    }
    // 2. same head and tail, different middle (only the full hash can tell)
    let mut mid = data.clone();
    mid[big / 2] ^= 0xff;
    fs::write(r.join("b/middle.bin"), &mid).unwrap();
    // 3. same size as the small pair below, different contents
    fs::write(r.join("a/small1.txt"), pattern(5000, 7)).unwrap();
    fs::write(r.join("b/small2.txt"), pattern(5000, 7)).unwrap();
    fs::write(r.join("b/small-other.txt"), pattern(5000, 9)).unwrap();
    // 4. a hard link to copy1 is the same file, not a duplicate
    fs::hard_link(r.join("a/copy1.bin"), r.join("b/copy1-link.bin")).unwrap();
    // 5. tiny identical files, below the minimum size
    fs::write(r.join("a/tiny1"), b"hello").unwrap();
    fs::write(r.join("b/tiny2"), b"hello").unwrap();
    // an empty pair is never reported
    fs::write(r.join("a/empty1"), b"").unwrap();
    fs::write(r.join("b/empty2"), b"").unwrap();
}

#[test]
fn five_cases() {
    let t = tempfile::tempdir().unwrap();
    fixture(t.path());
    let (tree, _) = scan_blocking(t.path(), ScanOptions::default()).unwrap();
    let p = Progress::default();
    let groups = dups::find(dups::collect(&tree, Tree::ROOT, 100, true), 4, &p);
    let got: Vec<Vec<String>> = groups.iter().map(|g| names(&tree, g)).collect();
    // exactly one hard-link name is kept, whichever the scan saw first
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0].len(), 3, "three copies, link collapsed: {got:?}");
    assert!(got[0].contains(&"b/copy2.bin".to_string()) && got[0].contains(&"a/deep/copy3.bin".to_string()));
    assert!(!got[0].iter().any(|n| n == "b/middle.bin"));
    assert_eq!(got[1], vec!["a/small1.txt", "b/small2.txt"]);
    assert_eq!(groups[0].reclaimable(), 2 * (3 * PARTIAL + 123));
    assert_eq!(p.errors.load(std::sync::atomic::Ordering::Relaxed), 0);

    // with no minimum, the tiny pair shows up too (empty files never do)
    let all = dups::find(dups::collect(&tree, Tree::ROOT, 0, true), 2, &Progress::default());
    assert_eq!(all.len(), 3);
}

#[test]
fn cancel_returns_nothing() {
    let t = tempfile::tempdir().unwrap();
    fixture(t.path());
    let (tree, _) = scan_blocking(t.path(), ScanOptions::default()).unwrap();
    let p = Progress::default();
    p.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(dups::find(dups::collect(&tree, Tree::ROOT, 0, true), 2, &p).is_empty());
}

/// The NAS finder (run on this machine through `sh`, as the SSH command would be on the NAS)
/// must find exactly the same groups as the local one, including awkward file names.
#[test]
fn remote_finder_matches_local() {
    let t = tempfile::tempdir().unwrap();
    fixture(t.path());
    let r = t.path();
    // names with spaces, quotes, a newline and a leading dash
    let odd = pattern(200_000, 42);
    fs::create_dir_all(r.join("odd dir")).unwrap();
    for name in ["odd dir/it's \"quoted\".bin", "odd dir/new\nline.bin", "odd dir/-dash.bin"] {
        fs::write(r.join(name), &odd).unwrap();
    }
    let (tree, _) = scan_blocking(r, ScanOptions::default()).unwrap();
    let local = dups::find(dups::collect(&tree, Tree::ROOT, 0, true), 4, &Progress::default());
    let p = Progress::default();
    let remote = disklens_core::remote_dups::find(None, dups::collect(&tree, Tree::ROOT, 0, true), &p).unwrap();
    let show = |gs: &[dups::Group]| -> Vec<(u64, Vec<String>)> { gs.iter().map(|g| (g.size, names(&tree, g))).collect() };
    assert_eq!(show(&remote), show(&local));
    assert_eq!(remote.len(), 4, "copies, small pair, tiny pair, odd names");
    assert_eq!(p.errors.load(std::sync::atomic::Ordering::Relaxed), 0);

    // stopping returns nothing
    let p = Progress::default();
    p.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(disklens_core::remote_dups::find(None, dups::collect(&tree, Tree::ROOT, 0, true), &p).unwrap().is_empty());
}
