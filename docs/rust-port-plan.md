# DiskLens → Rust: port plan

Status: **phase 0 done** (2026-11-03): engine + `disklens-scan` CLI in `disklens-rs/`. Decisions: plain GTK, Python frozen during the port, remote scan after parity. Python v1.1 (`disklens.py`) is the reference; its behaviour is the spec.

## Goal
Same app, same look, same features: a folder tree and a sortable contents list showing file and folder sizes, plus Biggest Files, File Types, File Ages, Treemap and Duplicates. It ships as one fast native binary. The Python version stays in git until the Rust one reaches parity.

## Why it pays off
| | Python v1.1 | Rust (expected) |
|---|---|---|
| Memory per file | ~300 bytes (tuple + str) | ~40 bytes (arena) → about 1/6 to 1/8 |
| Home scan (947k files) | 10.8 s | I/O-bound: roughly 1.5–3× faster when the cache is warm, about the same when cold |
| UI with millions of rows | the GIL competes with scan threads | scan threads never block the UI |
| Duplicate hashing | blake2b, GIL-bound | BLAKE3 (SIMD), truly parallel |
| Distribution | python3 + PyGObject | one binary in `~/.local/bin` |

## Stack
- **gtk4-rs 0.11** (GTK 4.22 installed; `rustc` 1.93 / `cargo` from the distro already present). Plain GTK, like today: no libadwaita, so no sudo installs needed.
- cairo-rs + pangocairo for the treemap.
- Scanner: our own work-stealing pool (`crossbeam-deque`, or `rayon::scope`) over `std::fs::read_dir` and `symlink_metadata`. Results go to the UI over `async-channel`, with `glib::timeout_add_local` ticking every 500 ms, as in Python.
- `blake3` for duplicate hashing, `serde`/`serde_json` for settings. Settings stay in the same `~/.config/disklens/settings.json`, so both versions share them.
- `gio::File::trash`, `gio::AppInfo::launch_default_for_uri`, and D-Bus `FileManager1.ShowItems` for the right-click actions.

## Architecture
```
disklens-rs/              (inside ~/claude, same repo)
  Cargo.toml              workspace
  crates/core/            no GTK: scan, tree model, stats, duplicates, squarify
  crates/gui/             gtk4-rs app: window, views, treemap, dialogs
  crates/cli/  (later)    `disklens-scan PATH --json`, the headless engine
```
- **Splitting core from GUI** keeps the engine testable without a display. It also makes the v2 remote-scan idea cheap: run `disklens-scan` on m3 or arjang3 next to the NAS, stream the result over SSH, and browse it locally. That's much faster than NFS. It needs arm64 builds for the Pis, which cross-compile easily because core has no GTK.
- **Data model:** an arena `Vec<Dir>` with `u32` indices instead of pointers. Each `Dir` holds `parent`, `name` (interned in one big `String` buffer), `dirs: Vec<u32>`, `files: Range` into a `Vec<File {name_off, size, alloc, mtime}>`, and subtree totals as `AtomicU64`. A folder that finishes scanning adds its totals up the parent chain with atomics, so there's no global lock (Python uses one).
- **List rows:** a `glib::Object` subclass `RowObject { dir_or_file index }` with the same properties as the Python `Row` (name, size_text, fraction, pct_text, …). The model chain mirrors Python 1:1: `TreeListModel → SortListModel(TreeListRowSorter) → SingleSelection → ColumnView`.

## Phases (each ends with something runnable and a check)
| # | Delivers | Done when |
|---|---|---|
| 0 | Cargo workspace, `core` scan engine, and a temporary CLI that prints totals and the 20 biggest files | Totals match `du -sb --apparent-size` and the Python engine on `~/claude`, `~/Documents` and `~`; benchmark numbers recorded |
| 1 | Window: headerbar, start screen with drives, tree + Contents, sorting on every column, live updates while scanning, breadcrumb, status bar | Browse home like the Python version; screenshot comparison |
| 2 | Biggest Files, File Types (same extension rules, including grouped `.r00–.r99`), File Ages, Treemap (recursive, click to drill in, tooltips) | Same numbers as Python on the same folder |
| 3 | Right-click menu (open, reveal, terminal, copy path, go to, rescan, Trash, with permanent delete only offered where Trash is unsupported), CSV export, hidden-files / size-on-disk / one-filesystem toggles, settings, keyboard shortcuts | Feature checklist vs Python ticks off |
| 4 | Duplicates: size → hard-link collapse → BLAKE3 of head and tail 64 KB → full BLAKE3; marks, keep-one guard, progress, stop | The 5-case test set (copies, middle-differs, same-size-different, hard link, small files) gives identical results to Python |
| 5 | `cargo build --release`, install to `~/.local/bin/disklens`, point the `.desktop` launcher at it, README | Parity confirmed by you; Python version kept in git as a fallback |

## Phase 0 results (2026-11-03, ~, 953k files, warm cache)
| | Python v1.1 engine | Rust engine |
|---|---|---|
| Scan time | 9.91 s | **0.38 s** (26×) |
| Peak memory | 359 MB | **64 MB** (5.6×), tree itself 57 MB |
| Biggest-files stats | — | 0.05 s |
| Totals | match each other and `du -sb --apparent-size` (±live log growth) | |
Cold-cache (first scan after boot) not measured, because dropping the page cache needs sudo. There the disk is the limit, so expect a smaller gain.
8 unit/integration tests: totals, hard links counted once, 1 vs 32 threads identical, reset/rescan bookkeeping, unreadable folder, stats, `human`/`fmt_int`/`ext_of` incl. RAR-part grouping.

## Testing
- `core` gets unit tests for scan totals on a generated tree, the duplicate cases, `squarify`, `ext_of`, and age buckets.
- The GUI gets a hidden `--selftest` flag that runs the same scripted walk-through as today's Python harness (scan, open the biggest folder, visit each tab, toggle size-on-disk, rescan) and writes PNG screenshots. Same technique: `WidgetPaintable` + `render_texture`.

## Risks / notes
- gtk4-rs object subclassing is verbose: about 2.5–3.5k lines of Rust versus 2.2k lines of Python.
- Sorting 100k+ rows re-runs the `CustomSorter` comparator. Precompute sort keys on `RowObject` to keep it fast.
- Hard links are handled as in Python: counted once in totals, collapsed in duplicate search.
- The distro `rustc` 1.93 meets gtk4-rs 0.11's MSRV. If a crate needs newer, install rustup in user space (no sudo).

## Open decisions
1. Keep plain GTK (recommended, same look, no sudo) or switch to libadwaita (more modern GNOME look, needs `sudo apt install libadwaita-1-dev`)?
2. Freeze new features in the Python version during the port (recommended), or keep adding to both?
3. Build the headless `disklens-scan` (remote NAS scanning) as part of the port, or after parity (recommended)?
