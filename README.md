# DiskLens

See what's using your disk space. A GNOME app (GTK 4 + libadwaita) for Linux, in the spirit of
TreeSize: folder sizes, biggest files, file types and ages, a treemap, a duplicate finder,
and scanning a NAS over SSH.

It's written in Rust: one native program, about 26× faster to scan than the original Python
version (`python/disklens.py`) and using about 1/6 of the memory.

## Install

Needs Rust plus the GTK 4 and libadwaita development files
(Fedora: `sudo dnf install gtk4-devel libadwaita-devel`).

```bash
git clone https://github.com/Machero7/disklens.git ~/disklens
cd ~/disklens
./install.sh              # build, install to ~/.local/bin, add "DiskLens" to the app grid
./install.sh --uninstall  # remove it and restore the old launchers
```

The installer moves older DiskLens launchers (the Python one and "DiskLens Preview") to
`~/.local/share/disklens-old-launchers/`. The Python version still works with
`python3 python/disklens.py`.

## What it does

| Tab | Shows |
|---|---|
| Folder | Sizes of the folders and files in the current folder, coloured usage bar |
| Biggest | The 1000 largest files under the current folder |
| Types | Space per file type |
| Ages | Space by last-modified date |
| Map | Treemap: each tile's area is its size; click a folder to open it |
| Copies | Files with identical contents; mark extra copies and move them to the Trash |

On the Types tab, double-click a type (or right-click → Show Files) to list only that type's
files on the Biggest tab; a banner offers Show All Types. Right-click → Move All to Trash… moves
every file of that type in the current folder and its subfolders to the Trash, after asking.

Right-click any file or folder: Open, Show in File Manager, Open Terminal Here, Copy Path,
Go to Folder, Rescan, Move to Trash. Trash can be undone (toast button or Ctrl+Z).

Search: press Ctrl+F (or just start typing) to filter the Folder, Biggest and Types lists by
name; Esc closes the search.

Shortcuts: Ctrl+O scan · Ctrl+F search · F5 rescan · Esc stop · Alt+Up parent · Delete trash ·
Ctrl+Z undo · Ctrl+E export CSV · Ctrl+, preferences · Ctrl+? all shortcuts.

Settings live in `~/.config/disklens/settings.json` (shared with the Python version).

## Scanning a NAS over SSH

Click **Scan NAS over SSH…** on the start page (or in the drive menu), or run
`disklens user@192.168.1.50:/volume1`. DiskLens runs GNU `find` on the NAS through one SSH
connection and streams back sizes, dates and inodes, so the walk runs at the NAS's own disk speed.
It uses the system `ssh`, so keys, the agent and `~/.ssh/config` host names all work. Password
logins don't: a window has nowhere to type the password.

One-time setup on a Synology:

1. DSM: **Control Panel → Terminal & SNMP → Enable SSH service**.
2. DSM: **Control Panel → User & Group → Advanced → Enable user home service** (needed for keys).
   Your DSM user must be in the *administrators* group to log in over SSH.
3. On this computer: `ssh-copy-id user@192.168.1.50`, then check `ssh user@192.168.1.50` logs in
   without asking for a password.

Shares are under `/volume1/<share>`. NAS scans are read-only: Open, Show in File Manager,
Terminal, Move to Trash and Find Copies are off, because they'd act on this computer.
Copy Path copies `host:/path`. F5 rescans a folder over SSH. The last NAS that scanned
successfully is listed on the start page. `disklens-scan USER@HOST:/PATH` does the same from a terminal.

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
