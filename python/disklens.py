#!/usr/bin/env python3
"""DiskLens: a disk-usage browser for Linux (GTK4).

Scan a folder or drive, then browse it like a file manager where every folder
shows the total size of everything inside it. Every column sorts; biggest
first by default. Side tabs show the largest files, space per file type,
space per file age, and a treemap.

Usage: disklens.py [PATH]
"""
import colorsys
import concurrent.futures
import csv
import heapq
import json
import os
import queue
import shutil
import stat
import subprocess
import sys
import threading
import time
from datetime import datetime

import gi
gi.require_version("Gtk", "4.0")
gi.require_version("PangoCairo", "1.0")
from gi.repository import Gtk, Gdk, GLib, Gio, GObject, Pango, PangoCairo

VERSION = "1.0"
APP_ID = "com.familjenhero.DiskLens"
CONFIG_PATH = os.path.expanduser("~/.config/disklens/settings.json")
SCAN_THREADS = 16
TICK_MS = 500

REAL_FS = {
    "ext2", "ext3", "ext4", "btrfs", "xfs", "zfs", "f2fs", "bcachefs", "jfs",
    "reiserfs", "vfat", "exfat", "ntfs", "ntfs3", "fuseblk", "nfs", "nfs4",
    "cifs", "smb3", "fuse.sshfs", "fuse.rclone",
}
VIRTUAL_FS = {
    "proc", "sysfs", "devtmpfs", "devpts", "cgroup", "cgroup2", "securityfs",
    "debugfs", "tracefs", "bpf", "pstore", "configfs", "fusectl", "mqueue",
    "hugetlbfs", "autofs", "binfmt_misc", "efivarfs", "nsfs", "ramfs",
}
AGE_BUCKETS = [
    ("Less than 1 month", 30),
    ("1 – 6 months", 182),
    ("6 – 12 months", 365),
    ("1 – 2 years", 730),
    ("2 – 5 years", 1826),
    ("More than 5 years", None),
]

STATE = {"ondisk": False, "hidden": True, "onefs": True}


# ---------------------------------------------------------------- helpers

def human(n):
    if n < 1024:
        return f"{n} B"
    for unit in ("KB", "MB", "GB", "TB", "PB"):
        n /= 1024
        if n < 1024 or unit == "PB":
            return f"{n:.1f} {unit}" if n < 100 else f"{n:.0f} {unit}"


def fmt_time(t):
    return datetime.fromtimestamp(t).strftime("%Y-%m-%d %H:%M") if t else ""


def fmt_int(n):
    return f"{n:,}"


def cmp(a, b):
    return (a > b) - (a < b)


def ext_of(name):
    ext = os.path.splitext(name)[1].lower()
    if not ext or len(ext) > 10 or not ext[1:].replace("-", "").replace("_", "").isalnum():
        return "(none)"
    if len(ext) == 4 and ext[1] == "r" and ext[2:].isdigit():
        return ".r00–.r99"     # split RAR volumes
    return ext


_kind_cache = {}


def kind_info(name):
    """(description, Gio.Icon) for a file name, cached per extension."""
    ext = ext_of(name)
    info = _kind_cache.get(ext)
    if info is None:
        ctype, _ = Gio.content_type_guess(name, None)
        if ext == "(none)":
            ctype, desc = "application/octet-stream", "File"
        elif ext == ".r00–.r99":
            desc = "RAR archive part"
        elif ext != "(none)" and ctype == "application/octet-stream":
            desc = f"{ext[1:].upper()} file"
        else:
            desc = Gio.content_type_get_description(ctype)
        info = (desc, Gio.content_type_get_icon(ctype))
        _kind_cache[ext] = info
    return info


FOLDER_ICON = Gio.ThemedIcon.new("folder")
WARN_ICON = Gio.ThemedIcon.new("dialog-warning-symbolic")


def visible(name):
    return STATE["hidden"] or not name.startswith(".")


def virtual_devs():
    devs = set()
    try:
        with open("/proc/self/mounts") as fh:
            for line in fh:
                parts = line.split()
                if len(parts) >= 3 and parts[2] in VIRTUAL_FS:
                    try:
                        devs.add(os.stat(parts[1].replace("\\040", " ")).st_dev)
                    except OSError:
                        pass
    except OSError:
        pass
    return devs


def list_mounts():
    """[(mountpoint, fstype, total, free)] for real filesystems; hung mounts are skipped."""
    mounts, seen = [], set()
    try:
        with open("/proc/self/mounts") as fh:
            for line in fh:
                parts = line.split()
                if len(parts) < 3 or parts[2] not in REAL_FS:
                    continue
                mnt = parts[1].replace("\\040", " ")
                if mnt in seen or mnt.startswith(("/snap/", "/var/snap/")):
                    continue
                seen.add(mnt)
                mounts.append((mnt, parts[2]))
    except OSError:
        return []
    ex = concurrent.futures.ThreadPoolExecutor(8)
    futs = [(m, fs, ex.submit(os.statvfs, m)) for m, fs in mounts]
    out = []
    deadline = time.monotonic() + 1.5
    for m, fs, fut in futs:
        try:
            st = fut.result(timeout=max(0.05, deadline - time.monotonic()))
            out.append((m, fs, st.f_blocks * st.f_frsize, st.f_bavail * st.f_frsize))
        except Exception:
            pass
    ex.shutdown(wait=False, cancel_futures=True)
    return out


def load_settings():
    try:
        with open(CONFIG_PATH) as fh:
            return json.load(fh)
    except (OSError, ValueError):
        return {}


def save_settings(data):
    try:
        os.makedirs(os.path.dirname(CONFIG_PATH), exist_ok=True)
        with open(CONFIG_PATH, "w") as fh:
            json.dump(data, fh, indent=2)
    except OSError:
        pass


# ---------------------------------------------------------------- scan model

class DirNode:
    """A scanned folder. Totals (size, alloc, nfiles, ndirs, mtime) cover the whole subtree."""
    __slots__ = ("name", "parent", "dirs", "files", "size", "alloc",
                 "nfiles", "ndirs", "mtime", "scanned", "error")

    def __init__(self, name, parent=None, mtime=0.0):
        self.name = name
        self.parent = parent
        self.dirs = []
        self.files = []        # (name, size, alloc, mtime)
        self.size = 0
        self.alloc = 0
        self.nfiles = 0
        self.ndirs = 0
        self.mtime = mtime
        self.scanned = False
        self.error = None

    @property
    def path(self):
        parts, n = [], self
        while n.parent is not None:
            parts.append(n.name)
            n = n.parent
        return os.path.join(n.name, *reversed(parts))

    def value(self, ondisk):
        return self.alloc if ondisk else self.size

    def is_inside(self, other):
        n = self
        while n is not None:
            if n is other:
                return True
            n = n.parent
        return False


def file_value(f, ondisk):
    return f[2] if ondisk else f[1]


