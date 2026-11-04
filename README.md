# DiskLens (Rust)

See what's using your disk space. A GNOME app (GTK 4 + libadwaita) and the Rust rewrite
of `../disklens.py`: one native program, about 26× faster to scan and using about 1/6 of the memory.

## Install

Needs Rust plus the GTK 4 and libadwaita development files
(Fedora: `sudo dnf install gtk4-devel libadwaita-devel`).

```bash
./install.sh              # build, install to ~/.local/bin, add "DiskLens" to the app grid
./install.sh --uninstall  # remove it and restore the old launchers
```

The installer moves older DiskLens launchers (the Python one and "DiskLens Preview") to
`~/.local/share/disklens-old-launchers/`. The Python version still works with
`python3 ~/claude/disklens.py`.

## What it does

| Tab | Shows |
|---|---|
| Folder | Sizes of the folders and files in the current folder, coloured usage bar |
| Biggest | The 1000 largest files under the current folder |
| Types | Space per file type |
| Ages | Space by last-modified date |
| Map | Treemap: each tile's area is its size; click a folder to open it |
| Copies | Files with identical contents; mark extra copies and move them to the Trash |

Right-click any file or folder: Open, Show in File Manager, Open Terminal Here, Copy Path,
Go to Folder, Rescan, Move to Trash. Trash can be undone (toast button or Ctrl+Z).

Shortcuts: Ctrl+O scan · F5 rescan · Esc stop · Alt+Up parent · Delete trash ·
Ctrl+Z undo · Ctrl+E export CSV · Ctrl+, preferences · Ctrl+? all shortcuts.

Settings live in `~/.config/disklens/settings.json` (shared with the Python version).

## Develop

```
crates/core   scan engine, size tree, stats, duplicate finder, trash restore (no GTK)
crates/cli    disklens-scan: headless scan, prints totals and biggest files
crates/gui    the app
```

```bash
cargo test                                         # engine tests
cargo run --release -p disklens -- ~/Downloads     # run the app
target/release/disklens --shots /tmp/shots PATH    # scripted walk-through that saves screenshots
```

The `--shots` walk-through creates a small `disklens-walkthrough-test` folder inside PATH,
moves it to the Trash and back to test Undo, and deletes it at the end.