class Scanner:
    """Parallel scandir walk. Each finished folder adds its own files to every ancestor."""

    def __init__(self, root, lock, one_fs, dev=None):
        self.root = root
        self.lock = lock
        self.one_fs = one_fs
        self.path = root.path
        self.dev = dev if dev is not None else os.stat(self.path).st_dev
        self.virtual = virtual_devs()
        self.q = queue.Queue()
        self.pending = 0
        self.cancelled = False
        self.finished = False
        self.nfiles = 0
        self.ndirs = 0
        self.bytes = 0
        self.errors = []
        self.skipped = []
        self.seen_inodes = set()
        self.started = time.monotonic()
        self.ended = None

    def start(self):
        self.pending = 1
        self.q.put((self.root, self.path))
        for _ in range(SCAN_THREADS):
            threading.Thread(target=self._worker, daemon=True).start()

    def cancel(self):
        self.cancelled = True

    @property
    def elapsed(self):
        return (self.ended or time.monotonic()) - self.started

    def _worker(self):
        while True:
            item = self.q.get()
            if item is None:
                return
            node, path = item
            if not self.cancelled:
                try:
                    self._scan(node, path)
                except Exception as e:  # never let a worker die with pending work
                    self.errors.append((path, str(e)))
            with self.lock:
                self.pending -= 1
                done = self.pending == 0
            if done:
                self.ended = time.monotonic()
                self.finished = True
                for _ in range(SCAN_THREADS):
                    self.q.put(None)

    def _scan(self, node, path):
        files, subdirs = [], []
        size = alloc = 0
        newest = 0.0
        try:
            it = os.scandir(path)
        except OSError as e:
            self.errors.append((path, e.strerror or str(e)))
            with self.lock:
                node.error = e.strerror or str(e)
                node.scanned = True
            return
        with it:
            for e in it:
                try:
                    st = e.stat(follow_symlinks=False)
                except OSError as ex:
                    self.errors.append((os.path.join(path, e.name), ex.strerror or str(ex)))
                    continue
                if stat.S_ISDIR(st.st_mode):
                    if st.st_dev != self.dev and (self.one_fs or st.st_dev in self.virtual):
                        self.skipped.append(e.path)
                        continue
                    subdirs.append((DirNode(e.name, node, st.st_mtime), e.path))
                    continue
                f = (e.name, st.st_size, st.st_blocks * 512, st.st_mtime)
                files.append(f)
                if st.st_nlink > 1:
                    key = (st.st_dev, st.st_ino)
                    with self.lock:
                        dup = key in self.seen_inodes
                        self.seen_inodes.add(key)
                    if dup:
                        continue
                size += f[1]
                alloc += f[2]
                if f[3] > newest:
                    newest = f[3]
        with self.lock:
            node.files = files
            node.dirs.extend(d for d, _ in subdirs)
            node.scanned = True
            nf, nd = len(files), len(subdirs)
            p = node
            while p is not None:
                p.size += size
                p.alloc += alloc
                p.nfiles += nf
                p.ndirs += nd
                if newest > p.mtime:
                    p.mtime = newest
                p = p.parent
            self.nfiles += nf
            self.ndirs += nd
            self.bytes += size
            self.pending += nd
        for item in subdirs:
            self.q.put(item)


# ---------------------------------------------------------------- list rows

class Row(GObject.Object):
    """One folder or file in the tree / contents views. Display props update live."""
    __gtype_name__ = "DiskLensRow"

    name = GObject.Property(type=str, default="")
    size_text = GObject.Property(type=str, default="")
    fraction = GObject.Property(type=float, default=0.0)
    pct_text = GObject.Property(type=str, default="")
    files_text = GObject.Property(type=str, default="")
    dirs_text = GObject.Property(type=str, default="")
    mtime_text = GObject.Property(type=str, default="")
    kind_text = GObject.Property(type=str, default="")
    gicon = GObject.Property(type=Gio.Icon)

    def __init__(self, node=None, f=None, parent=None, root=False):
        super().__init__()
        self.node = node
        self.f = f
        self.parent = parent
        self.is_dir = node is not None
        self._key = None
        if node is not None:
            self.props.name = node.path if root else node.name
            self.props.kind_text = "Folder"
            self.props.gicon = FOLDER_ICON
        else:
            desc, icon = kind_info(f[0])
            self.props.name = f[0]
            self.props.kind_text = desc
            self.props.gicon = icon
            self.props.mtime_text = fmt_time(f[3])
        self.refresh()

    def value(self, ondisk=None):
        ondisk = STATE["ondisk"] if ondisk is None else ondisk
        return self.node.value(ondisk) if self.is_dir else file_value(self.f, ondisk)

    @property
    def path(self):
        return self.node.path if self.is_dir else os.path.join(self.parent.path, self.f[0])

    def refresh(self):
        ondisk = STATE["ondisk"]
        v = self.value(ondisk)
        pv = self.parent.value(ondisk) if self.parent is not None else v
        if self.is_dir:
            n = self.node
            key = (v, pv, n.nfiles, n.ndirs, n.mtime, n.error)
        else:
            key = (v, pv)
        if key == self._key:
            return
        self._key = key
        frac = (v / pv) if pv else (1.0 if self.parent is None else 0.0)
        self.props.size_text = human(v)
        self.props.fraction = min(frac, 1.0)
        self.props.pct_text = f"{frac * 100:.1f} %"
        if self.is_dir:
            n = self.node
            self.props.files_text = fmt_int(n.nfiles)
            self.props.dirs_text = fmt_int(n.ndirs)
            self.props.mtime_text = fmt_time(n.mtime)
            if n.error:
                self.props.kind_text = f"No access ({n.error})"
                self.props.gicon = WARN_ICON


SORT_KEYS = {
    "name": lambda r: r.props.name.casefold(),
    "size": lambda r: r.value(),
    "pct": lambda r: r.value(),
    "files": lambda r: r.node.nfiles if r.is_dir else -1,
    "dirs": lambda r: r.node.ndirs if r.is_dir else -1,
    "mtime": lambda r: r.node.mtime if r.is_dir else r.f[3],
    "kind": lambda r: (0, "") if r.is_dir else (1, r.props.kind_text.casefold()),
}


def unwrap(item):
    return item.get_item() if isinstance(item, Gtk.TreeListRow) else item


def key_sorter(keyf, wrap=unwrap):
    return Gtk.CustomSorter.new(lambda a, b, _: cmp(keyf(wrap(a)), keyf(wrap(b))), None)


def _bind(li, row, pairs):
    w = li.get_child()
    w._dlrow = row
    w._binds = [row.bind_property(src, tgt, prop, GObject.BindingFlags.SYNC_CREATE)
                for src, tgt, prop in pairs]


def _unbind(_f, li):
    w = li.get_child()
    for b in getattr(w, "_binds", []):
        b.unbind()
    w._binds = []
    w._dlrow = None


def name_factory(tree):
    f = Gtk.SignalListItemFactory()

    def setup(_f, li):
        box = Gtk.Box(spacing=6)
        box.append(Gtk.Image())
        box.append(Gtk.Label(xalign=0, ellipsize=Pango.EllipsizeMode.MIDDLE))
        if tree:
            exp = Gtk.TreeExpander()
            exp.set_child(box)
            li.set_child(exp)
        else:
            li.set_child(box)

    def bind(_f, li):
        item = li.get_item()
        if tree:
            exp = li.get_child()
            exp.set_list_row(item)
            box = exp.get_child()
        else:
            box = li.get_child()
        row = unwrap(item)
        img = box.get_first_child()
        lab = img.get_next_sibling()
        _bind(li, row, [("gicon", img, "gicon"), ("name", lab, "label")])

    f.connect("setup", setup)
    f.connect("bind", bind)
    f.connect("unbind", _unbind)
    return f


def label_factory(prop, xalign=1.0, dim=False):
    f = Gtk.SignalListItemFactory()

    def setup(_f, li):
        lab = Gtk.Label(xalign=xalign, ellipsize=Pango.EllipsizeMode.END)
        if dim:
            lab.add_css_class("dim-label")
        lab.add_css_class("numeric")
        li.set_child(lab)

    f.connect("setup", setup)
    f.connect("bind", lambda _f, li: _bind(li, unwrap(li.get_item()), [(prop, li.get_child(), "label")]))
    f.connect("unbind", _unbind)
    return f


def bar_factory():
    f = Gtk.SignalListItemFactory()

    def setup(_f, li):
        box = Gtk.Box(spacing=6)
        bar = Gtk.ProgressBar(hexpand=True, valign=Gtk.Align.CENTER)
        lab = Gtk.Label(xalign=1, width_chars=7)
        lab.add_css_class("numeric")
        box.append(bar)
        box.append(lab)
        li.set_child(box)

    def bind(_f, li):
        box = li.get_child()
        bar = box.get_first_child()
        _bind(li, unwrap(li.get_item()), [("fraction", bar, "fraction"),
                                           ("pct_text", bar.get_next_sibling(), "label")])

    f.connect("setup", setup)
    f.connect("bind", bind)
    f.connect("unbind", _unbind)
    return f


def add_column(view, title, factory, key, width=None, expand=False):
    col = Gtk.ColumnViewColumn.new(title, factory)
    col.set_resizable(True)
    col.set_expand(expand)
    if width:
        col.set_fixed_width(width)
    col.set_sorter(key_sorter(SORT_KEYS[key]))
    view.append_column(col)
    return col


def row_at(widget, x, y):
    """The Row whose cell is under (x, y) in a ColumnView, or None."""
    w = widget.pick(x, y, Gtk.PickFlags.DEFAULT)
    while w is not None and w is not widget:
        row = getattr(w, "_dlrow", None)
        if row is not None:
            return row
        w = w.get_parent()
    return None


# ---------------------------------------------------------------- simple tables

class Item(GObject.Object):
    __gtype_name__ = "DiskLensItem"

    def __init__(self, data):
        super().__init__()
        self.data = data


class Table:
    """Static, sortable ColumnView for the analysis tabs.

    columns: dicts with title, text(data)->str, key(data), and optional
    bar(data)->fraction, expand, width, xalign.
    """

    def __init__(self, columns, default_sort=1, on_activate=None):
        self.store = Gio.ListStore(item_type=Item)
        self.view = Gtk.ColumnView(show_column_separators=False, reorderable=False)
        self.view.add_css_class("data-table")
        cols = []
        for c in columns:
            col = Gtk.ColumnViewColumn.new(c["title"], self._factory(c))
            col.set_resizable(True)
            col.set_expand(c.get("expand", False))
            if c.get("width"):
                col.set_fixed_width(c["width"])
            col.set_sorter(key_sorter(c["key"], wrap=lambda it: it.data))
            self.view.append_column(col)
            cols.append(col)
        self.sort = Gtk.SortListModel(model=self.store, sorter=self.view.get_sorter())
        self.sel = Gtk.SingleSelection(model=self.sort, autoselect=False)
        self.view.set_model(self.sel)
        self.view.sort_by_column(cols[default_sort], Gtk.SortType.DESCENDING)
        if on_activate:
            self.view.connect("activate", lambda _v, pos: on_activate(self.sel.get_item(pos).data))
        self.widget = Gtk.ScrolledWindow(child=self.view, vexpand=True)

    @staticmethod
    def _factory(c):
        f = Gtk.SignalListItemFactory()

        def setup(_f, li):
            lab = Gtk.Label(xalign=c.get("xalign", 1.0), ellipsize=Pango.EllipsizeMode.MIDDLE)
            lab.add_css_class("numeric")
            if c.get("bar"):
                box = Gtk.Box(spacing=6)
                box.append(Gtk.ProgressBar(hexpand=True, valign=Gtk.Align.CENTER))
                lab.set_width_chars(7)
                box.append(lab)
                li.set_child(box)
            else:
                li.set_child(lab)

        def bind(_f, li):
            data = li.get_item().data
            w = li.get_child()
            w._dlitem = data
            if c.get("bar"):
                w.get_first_child().set_fraction(min(1.0, c["bar"](data)))
                w.get_last_child().set_label(c["text"](data))
            else:
                w.set_label(c["text"](data))

        f.connect("setup", setup)
        f.connect("bind", bind)
        return f

    def set_rows(self, rows):
        self.store.splice(0, self.store.get_n_items(), [Item(d) for d in rows])

    def item_at(self, x, y):
        w = self.view.pick(x, y, Gtk.PickFlags.DEFAULT)
        while w is not None and w is not self.view:
            data = getattr(w, "_dlitem", None)
            if data is not None:
                return data
            w = w.get_parent()
        return None


def compute_stats(node, ondisk, top_n=200):
    top, exts, i = [], {}, 0
    ages = [[0, 0] for _ in AGE_BUCKETS]
    now = time.time()
    stack = [node]
    while stack:
        d = stack.pop()
        stack.extend(list(d.dirs))
        for f in list(d.files):
            if not visible(f[0]):
                continue
            v = file_value(f, ondisk)
            if len(top) < top_n:
                heapq.heappush(top, (v, i, d, f))
            elif v > top[0][0]:
                heapq.heapreplace(top, (v, i, d, f))
            i += 1
            e = exts.get(ext_of(f[0]))
            if e is None:
                exts[ext_of(f[0])] = [v, 1, f[0]]
            else:
                e[0] += v
                e[1] += 1
            days = (now - f[3]) / 86400
            for b, (_, limit) in enumerate(AGE_BUCKETS):
                if limit is None or days < limit:
                    ages[b][0] += v
                    ages[b][1] += 1
                    break
    top.sort(reverse=True)
    return [(d, f) for _, _, d, f in top], exts, ages


# ---------------------------------------------------------------- treemap

def _worst(row, short):
    s = sum(row)
    return max(short * short * max(row) / (s * s), (s * s) / (short * short * min(row)))


def squarify(values, x, y, w, h):
    """Squarified treemap layout. values must be > 0 and sorted descending."""
    total = sum(values)
    if total <= 0 or w <= 0 or h <= 0:
        return [(x, y, 0, 0)] * len(values)
    scale = w * h / total
    vals = [v * scale for v in values]
    rects, i = [], 0
    while i < len(vals):
        short = min(w, h) or 1e-9
        row = [vals[i]]
        j = i + 1
        while j < len(vals) and _worst(row + [vals[j]], short) <= _worst(row, short):
            row.append(vals[j])
            j += 1
        s = sum(row)
        if w >= h:
            cw = s / h if h else 0
            yy = y
            for v in row:
                rh = v / cw if cw else 0
                rects.append((x, yy, cw, rh))
                yy += rh
            x += cw
            w -= cw
        else:
            rh = s / w if w else 0
            xx = x
            for v in row:
                cw = v / rh if rh else 0
                rects.append((xx, y, cw, rh))
                xx += cw
            y += rh
            h -= rh
        i = j
    return rects


class Treemap(Gtk.DrawingArea):
    def __init__(self, win):
        super().__init__(hexpand=True, vexpand=True)
        self.win = win
        self.node = None
        self.rects = []   # (x, y, w, h, dirnode|None, file|None, parentnode, value, label)
        self.set_draw_func(self._draw)
        click = Gtk.GestureClick(button=1)
        click.connect("released", self._on_click)
        self.add_controller(click)
        self.set_has_tooltip(True)
        self.connect("query-tooltip", self._on_tooltip)

    def set_node(self, node):
        self.node = node
        self.queue_draw()

    @staticmethod
    def _items(node, ondisk, limit):
        items = [(d.value(ondisk), d, None) for d in list(node.dirs) if visible(d.name)]
        items += [(file_value(f, ondisk), None, f) for f in list(node.files) if visible(f[0])]
        items = [it for it in items if it[0] > 0]
        items.sort(key=lambda it: it[0], reverse=True)
        if len(items) > limit:
            rest = items[limit:]
            items = items[:limit] + [(sum(it[0] for it in rest), None, len(rest))]
        return items

    def _label(self, cr, text, x, y, w, h, dark):
        if w < 40 or h < 16:
            return
        layout = PangoCairo.create_layout(cr)
        layout.set_font_description(Pango.FontDescription.from_string("Sans 8.5"))
        layout.set_text(text, -1)
        layout.set_width(int((w - 6) * Pango.SCALE))
        layout.set_height(int((h - 4) * Pango.SCALE))
        layout.set_ellipsize(Pango.EllipsizeMode.END)
        cr.set_source_rgb(*((1, 1, 1) if dark else (0.08, 0.08, 0.08)))
        cr.move_to(x + 3, y + 2)
        PangoCairo.show_layout(cr, layout)

    def _box(self, cr, x, y, w, h, rgb):
        cr.set_source_rgb(*rgb)
        cr.rectangle(x + 0.5, y + 0.5, max(0, w - 1), max(0, h - 1))
        cr.fill_preserve()
        cr.set_source_rgba(0, 0, 0, 0.35)
        cr.set_line_width(1)
        cr.stroke()

    def _draw(self, _area, cr, width, height):
        self.rects = []
        cr.set_source_rgb(0.16, 0.16, 0.17)
        cr.paint()
        if self.node is not None:
            self._level(cr, self.node, 0, 0, width, height, 0, None)

    def _level(self, cr, node, x, y, w, h, depth, hue):
        """Lay out node's children in the box; recurse into folders that are big enough."""
        ondisk = STATE["ondisk"]
        items = self._items(node, ondisk, 300 if depth == 0 else 120)
        for idx, ((v, d, f), (rx, ry, rw, rh)) in enumerate(
                zip(items, squarify([it[0] for it in items], x, y, w, h))):
            if rw < 1 or rh < 1:
                continue
            ihue = (idx * 0.618034) % 1.0 if hue is None else hue
            if isinstance(f, int):
                rgb, name = (0.45, 0.45, 0.47), f"{f} smaller items"
            elif d is None:
                rgb, name = colorsys.hls_to_rgb(ihue, min(0.78, 0.62 + 0.04 * depth), 0.30), f[0]
            else:
                rgb, name = colorsys.hls_to_rgb(ihue, min(0.66, 0.40 + 0.07 * depth), 0.55), d.name
            self._box(cr, rx, ry, rw, rh, rgb)
            self.rects.append((rx, ry, rw, rh, d, None if isinstance(f, int) else f, node, v, name))
            label = f"{name}  {human(v)}"
            if d is not None and rw > 50 and rh > 40 and depth < 6:
                self._label(cr, label, rx, ry, rw, 17, depth < 2)
                self._level(cr, d, rx + 3, ry + 17, rw - 6, rh - 20, depth + 1, ihue)
            else:
                self._label(cr, label, rx, ry, rw, rh, d is not None and depth < 2)

    def hit(self, x, y):
        for r in reversed(self.rects):
            if r[0] <= x < r[0] + r[2] and r[1] <= y < r[1] + r[3]:
                return r
        return None

    def _on_click(self, _g, _n, x, y):
        r = self.hit(x, y)
        if r is None:
            return
        if r[4] is not None:
            self.win.show_folder(r[4])
        elif r[6] is not self.node:
            self.win.show_folder(r[6])

    def _on_tooltip(self, _w, x, y, _kb, tooltip):
        r = self.hit(x, y)
        if r is None:
            return False
        path = r[4].path if r[4] is not None else os.path.join(r[6].path, r[8])
        tooltip.set_text(f"{path}\n{human(r[7])}")
        return True


# ---------------------------------------------------------------- context targets

class Target:
    def __init__(self, node=None, parent=None, f=None):
        self.node = node
        self.parent = parent if node is None else node.parent
        self.f = f

    @classmethod
    def from_row(cls, row):
        return cls(node=row.node) if row.is_dir else cls(parent=row.parent, f=row.f)

    @property
    def is_dir(self):
        return self.node is not None

    @property
    def path(self):
        return self.node.path if self.is_dir else os.path.join(self.parent.path, self.f[0])

    @property
    def size(self):
        return self.node.value(STATE["ondisk"]) if self.is_dir else file_value(self.f, STATE["ondisk"])


TERMINALS = [
    ["ptyxis", "--new-window", "--working-directory={}"],
    ["gnome-terminal", "--working-directory={}"],
    ["kgx", "--working-directory={}"],
    ["konsole", "--workdir", "{}"],
    ["xfce4-terminal", "--working-directory={}"],
    ["qterminal", "-w", "{}"],
    ["x-terminal-emulator"],
]


# ---------------------------------------------------------------- window

class DiskLensWindow(Gtk.ApplicationWindow):
    def __init__(self, app, settings):
        super().__init__(application=app, title="DiskLens")
        self.settings = settings
        self.set_default_size(settings.get("width", 1350), settings.get("height", 820))
        self.lock = threading.Lock()
        self.root = None
        self.root_dev = None
        self.current = None
        self.scanner = None
        self.tick_id = None
        self.tick_count = 0
        self.tree_stores = {}       # id(node) -> [node, store, synced]
        self.contents = None        # [node, store, synced_dirs, files_added]
        self.stats_gen = 0
        self.ctx_targets = []
        self._syncing = False
        self.all_errors = []

        self._build_actions()
        self._build_header()
        self.stack = Gtk.Stack(transition_type=Gtk.StackTransitionType.CROSSFADE)
        self.stack.add_named(self._build_welcome(), "welcome")
        self.stack.add_named(self._build_main(), "main")
        self.set_child(self.stack)
        self.connect("close-request", self._on_close)

    # ---------- construction

    def _build_actions(self):
        def act(name, cb, accel=None):
            a = Gio.SimpleAction.new(name, None)
            a.connect("activate", lambda *_: cb())
            self.add_action(a)
            if accel:
                self.get_application().set_accels_for_action(f"win.{name}", accel)
            return a

        def toggle(name, key, cb):
            a = Gio.SimpleAction.new_stateful(name, None, GLib.Variant.new_boolean(STATE[key]))

            def changed(action, value):
                action.set_state(value)
                STATE[key] = value.get_boolean()
                cb()
            a.connect("change-state", changed)
            self.add_action(a)

        act("open", self.choose_folder, ["<Control>o"])
        act("up", self.go_up, ["<Alt>Up", "BackSpace"])
        self.a_rescan = act("rescan", lambda: self.current and self.rescan(self.current), ["F5"])
        self.a_stop = act("stop", self.stop_scan, ["Escape"])
        act("export", self.export_csv, ["<Control>e"])
        act("trash-selection", self.trash_selection, ["Delete"])
        act("about", self.show_about)
        act("errors", self.show_errors)
        toggle("ondisk", "ondisk", self.on_mode_changed)
        toggle("hidden", "hidden", self.on_hidden_changed)
        toggle("onefs", "onefs", lambda: None)

        act("ctx-open", lambda: self._ctx(self.open_default))
        act("ctx-reveal", lambda: self._ctx(self.reveal))
        act("ctx-terminal", lambda: self._ctx(self.open_terminal))
        act("ctx-copy", lambda: self.copy_paths(self.ctx_targets))
        act("ctx-goto", lambda: self._ctx(self.goto))
        self.a_ctx_rescan = act("ctx-rescan", lambda: self._ctx(lambda t: self.rescan(t.node or t.parent)))
        self.a_ctx_trash = act("ctx-trash", lambda: self.trash(self.ctx_targets))

        self.ctx_menu = Gio.Menu()
        s1 = Gio.Menu()
        s1.append("Open", "win.ctx-open")
        s1.append("Show in File Manager", "win.ctx-reveal")
        s1.append("Open Terminal Here", "win.ctx-terminal")
        s1.append("Copy Path", "win.ctx-copy")
        s2 = Gio.Menu()
        s2.append("Go to Folder", "win.ctx-goto")
        s2.append("Rescan", "win.ctx-rescan")
        s2.append("Export View to CSV…", "win.export")
        s3 = Gio.Menu()
        s3.append("Move to Trash", "win.ctx-trash")
        for s in (s1, s2, s3):
            self.ctx_menu.append_section(None, s)

    def _build_header(self):
        hb = Gtk.HeaderBar()
        open_btn = Gtk.Button(icon_name="folder-open-symbolic", tooltip_text="Scan a folder (Ctrl+O)",
                              action_name="win.open")
        self.drives_btn = Gtk.MenuButton(icon_name="drive-harddisk-symbolic", tooltip_text="Scan a drive")
        self.drives_pop = Gtk.Popover()
        self.drives_pop.connect("show", lambda p: p.set_child(self._drive_list(compact=True)))
        self.drives_btn.set_popover(self.drives_pop)
        up_btn = Gtk.Button(icon_name="go-up-symbolic", tooltip_text="Parent folder (Alt+Up)", action_name="win.up")
        self.rescan_btn = Gtk.Button(icon_name="view-refresh-symbolic", tooltip_text="Rescan this folder (F5)",
                                     action_name="win.rescan")
        self.stop_btn = Gtk.Button(icon_name="process-stop-symbolic", tooltip_text="Stop scan (Esc)",
                                   action_name="win.stop", visible=False)
        for w in (open_btn, self.drives_btn, up_btn, self.rescan_btn, self.stop_btn):
            hb.pack_start(w)

        menu = Gio.Menu()
        view = Gio.Menu()
        view.append("Show Size on Disk (allocated)", "win.ondisk")
        view.append("Show Hidden Files", "win.hidden")
        view.append("Stay on One Filesystem (next scan)", "win.onefs")
        menu.append_section(None, view)
        other = Gio.Menu()
        other.append("Export View to CSV…", "win.export")
        other.append("About DiskLens", "win.about")
        menu.append_section(None, other)
        hb.pack_end(Gtk.MenuButton(icon_name="open-menu-symbolic", menu_model=menu))

        self.mode_label = Gtk.Label(css_classes=["dim-label"])
        hb.pack_end(self.mode_label)
        self._update_mode_label()
        self.set_titlebar(hb)

    def _drive_list(self, compact=False):
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
        if compact:
            box.set_margin_top(6)
            box.set_margin_bottom(6)
            box.set_margin_start(6)
            box.set_margin_end(6)
        entries = [(os.path.expanduser("~"), "Home folder", None, None)]
        entries += [(m, fs, total, free) for m, fs, total, free in list_mounts()]
        for path, fs, total, free in entries:
            btn = Gtk.Button(css_classes=["flat"])
            inner = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=3)
            top = Gtk.Box(spacing=12)
            top.append(Gtk.Image(icon_name="user-home-symbolic" if total is None else
                                 ("folder-remote-symbolic" if fs.startswith(("nfs", "cifs", "smb", "fuse.")) else
                                  "drive-harddisk-symbolic")))
            top.append(Gtk.Label(label=path, xalign=0, hexpand=True, ellipsize=Pango.EllipsizeMode.MIDDLE,
                                 width_chars=28, css_classes=["heading"]))
            if total:
                used = total - free
                top.append(Gtk.Label(label=f"{human(used)} of {human(total)} · {fs}", css_classes=["dim-label"]))
            else:
                top.append(Gtk.Label(label=fs, css_classes=["dim-label"]))
            inner.append(top)
            if total:
                lb = Gtk.LevelBar(min_value=0, max_value=1, value=(total - free) / total if total else 0)
                inner.append(lb)
            btn.set_child(inner)
            btn.connect("clicked", lambda _b, p=path: (self.drives_pop.popdown(), self.start_scan(p)))
            box.append(btn)
        return box

    def _build_welcome(self):
        outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=18, valign=Gtk.Align.CENTER,
                        halign=Gtk.Align.CENTER, margin_top=24, margin_bottom=24)
        outer.append(Gtk.Image(icon_name="drive-harddisk-symbolic", pixel_size=64, css_classes=["dim-label"]))
        outer.append(Gtk.Label(label="What should DiskLens scan?", css_classes=["title-2"]))
        self.welcome_list = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        self.welcome_list.set_size_request(560, -1)
        outer.append(self.welcome_list)
        GLib.idle_add(lambda: self.welcome_list.append(self._drive_list()) and False)
        btn = Gtk.Button(label="Choose Folder…", action_name="win.open", halign=Gtk.Align.CENTER,
                         css_classes=["suggested-action", "pill"])
        outer.append(btn)
        return Gtk.ScrolledWindow(child=outer)

    def _build_main(self):
        main = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)

        # breadcrumb
        self.crumbs = Gtk.Box(spacing=0, margin_start=6, margin_end=6, margin_top=4, margin_bottom=4)
        crumb_scroll = Gtk.ScrolledWindow(child=self.crumbs, vscrollbar_policy=Gtk.PolicyType.NEVER,
                                          hscrollbar_policy=Gtk.PolicyType.AUTOMATIC)
        self.folder_summary = Gtk.Label(css_classes=["dim-label"], margin_end=10)
        bar = Gtk.Box()
        crumb_scroll.set_hexpand(True)
        bar.append(crumb_scroll)
        bar.append(self.folder_summary)
        main.append(bar)
        main.append(Gtk.Separator())

        # folder tree
        self.tree_root_store = Gio.ListStore(item_type=Row)
        self.tree_model = Gtk.TreeListModel.new(self.tree_root_store, False, False, self._tree_children)
        self.tree_view = Gtk.ColumnView(reorderable=False)
        self.tree_view.add_css_class("data-table")
        add_column(self.tree_view, "Folder", name_factory(True), "name", expand=True)
        tsize = add_column(self.tree_view, "Size", label_factory("size_text"), "size", width=90)
        add_column(self.tree_view, "%", bar_factory(), "pct", width=130)
        self.tree_sort = Gtk.SortListModel(model=self.tree_model,
                                           sorter=Gtk.TreeListRowSorter.new(self.tree_view.get_sorter()))
        self.tree_sel = Gtk.SingleSelection(model=self.tree_sort)
        self.tree_sel.connect("notify::selected-item", self._on_tree_selected)
        self.tree_view.set_model(self.tree_sel)
        self.tree_view.sort_by_column(tsize, Gtk.SortType.DESCENDING)
        self._attach_context(self.tree_view, lambda x, y: self._row_targets(self.tree_view, None, x, y))

        # contents
        self.contents_view = Gtk.ColumnView(reorderable=False)
        self.contents_view.add_css_class("data-table")
        add_column(self.contents_view, "Name", name_factory(False), "name", expand=True)
        csize = add_column(self.contents_view, "Size", label_factory("size_text"), "size", width=90)
        add_column(self.contents_view, "% of folder", bar_factory(), "pct", width=150)
        add_column(self.contents_view, "Files", label_factory("files_text", dim=True), "files", width=80)
        add_column(self.contents_view, "Folders", label_factory("dirs_text", dim=True), "dirs", width=75)
        add_column(self.contents_view, "Modified", label_factory("mtime_text", dim=True), "mtime", width=135)
        add_column(self.contents_view, "Type", label_factory("kind_text", xalign=0, dim=True), "kind", width=170)
        self.contents_sort = Gtk.SortListModel(sorter=self.contents_view.get_sorter())
        self.contents_sel = Gtk.MultiSelection(model=self.contents_sort)
        self.contents_view.set_model(self.contents_sel)
        self.contents_view.sort_by_column(csize, Gtk.SortType.DESCENDING)
        self.contents_view.connect("activate", self._on_contents_activate)
        self._attach_context(self.contents_view,
                             lambda x, y: self._row_targets(self.contents_view, self.contents_sel, x, y))

        # analysis tabs
        self.top_table = Table([
            {"title": "Name", "text": lambda d: d[1][0], "key": lambda d: d[1][0].casefold(), "xalign": 0,
             "expand": True},
            {"title": "Size", "text": lambda d: human(file_value(d[1], STATE["ondisk"])),
             "key": lambda d: file_value(d[1], STATE["ondisk"]), "width": 90},
            {"title": "Folder", "text": lambda d: self._rel(d[0]), "key": lambda d: d[0].path, "xalign": 0,
             "width": 380},
            {"title": "Modified", "text": lambda d: fmt_time(d[1][3]), "key": lambda d: d[1][3], "width": 135},
        ], on_activate=lambda d: self.show_folder(d[0], select_file=d[1]))
        self._attach_context(self.top_table.view, lambda x, y: self._table_targets(self.top_table, x, y))

        self.types_table = Table([
            {"title": "Extension", "text": lambda d: d[0], "key": lambda d: d[0], "xalign": 0, "width": 110},
            {"title": "Type", "text": lambda d: kind_info(d[3])[0], "key": lambda d: kind_info(d[3])[0],
             "xalign": 0, "expand": True},
            {"title": "Size", "text": lambda d: human(d[1]), "key": lambda d: d[1], "width": 90},
            {"title": "%", "text": lambda d: f"{d[4] * 100:.1f} %", "key": lambda d: d[1], "bar": lambda d: d[4],
             "width": 170},
            {"title": "Files", "text": lambda d: fmt_int(d[2]), "key": lambda d: d[2], "width": 90},
        ], default_sort=2)

        self.ages_table = Table([
            {"title": "Last modified", "text": lambda d: d[0], "key": lambda d: d[3], "xalign": 0, "expand": True},
            {"title": "Size", "text": lambda d: human(d[1]), "key": lambda d: d[1], "width": 90},
            {"title": "%", "text": lambda d: f"{d[4] * 100:.1f} %", "key": lambda d: d[1], "bar": lambda d: d[4],
             "width": 220},
            {"title": "Files", "text": lambda d: fmt_int(d[2]), "key": lambda d: d[2], "width": 90},
        ], default_sort=1)

        self.treemap = Treemap(self)
        self._attach_context(self.treemap, self._treemap_targets)

        self.notebook = Gtk.Notebook()
        self.notebook.append_page(Gtk.ScrolledWindow(child=self.contents_view), Gtk.Label(label="Contents"))
        self.notebook.append_page(self.top_table.widget, Gtk.Label(label="Biggest Files"))
        self.notebook.append_page(self.types_table.widget, Gtk.Label(label="File Types"))
        self.notebook.append_page(self.ages_table.widget, Gtk.Label(label="File Ages"))
        self.notebook.append_page(self.treemap, Gtk.Label(label="Treemap"))
        self.notebook.connect("switch-page", lambda *_: GLib.idle_add(self._refresh_page))

        self.paned = Gtk.Paned(orientation=Gtk.Orientation.HORIZONTAL, vexpand=True,
                               position=self.settings.get("pane", 430))
        self.paned.set_start_child(Gtk.ScrolledWindow(child=self.tree_view))
        self.paned.set_end_child(self.notebook)
        self.paned.set_shrink_start_child(False)
        main.append(self.paned)

        # status bar
        main.append(Gtk.Separator())
        status = Gtk.Box(spacing=8, margin_start=10, margin_end=10, margin_top=4, margin_bottom=4)
        self.spinner = Gtk.Spinner()
        self.status = Gtk.Label(xalign=0, hexpand=True, ellipsize=Pango.EllipsizeMode.END)
        self.err_btn = Gtk.Button(css_classes=["flat"], visible=False, action_name="win.errors")
        status.append(self.spinner)
        status.append(self.status)
        status.append(self.err_btn)
        main.append(status)
        return main

    # ---------- context menus

    def _attach_context(self, widget, resolver):
        pop = Gtk.PopoverMenu.new_from_model(self.ctx_menu)
        pop.set_parent(widget)
        pop.set_has_arrow(False)
        pop.set_halign(Gtk.Align.START)
        g = Gtk.GestureClick(button=3)
        g.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)

        def pressed(_g, _n, x, y):
            targets = resolver(x, y)
            if not targets:
                return
            self.ctx_targets = targets
            self.a_ctx_trash.set_enabled(all(not (t.is_dir and t.node is self.root) for t in targets))
            self.a_ctx_rescan.set_enabled(not self.scanning)
            rect = Gdk.Rectangle()
            rect.x, rect.y, rect.width, rect.height = int(x), int(y), 1, 1
            pop.set_pointing_to(rect)
            pop.popup()
        g.connect("pressed", pressed)
        widget.add_controller(g)

    def _row_targets(self, view, multisel, x, y):
        row = row_at(view, x, y)
        if row is None:
            return None
        if multisel is not None:
            sel = self._selected_rows()
            if row in sel and len(sel) > 1:
                return [Target.from_row(r) for r in sel]
            pos = self._position_of(self.contents_sort, lambda r: r is row)
            if pos is not None:
                multisel.select_item(pos, True)
        return [Target.from_row(row)]

    def _table_targets(self, table, x, y):
        d = table.item_at(x, y)
        return [Target(parent=d[0], f=d[1])] if d else None

    def _treemap_targets(self, x, y):
        r = self.treemap.hit(x, y)
        if r is None or (r[4] is None and r[5] is None):
            return None
        return [Target(node=r[4])] if r[4] is not None else [Target(parent=r[6], f=r[5])]

    def _ctx(self, fn):
        if self.ctx_targets:
            fn(self.ctx_targets[0])

    # ---------- scanning

    @property
    def scanning(self):
        return self.scanner is not None and not self.scanner.finished

    def choose_folder(self):
        dlg = Gtk.FileDialog(title="Choose a folder to scan")

        def done(d, res):
            try:
                f = d.select_folder_finish(res)
            except GLib.Error:
                return
            if f and f.get_path():
                self.start_scan(f.get_path())
        dlg.select_folder(self, None, done)

    def start_scan(self, path):
        path = os.path.realpath(os.path.expanduser(path))
        if not os.path.isdir(path):
            self.alert("Can't scan that", f"{path} is not a folder.")
            return
        if self.scanner is not None:
            self.scanner.cancel()
        self.root = DirNode(path)
        try:
            self.root_dev = os.stat(path).st_dev
        except OSError as e:
            self.alert("Can't scan that", str(e))
            return
        self.all_errors = []
        self.tree_stores = {}
        self.contents = None
        self.current = None
        self.stack.set_visible_child_name("main")
        self.tree_root_store.remove_all()
        self.tree_root_store.append(Row(self.root, root=True))
        row = self.tree_model.get_row(0)
        if row is not None:
            row.set_expanded(True)
        self.show_folder(self.root)
        self.settings["last_path"] = path
        self._run_scanner(Scanner(self.root, self.lock, STATE["onefs"]))

    def rescan(self, node):
        if node is None or self.scanning:
            return
        with self.lock:
            p = node.parent
            while p is not None:
                p.size -= node.size
                p.alloc -= node.alloc
                p.nfiles -= node.nfiles
                p.ndirs -= node.ndirs
                p = p.parent
            node.dirs = []
            node.files = []
            node.size = node.alloc = node.nfiles = node.ndirs = 0
            node.scanned = False
            node.error = None
        entry = self.tree_stores.get(id(node))
        if entry is not None:
            entry[1].remove_all()
            entry[2] = 0
        if self.current is not None and self.current.is_inside(node):
            self.show_folder(node)
        self._run_scanner(Scanner(node, self.lock, STATE["onefs"], dev=self.root_dev))

    def _run_scanner(self, scanner):
        self.scanner = scanner
        scanner.start()
        self.spinner.start()
        self.stop_btn.set_visible(True)
        self.rescan_btn.set_sensitive(False)
        if self.tick_id is None:
            self.tick_id = GLib.timeout_add(TICK_MS, self._tick)

    def stop_scan(self):
        if self.scanning:
            self.scanner.cancel()

    def _tick(self):
        self.tick_count += 1
        finished = self.scanner is None or self.scanner.finished
        self.sync_views()
        if self.tick_count % 6 == 0 or finished:
            self._refresh_page()
        self._update_status()
        if finished:
            self.tick_id = None
            self.spinner.stop()
            self.stop_btn.set_visible(False)
            self.rescan_btn.set_sensitive(True)
            if self.scanner is not None:
                self.all_errors.extend(self.scanner.errors)
            self._update_status()
            return False
        return True

    def _update_status(self):
        s = self.scanner
        if s is None or self.root is None:
            self.status.set_label("")
            return
        if not s.finished:
            rate = s.nfiles / s.elapsed if s.elapsed > 0 else 0
            text = (f"Scanning {s.path} … {fmt_int(s.nfiles)} files · {fmt_int(s.ndirs)} folders · "
                    f"{human(s.bytes)} · {s.elapsed:.0f}s ({fmt_int(int(rate))} files/s)")
            nerr = len(self.all_errors) + len(s.errors)
        else:
            r = self.root
            text = (f"{r.path}: {human(r.value(STATE['ondisk']))} in {fmt_int(r.nfiles)} files, "
                    f"{fmt_int(r.ndirs)} folders")
            text += " · scan stopped (partial)" if s.cancelled else f" · scanned in {s.elapsed:.1f}s"
            try:
                st = os.statvfs(r.path)
                total = st.f_blocks * st.f_frsize
                free = st.f_bavail * st.f_frsize
                text += f" · drive: {human(free)} free of {human(total)}"
            except OSError:
                pass
            nerr = len(self.all_errors)
        self.status.set_label(text)
        self.err_btn.set_visible(nerr > 0)
        self.err_btn.set_label(f"⚠ {fmt_int(nerr)} unreadable")

    # ---------- keeping list stores in sync with the scan

    def _tree_children(self, item):
        if not item.is_dir:
            return None
        node = item.node
        with self.lock:
            dirs = list(node.dirs)
            scanned = node.scanned
        rows = [Row(d, parent=node) for d in dirs if visible(d.name)]
        if scanned and not rows:
            return None
        store = Gio.ListStore(item_type=Row)
        store.splice(0, 0, rows)
        self.tree_stores[id(node)] = [node, store, len(dirs)]
        return store

    def sync_views(self):
        for entry in list(self.tree_stores.values()):
            node, store, synced = entry
            with self.lock:
                new = node.dirs[synced:]
                entry[2] = len(node.dirs)
            if new:
                store.splice(store.get_n_items(), 0, [Row(d, parent=node) for d in new if visible(d.name)])
            for i in range(store.get_n_items()):
                store.get_item(i).refresh()
        for i in range(self.tree_root_store.get_n_items()):
            self.tree_root_store.get_item(i).refresh()

        if self.contents is not None:
            node, store, synced, files_added = self.contents
            with self.lock:
                new = node.dirs[synced:]
                self.contents[2] = len(node.dirs)
                files = node.files if (node.scanned and not files_added) else None
                if files is not None:
                    self.contents[3] = True
            add = [Row(d, parent=node) for d in new if visible(d.name)]
            if files:
                add += [Row(f=f, parent=node) for f in files if visible(f[0])]
            if add:
                store.splice(store.get_n_items(), 0, add)
            for i in range(store.get_n_items()):
                store.get_item(i).refresh()
            self._update_summary()

        for view in (self.tree_view, self.contents_view):
            at_top = view.get_vadjustment() is None or view.get_vadjustment().get_value() < 1
            view.get_sorter().changed(Gtk.SorterChange.DIFFERENT)
            if at_top and view.get_model().get_n_items():
                view.scroll_to(0, None, Gtk.ListScrollFlags.NONE, None)
        if self.notebook.get_current_page() == 4:
            self.treemap.queue_draw()

    # ---------- navigation

    def _rel(self, node):
        if self.root is None:
            return node.path
        if node is self.root:
            return "."
        return os.path.relpath(node.path, self.root.path)

    def show_folder(self, node, select_file=None, from_tree=False):
        if node is None:
            return
        self.current = node
        store = Gio.ListStore(item_type=Row)
        with self.lock:
            dirs = list(node.dirs)
            files = list(node.files) if node.scanned else []
            scanned = node.scanned
        rows = [Row(d, parent=node) for d in dirs if visible(d.name)]
        rows += [Row(f=f, parent=node) for f in files if visible(f[0])]
        store.splice(0, 0, rows)
        self.contents = [node, store, len(dirs), scanned]
        self.contents_sort.set_model(store)
        self._update_crumbs()
        self._update_summary()
        if not from_tree:
            self._select_in_tree(node)
        if select_file is not None:
            pos = self._position_of(self.contents_sort, lambda r: r.f is select_file)
            if pos is not None:
                self.notebook.set_current_page(0)
                self.contents_sel.select_item(pos, True)
                self.contents_view.scroll_to(pos, None, Gtk.ListScrollFlags.FOCUS, None)
        self._refresh_page()

    def go_up(self):
        if self.current is not None and self.current.parent is not None:
            self.show_folder(self.current.parent)

    def goto(self, t):
        if t.is_dir:
            self.show_folder(t.node)
        else:
            self.show_folder(t.parent, select_file=t.f)

    def _on_tree_selected(self, sel, _pspec):
        if self._syncing:
            return
        item = sel.get_selected_item()
        if item is not None:
            row = unwrap(item)
            if row.node is not self.current:
                self.show_folder(row.node, from_tree=True)

    def _on_contents_activate(self, _view, pos):
        row = self.contents_sort.get_item(pos)
        if row is None:
            return
        if row.is_dir:
            self.show_folder(row.node)
        else:
            self.open_default(Target.from_row(row))

    @staticmethod
    def _position_of(model, pred):
        for i in range(model.get_n_items()):
            if pred(unwrap(model.get_item(i))):
                return i
        return None

    def _select_in_tree(self, node):
        chain, n = [], node
        while n is not None:
            chain.append(n)
            n = n.parent
        chain.reverse()
        self._syncing = True
        try:
            pos = None
            for target in chain:
                pos = self._position_of(self.tree_sort, lambda r, t=target: r.node is t)
                if pos is None:
                    return
                if target is not node:
                    self.tree_sort.get_item(pos).set_expanded(True)
            self.tree_sel.set_selected(pos)
            self.tree_view.scroll_to(pos, None, Gtk.ListScrollFlags.NONE, None)
        finally:
            self._syncing = False

    def _update_crumbs(self):
        while (c := self.crumbs.get_first_child()) is not None:
            self.crumbs.remove(c)
        chain, n = [], self.current
        while n is not None:
            chain.append(n)
            n = n.parent
        for i, node in enumerate(reversed(chain)):
            if i:
                self.crumbs.append(Gtk.Label(label="›", css_classes=["dim-label"]))
            b = Gtk.Button(label=node.path if node.parent is None else node.name, css_classes=["flat"])
            b.connect("clicked", lambda _b, nd=node: self.show_folder(nd))
            self.crumbs.append(b)

    def _update_summary(self):
        n = self.current
        if n is None:
            return
        self.folder_summary.set_label(
            f"{human(n.value(STATE['ondisk']))} · {fmt_int(n.nfiles)} files · {fmt_int(n.ndirs)} folders")

    # ---------- analysis tabs

    def _refresh_page(self):
        page = self.notebook.get_current_page()
        if self.current is None:
            return False
        if page == 4:
            self.treemap.set_node(self.current)
        elif page in (1, 2, 3):
            self.stats_gen += 1
            gen, node, ondisk = self.stats_gen, self.current, STATE["ondisk"]

            def work():
                result = compute_stats(node, ondisk)
                GLib.idle_add(self._apply_stats, gen, node, result)
            threading.Thread(target=work, daemon=True).start()
        return False

    def _apply_stats(self, gen, node, result):
        if gen != self.stats_gen:
            return False
        top, exts, ages = result
        total = sum(e[0] for e in exts.values()) or 1
        self.top_table.set_rows(top)
        self.types_table.set_rows([(ext, e[0], e[1], e[2], e[0] / total) for ext, e in exts.items()])
        self.ages_table.set_rows([(AGE_BUCKETS[i][0], a[0], a[1], i, a[0] / total) for i, a in enumerate(ages)])
        return False

    # ---------- settings toggles

    def _update_mode_label(self):
        self.mode_label.set_label("size on disk" if STATE["ondisk"] else "")

    def on_mode_changed(self):
        self._update_mode_label()
        self.sync_views()
        self._update_status()
        self._refresh_page()

    def on_hidden_changed(self):
        if self.root is None:
            return
        current = self.current
        self.tree_stores = {}
        self.tree_root_store.remove_all()
        self.tree_root_store.append(Row(self.root, root=True))
        row = self.tree_model.get_row(0)
        if row is not None:
            row.set_expanded(True)
        while current is not None and not visible(current.name) and current.parent is not None:
            current = current.parent
        self.show_folder(current or self.root)

    # ---------- actions

    def open_default(self, t):
        try:
            Gio.AppInfo.launch_default_for_uri(Gio.File.new_for_path(t.path).get_uri(), None)
        except GLib.Error as e:
            self.alert("Couldn't open", e.message)

    def reveal(self, t):
        uri = Gio.File.new_for_path(t.path).get_uri()
        try:
            bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
            bus.call_sync("org.freedesktop.FileManager1", "/org/freedesktop/FileManager1",
                          "org.freedesktop.FileManager1", "ShowItems",
                          GLib.Variant("(ass)", ([uri], "")), None, Gio.DBusCallFlags.NONE, 3000, None)
        except GLib.Error:
            folder = t.path if t.is_dir else t.parent.path
            Gio.AppInfo.launch_default_for_uri(Gio.File.new_for_path(folder).get_uri(), None)

    def open_terminal(self, t):
        folder = t.path if t.is_dir else t.parent.path
        for cmd in TERMINALS:
            if shutil.which(cmd[0]):
                subprocess.Popen([c.format(folder) for c in cmd], cwd=folder, start_new_session=True)
                return
        self.alert("No terminal found", "Install a terminal emulator such as Ptyxis or GNOME Terminal.")

    def copy_paths(self, targets):
        if targets:
            self.get_clipboard().set("\n".join(t.path for t in targets))

    def _selected_rows(self):
        bits = self.contents_sel.get_selection()
        rows = []
        ok, it, pos = Gtk.BitsetIter.init_first(bits)
        while ok:
            rows.append(self.contents_sort.get_item(pos))
            ok, pos = it.next()
        return rows

    def trash_selection(self):
        if self.stack.get_visible_child_name() != "main" or self.notebook.get_current_page() != 0:
            return
        rows = self._selected_rows()
        if rows:
            self.trash([Target.from_row(r) for r in rows])

    def trash(self, targets):
        targets = [t for t in targets if not (t.is_dir and t.node is self.root)]
        if not targets or self.scanning:
            if self.scanning:
                self.alert("Scan in progress", "Wait for the scan to finish (or stop it) before deleting.")
            return
        total = sum(t.size for t in targets)
        names = "\n".join(t.path for t in targets[:8]) + (f"\n… and {len(targets) - 8} more" if len(targets) > 8 else "")
        what = f"“{os.path.basename(targets[0].path)}”" if len(targets) == 1 else f"{len(targets)} items"
        dlg = Gtk.AlertDialog(message=f"Move {what} to the Trash?",
                              detail=f"{names}\n\n{human(total)} will be moved to the Trash.",
                              buttons=["Cancel", "Move to Trash"], cancel_button=0, default_button=0)

        def chosen(d, res):
            try:
                if d.choose_finish(res) != 1:
                    return
            except GLib.Error:
                return
            self._run_delete(targets, permanent=False)
        dlg.choose(self, None, chosen)

    def _run_delete(self, targets, permanent):
        self.spinner.start()
        self.status.set_label(("Deleting" if permanent else "Moving to Trash") + f" {len(targets)} item(s)…")

        def work():
            done, failed = [], []
            for t in targets:
                try:
                    if permanent:
                        if t.is_dir:
                            shutil.rmtree(t.path)
                        else:
                            os.remove(t.path)
                    else:
                        Gio.File.new_for_path(t.path).trash(None)
                    done.append(t)
                except GLib.Error as e:
                    unsupported = e.matches(Gio.io_error_quark(), Gio.IOErrorEnum.NOT_SUPPORTED)
                    failed.append((t, e.message, unsupported))
                except OSError as e:
                    failed.append((t, e.strerror or str(e), False))
            GLib.idle_add(self._after_delete, done, failed, permanent)
        threading.Thread(target=work, daemon=True).start()

    def _after_delete(self, done, failed, permanent):
        self.spinner.stop()
        for t in done:
            self._remove_from_model(t)
        self.sync_views()
        self._update_status()
        self._refresh_page()
        no_trash = [t for t, _, unsupported in failed if unsupported]
        other = [(t, msg) for t, msg, unsupported in failed if not unsupported]
        if other:
            self.alert("Some items couldn't be removed",
                       "\n".join(f"{t.path}: {msg}" for t, msg in other[:15]))
        if no_trash and not permanent:
            total = sum(t.size for t in no_trash)
            dlg = Gtk.AlertDialog(
                message="This drive has no Trash",
                detail=(f"{len(no_trash)} item(s), {human(total)}, can't be moved to the Trash here "
                        f"(typical for network shares).\n\nDelete them permanently instead? This can't be undone."),
                buttons=["Cancel", "Delete Permanently"], cancel_button=0, default_button=0)

            def chosen(d, res):
                try:
                    if d.choose_finish(res) == 1:
                        self._run_delete(no_trash, permanent=True)
                except GLib.Error:
                    pass
            dlg.choose(self, None, chosen)
        return False

    def _remove_from_model(self, t):
        with self.lock:
            par = t.parent
            if t.is_dir:
                node = t.node
                if node not in par.dirs:
                    return
                par.dirs.remove(node)
                dsize, dalloc, dfiles, ddirs = node.size, node.alloc, node.nfiles, node.ndirs + 1
            else:
                if t.f not in par.files:
                    return
                par.files.remove(t.f)
                dsize, dalloc, dfiles, ddirs = t.f[1], t.f[2], 1, 0
            p = par
            while p is not None:
                p.size -= dsize
                p.alloc -= dalloc
                p.nfiles -= dfiles
                p.ndirs -= ddirs
                p = p.parent
        if t.is_dir:
            entry = self.tree_stores.get(id(par))
            if entry is not None:
                self._store_remove(entry[1], lambda r: r.node is t.node)
                entry[2] -= 1
        if self.contents is not None and self.contents[0] is par:
            self._store_remove(self.contents[1], (lambda r: r.node is t.node) if t.is_dir else (lambda r: r.f is t.f))
            if t.is_dir:
                self.contents[2] -= 1
        if t.is_dir and self.current is not None and self.current.is_inside(t.node):
            self.show_folder(par)

    @staticmethod
    def _store_remove(store, pred):
        for i in range(store.get_n_items()):
            if pred(store.get_item(i)):
                store.remove(i)
                return

    def export_csv(self):
        if self.contents is None:
            return
        node = self.contents[0]
        dlg = Gtk.FileDialog(title="Export folder contents", initial_name=f"disklens-{node.name or 'root'}.csv")

        def done(d, res):
            try:
                f = d.save_finish(res)
            except GLib.Error:
                return
            path = f.get_path()
            try:
                with open(path, "w", newline="") as fh:
                    w = csv.writer(fh)
                    w.writerow(["Name", "Type", "Size (bytes)", "Size on disk (bytes)", "Files", "Folders",
                                "Modified", "Path"])
                    for i in range(self.contents_sort.get_n_items()):
                        r = self.contents_sort.get_item(i)
                        if r.is_dir:
                            n = r.node
                            w.writerow([n.name, "Folder", n.size, n.alloc, n.nfiles, n.ndirs, fmt_time(n.mtime),
                                        n.path])
                        else:
                            fl = r.f
                            w.writerow([fl[0], r.props.kind_text, fl[1], fl[2], "", "", fmt_time(fl[3]), r.path])
            except OSError as e:
                self.alert("Export failed", str(e))
        dlg.save(self, None, done)

    def show_errors(self):
        errs = list(self.all_errors)
        if self.scanner is not None and not self.scanner.finished:
            errs += self.scanner.errors
        win = Gtk.Window(title=f"Unreadable items ({len(errs)})", transient_for=self, modal=True,
                         default_width=800, default_height=450)
        tv = Gtk.TextView(editable=False, monospace=True, left_margin=8, top_margin=8)
        lines = [f"{p}  —  {m}" for p, m in errs[:2000]]
        if len(errs) > 2000:
            lines.append(f"… and {len(errs) - 2000} more")
        tv.get_buffer().set_text("\n".join(lines))
        win.set_child(Gtk.ScrolledWindow(child=tv))
        win.present()

    def show_about(self):
        Gtk.AboutDialog(transient_for=self, modal=True, program_name="DiskLens", version=VERSION,
                        comments="See what's using your disk space.", logo_icon_name="drive-harddisk",
                        license_type=Gtk.License.MIT_X11).present()

    def alert(self, message, detail=""):
        Gtk.AlertDialog(message=message, detail=detail).show(self)

    def _on_close(self, *_):
        if self.scanner is not None:
            self.scanner.cancel()
        self.settings.update({
            "width": self.get_width(), "height": self.get_height(),
            "pane": self.paned.get_position(),
            "ondisk": STATE["ondisk"], "hidden": STATE["hidden"], "onefs": STATE["onefs"],
        })
        save_settings(self.settings)
        return False


CSS = """
.data-table progressbar trough, .data-table progressbar progress { min-height: 8px; }
.data-table progressbar trough { min-width: 30px; }
.numeric { font-feature-settings: "tnum"; }
"""


class DiskLensApp(Gtk.Application):
    def __init__(self):
        super().__init__(application_id=APP_ID, flags=Gio.ApplicationFlags.HANDLES_OPEN | Gio.ApplicationFlags.NON_UNIQUE)
        self.settings = load_settings()
        for k in STATE:
            if k in self.settings:
                STATE[k] = bool(self.settings[k])

    def do_startup(self):
        Gtk.Application.do_startup(self)
        css = Gtk.CssProvider()
        css.load_from_string(CSS)
        Gtk.StyleContext.add_provider_for_display(Gdk.Display.get_default(), css,
                                                  Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)
        quit_action = Gio.SimpleAction.new("quit", None)
        quit_action.connect("activate", lambda *_: [w.close() for w in self.get_windows()])
        self.add_action(quit_action)
        self.set_accels_for_action("app.quit", ["<Control>q"])

    def _window(self):
        win = DiskLensWindow(self, self.settings)
        win.present()
        return win

    def do_activate(self):
        self._window()

    def do_open(self, files, _n, _hint):
        win = self._window()
        if files and files[0].get_path():
            win.start_scan(files[0].get_path())


if __name__ == "__main__":
    sys.exit(DiskLensApp().run(sys.argv))
