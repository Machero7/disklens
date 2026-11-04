//! The Copies tab: find files with identical contents under the current folder, mark the
//! extra copies and move them to the Trash. A child module of `window`, so it can use `State`.

use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use disklens_core::dups::{self, Progress, PHASES};
use disklens_core::util::{fmt_int, human};
use disklens_core::NONE;
use gtk::{gio, glib};

use super::actions::{self, Target};
use super::State;
use crate::dupitem::DupItem;

const REMOTE_TEXT: &str = "Finding copies compares file contents, so it works on folders on this computer, not on NAS scans.";
const IDLE_TEXT: &str = "Finds files with identical contents in the folder you're viewing, including its subfolders.";

pub struct DupUi {
    pub widget: gtk::Box,
    where_lbl: gtk::Label,
    min_spin: gtk::SpinButton,
    find_btn: gtk::Button,
    progress: gtk::ProgressBar,
    summary: gtk::Label,
    mark_btns: gtk::Box,
    trash_btn: gtk::Button,
    root: gio::ListStore,
    pub view: gtk::ColumnView,
    /// Rows that stack vertically in narrow windows.
    pub top: gtk::Box,
    pub bar: gtk::Box,
    pub narrow_hide: Vec<gtk::ColumnViewColumn>,
}

fn dup_cell(prop: &'static str, xalign: f32, dim: bool) -> gtk::SignalListItemFactory {
    let f = gtk::SignalListItemFactory::new();
    f.connect_setup(move |_, obj| {
        let li = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let l = gtk::Label::builder().xalign(xalign).ellipsize(gtk::pango::EllipsizeMode::Middle).build();
        l.add_css_class("numeric");
        if dim {
            l.add_css_class("dim-label");
        }
        li.set_child(Some(&l));
        actions::attach_ctx(&l, li);
        li.property_expression("item")
            .chain_property::<gtk::TreeListRow>("item")
            .chain_property::<DupItem>(prop)
            .bind(&l, "label", gtk::Widget::NONE);
    });
    f
}

pub fn build_page(min_mb: f64) -> DupUi {
    let where_lbl = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::Middle).build();
    where_lbl.add_css_class("heading");
    let min_spin = gtk::SpinButton::with_range(0.0, 100_000.0, 1.0);
    min_spin.set_value(min_mb);
    min_spin.set_valign(gtk::Align::Center);
    min_spin.set_tooltip_text(Some("Ignore files smaller than this"));
    let min_lbl = gtk::Label::new(Some("Min. size (MB)"));
    min_lbl.add_css_class("dim-label");
    let find_btn = gtk::Button::builder().label("Find Duplicates").action_name("win.dups-find").build();
    find_btn.add_css_class("suggested-action");
    find_btn.add_css_class("pill");
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    top.append(&where_lbl);
    top.append(&min_lbl);
    top.append(&min_spin);
    top.append(&find_btn);

    let progress = gtk::ProgressBar::builder().show_text(true).visible(false).build();

    let summary = gtk::Label::builder().label(IDLE_TEXT).xalign(0.0).hexpand(true).wrap(true).build();
    summary.add_css_class("dim-label");
    let mark_btns = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    mark_btns.add_css_class("linked");
    for (label, action) in [("Keep Oldest", "win.dups-keep-oldest"), ("Keep Newest", "win.dups-keep-newest"), ("Clear Marks", "win.dups-clear")] {
        let b = gtk::Button::builder().label(label).action_name(action).build();
        mark_btns.append(&b);
    }
    mark_btns.set_tooltip_text(Some("Mark every copy except the oldest/newest in each group"));
    mark_btns.set_sensitive(false);
    let trash_btn = gtk::Button::builder().label("Move Marked to Trash").sensitive(false).build();
    trash_btn.add_css_class("destructive-action");
    trash_btn.set_action_name(Some("win.dups-trash"));
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    bar.append(&summary);
    bar.append(&mark_btns);
    bar.append(&trash_btn);

    // groups expand to their files
    let root = gio::ListStore::new::<DupItem>();
    let model = gtk::TreeListModel::new(root.clone(), false, true, |obj| {
        obj.downcast_ref::<DupItem>().and_then(|d| d.children()).map(|c| c.upcast())
    });
    let view = gtk::ColumnView::builder().reorderable(false).build();
    view.set_model(Some(&gtk::SingleSelection::builder().model(&model).autoselect(false).can_unselect(true).build()));

    // mark: a check box bound both ways to the item's "marked" (hidden on group rows)
    let mark_f = gtk::SignalListItemFactory::new();
    mark_f.connect_setup(|_, obj| {
        let li = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let c = gtk::CheckButton::new();
        c.set_tooltip_text(Some("Mark this copy for Move to Trash"));
        li.set_child(Some(&c));
    });
    mark_f.connect_bind(|_, obj| {
        let li = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let (Some(c), Some(item)) = (
            li.child().and_downcast::<gtk::CheckButton>(),
            li.item().and_downcast::<gtk::TreeListRow>().and_then(|r| r.item()).and_downcast::<DupItem>(),
        ) else {
            return;
        };
        c.set_visible(!item.is_group());
        let b = item.bind_property("marked", &c, "active").bidirectional().sync_create().build();
        // SAFETY: the binding is only ever stored and taken back under this key, as a glib::Binding
        unsafe { li.set_data("mark-binding", b) };
    });
    mark_f.connect_unbind(|_, obj| {
        let li = obj.downcast_ref::<gtk::ListItem>().unwrap();
        // SAFETY: see bind
        if let Some(b) = unsafe { li.steal_data::<glib::Binding>("mark-binding") } {
            b.unbind();
        }
    });

    let name_f = gtk::SignalListItemFactory::new();
    name_f.connect_setup(|_, obj| {
        let li = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let l = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Middle).build();
        let exp = gtk::TreeExpander::builder().child(&l).build();
        li.set_child(Some(&exp));
        actions::attach_ctx(&exp, li);
        let row = li.property_expression("item");
        row.bind(&exp, "list-row", gtk::Widget::NONE);
        let item = row.chain_property::<gtk::TreeListRow>("item");
        item.chain_property::<DupItem>("name").bind(&l, "label", gtk::Widget::NONE);
        gtk::ClosureExpression::new::<Vec<String>>(
            [item.chain_property::<DupItem>("is-group")],
            glib::closure!(|_: Option<glib::Object>, g: bool| if g { vec!["heading".to_string()] } else { vec![] }),
        )
        .bind(&l, "css-classes", gtk::Widget::NONE);
    });

    let mut narrow_hide = Vec::new();
    for (title, f, width, expand) in [
        ("", mark_f, 40, false),
        ("Name", name_f, 0, true),
        ("Size", dup_cell("size-text", 1.0, false), 95, false),
        ("Reclaimable", dup_cell("reclaim-text", 1.0, false), 110, false),
        ("Folder", dup_cell("folder", 0.0, true), 300, false),
        ("Modified", dup_cell("mtime-text", 0.0, true), 115, false),
    ] {
        let c = gtk::ColumnViewColumn::new(Some(title), Some(f));
        c.set_resizable(true);
        c.set_expand(expand);
        if width > 0 {
            c.set_fixed_width(width);
        }
        view.append_column(&c);
        if matches!(title, "Reclaimable" | "Folder" | "Modified") {
            narrow_hide.push(c);
        }
    }

    let scroll = gtk::ScrolledWindow::builder().child(&view).vexpand(true).build();
    scroll.add_css_class("card");
    scroll.add_css_class("contents-card");
    scroll.set_overflow(gtk::Overflow::Hidden);

    let widget = gtk::Box::new(gtk::Orientation::Vertical, 10);
    widget.set_margin_top(14);
    widget.set_margin_start(12);
    widget.set_margin_end(12);
    widget.set_margin_bottom(12);
    for w in [top.upcast_ref::<gtk::Widget>(), progress.upcast_ref(), bar.upcast_ref(), scroll.upcast_ref()] {
        widget.append(w);
    }
    top.set_margin_start(6);
    bar.set_margin_start(6);
    DupUi { widget, where_lbl, min_spin, find_btn, progress, summary, mark_btns, trash_btn, root, view, top, bar, narrow_hide }
}

pub fn wire(st: &Rc<State>) {
    let win = &st.ui.window;
    let add = |name: &str, f: fn(&Rc<State>)| {
        let a = gio::SimpleAction::new(name, None);
        let w = Rc::downgrade(st);
        a.connect_activate(move |_, _| {
            if let Some(st) = w.upgrade() {
                f(&st)
            }
        });
        win.add_action(&a);
    };
    add("dups-find", |st| st.dups_toggle());
    add("dups-keep-oldest", |st| st.dups_mark(Some(false)));
    add("dups-keep-newest", |st| st.dups_mark(Some(true)));
    add("dups-clear", |st| st.dups_mark(None));
    add("dups-trash", |st| st.dups_trash_marked());

    let w = Rc::downgrade(st);
    st.ui.dups.view.connect_activate(move |view, pos| {
        let Some(st) = w.upgrade() else { return };
        let item = view.model().and_then(|m| m.item(pos)).and_downcast::<gtk::TreeListRow>().and_then(|r| r.item()).and_downcast::<DupItem>();
        if let Some(r) = item.and_then(|i| i.file_ref()) {
            st.show_folder(r.dir, false);
            st.set_page("contents");
        }
    });
    let w = Rc::downgrade(st);
    st.ui.dups.min_spin.connect_value_changed(move |s| {
        if let Some(st) = w.upgrade() {
            st.settings.borrow_mut().set("dup_min_mb", s.value());
        }
    });
}

impl State {
    pub fn dups_running(&self) -> bool {
        self.dup_progress.borrow().is_some()
    }

    /// Called when the shown folder changes.
    pub(super) fn dups_update_where(&self) {
        if self.dup_progress.borrow().is_some() || self.current.get() == NONE || self.ui.dups.root.n_items() > 0 {
            return;
        }
        let path = self.tree.borrow().path(self.current.get());
        self.ui.dups.where_lbl.set_label(&format!("Search in {}", path.display()));
    }

    /// Forget results (their file references go stale when the tree is rescanned).
    pub(super) fn dups_reset(&self) {
        if let Some(p) = self.dup_progress.borrow().as_ref() {
            p.cancel.store(true, Ordering::Relaxed);
        }
        self.ui.dups.root.remove_all();
        // comparing contents would mean downloading every file from the NAS
        let remote = self.is_remote();
        self.ui.dups.find_btn.set_sensitive(!remote);
        self.ui.dups.min_spin.set_sensitive(!remote);
        self.ui.dups.summary.set_label(if remote { REMOTE_TEXT } else { IDLE_TEXT });
        self.dups_update_buttons();
        self.dups_update_where();
    }

    fn dups_toggle(self: &Rc<Self>) {
        if let Some(p) = self.dup_progress.borrow().as_ref() {
            p.cancel.store(true, Ordering::Relaxed);
            return;
        }
        let root = self.current.get();
        if root == NONE {
            return;
        }
        if self.is_scanning() {
            self.toast("Wait for the scan to finish before looking for duplicates");
            return;
        }
        if self.is_remote() {
            self.toast("Finding copies reads every file's contents, so it only works on folders on this computer");
            return;
        }
        let min = (self.ui.dups.min_spin.value() * 1024.0 * 1024.0) as u64;
        let cands = { dups::collect(&self.tree.borrow(), root, min, self.show_hidden.get()) };
        let path = self.tree.borrow().path(root);
        self.ui.dups.where_lbl.set_label(&format!("Searching {}", path.display()));
        self.ui.dups.root.remove_all();
        self.dups_update_buttons();
        let p = Arc::new(Progress::default());
        *self.dup_progress.borrow_mut() = Some(p.clone());
        self.ui.dups.find_btn.set_label("Stop");
        self.ui.dups.find_btn.remove_css_class("suggested-action");
        self.ui.dups.progress.set_visible(true);
        self.ui.dups.progress.set_fraction(0.0);

        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8);
        let (tx, rx) = std::sync::mpsc::channel();
        let pw = p.clone();
        std::thread::spawn(move || {
            let _ = tx.send(dups::find(cands, threads, &pw));
        });
        let w = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(200), move || {
            let Some(st) = w.upgrade() else { return glib::ControlFlow::Break };
            match rx.try_recv() {
                Ok(groups) => {
                    st.dups_finished(groups, &p);
                    glib::ControlFlow::Break
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    st.dups_finished(Vec::new(), &p);
                    glib::ControlFlow::Break
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    st.dups_show_progress(&p);
                    glib::ControlFlow::Continue
                }
            }
        });
    }

    fn dups_show_progress(&self, p: &Progress) {
        let phase = PHASES[p.phase.load(Ordering::Relaxed) as usize % PHASES.len()];
        let (db, tb) = (p.done_bytes.load(Ordering::Relaxed), p.total_bytes.load(Ordering::Relaxed));
        let (df, tf) = (p.done_files.load(Ordering::Relaxed), p.total_files.load(Ordering::Relaxed));
        let (frac, text) = if p.phase.load(Ordering::Relaxed) == 2 && tb > 0 {
            (db as f64 / tb as f64, format!("{phase}: {} of {}", human(db), human(tb)))
        } else if tf > 0 {
            (df as f64 / tf as f64, format!("{phase}: {} of {} files", fmt_int(df), fmt_int(tf)))
        } else {
            (0.0, format!("{phase}…"))
        };
        self.ui.dups.progress.set_fraction(frac.min(1.0));
        self.ui.dups.progress.set_text(Some(&text));
    }

    fn dups_finished(&self, groups: Vec<dups::Group>, p: &Progress) {
        *self.dup_progress.borrow_mut() = None;
        self.ui.dups.find_btn.set_label("Find Duplicates");
        self.ui.dups.find_btn.add_css_class("suggested-action");
        self.ui.dups.progress.set_visible(false);
        if p.cancelled() {
            self.ui.dups.summary.set_label("Stopped.");
            self.dups_update_where();
            return;
        }
        let searched = self.ui.dups.where_lbl.label().replacen("Searching ", "Results for ", 1);
        self.ui.dups.where_lbl.set_label(&searched);
        let items: Vec<DupItem> = {
            let tree = self.tree.borrow();
            let base = tree.path(self.current.get());
            let now = super::now();
            groups
                .iter()
                .map(|g| {
                    let gi = DupItem::group(g.size);
                    let kids: Vec<DupItem> = g
                        .members
                        .iter()
                        .map(|r| {
                            let d = tree.get(r.dir);
                            let f = &d.files[r.idx as usize];
                            let dir = tree.path(r.dir);
                            let rel = dir.strip_prefix(&base).map(|p| p.to_path_buf()).unwrap_or(dir);
                            let folder = if rel.as_os_str().is_empty() { ".".into() } else { rel.display().to_string() };
                            DupItem::file(*r, d.file_name(f).into_owned(), folder, f.size, f.mtime, crate::row::relative_date(f.mtime, now))
                        })
                        .collect();
                    if let Some(c) = gi.children() {
                        c.extend_from_slice(&kids);
                    }
                    for k in &kids {
                        let w = self.ui.dups.trash_btn.downgrade();
                        let root = self.ui.dups.root.clone();
                        k.connect_marked_notify(move |_| {
                            if let Some(b) = w.upgrade() {
                                update_trash_button(&b, &root);
                            }
                        });
                    }
                    gi.update_group();
                    gi
                })
                .collect()
        };
        self.ui.dups.root.extend_from_slice(&items);
        let errors = p.errors.load(Ordering::Relaxed);
        self.dups_summary(errors);
        self.dups_update_buttons();
    }

    fn dups_summary(&self, errors: u64) {
        let groups: Vec<DupItem> = self.ui.dups.root.iter::<DupItem>().flatten().collect();
        let mut s = if groups.is_empty() {
            "No duplicates found.".to_string()
        } else {
            let files: u32 = groups.iter().filter_map(|g| g.children()).map(|c| c.n_items()).sum();
            let reclaim: u64 = groups
                .iter()
                .map(|g| g.size() * (g.children().map(|c| c.n_items()).unwrap_or(1) as u64).saturating_sub(1))
                .sum();
            format!("{} groups · {} files · {} reclaimable", fmt_int(groups.len() as u64), fmt_int(files as u64), human(reclaim))
        };
        if errors > 0 {
            s.push_str(&format!(" · {} unreadable files skipped", fmt_int(errors)));
        }
        self.ui.dups.summary.set_label(&s);
    }

    fn dups_update_buttons(&self) {
        let any = self.ui.dups.root.n_items() > 0;
        self.ui.dups.mark_btns.set_sensitive(any);
        update_trash_button(&self.ui.dups.trash_btn, &self.ui.dups.root);
    }

    /// Some(newest_first): mark all but the oldest (false) or newest (true) copy; None: clear.
    fn dups_mark(&self, keep_newest: Option<bool>) {
        for g in self.ui.dups.root.iter::<DupItem>().flatten() {
            let Some(c) = g.children() else { continue };
            let mut kids: Vec<DupItem> = c.iter::<DupItem>().flatten().collect();
            match keep_newest {
                None => kids.iter().for_each(|k| k.set_marked(false)),
                Some(newest) => {
                    kids.sort_by(|a, b| (a.mtime(), a.sort_key()).cmp(&(b.mtime(), b.sort_key())));
                    if newest {
                        kids.reverse();
                    }
                    for (i, k) in kids.iter().enumerate() {
                        k.set_marked(i > 0);
                    }
                }
            }
        }
    }

    fn dups_trash_marked(self: &Rc<Self>) {
        let mut targets = Vec::new();
        let mut all_marked = 0;
        for g in self.ui.dups.root.iter::<DupItem>().flatten() {
            let Some(c) = g.children() else { continue };
            let kids: Vec<DupItem> = c.iter::<DupItem>().flatten().collect();
            let marked: Vec<&DupItem> = kids.iter().filter(|k| k.marked()).collect();
            if !marked.is_empty() && marked.len() == kids.len() {
                all_marked += 1;
                continue;
            }
            targets.extend(marked.iter().filter_map(|k| k.file_ref()).map(|r| Target::File(r.dir, r.idx)));
        }
        if all_marked > 0 {
            let dlg = adw::AlertDialog::new(
                Some("Every copy is marked"),
                Some(&format!("In {all_marked} group(s) all copies are marked. Unmark at least one copy in each group so one stays.")),
            );
            dlg.add_response("ok", "_OK");
            dlg.present(Some(&self.ui.window));
            return;
        }
        self.trash(targets);
    }

    /// Drop copies that were deleted (from here or any other tab) and groups left with one file.
    pub(super) fn dups_prune(&self) {
        let root = &self.ui.dups.root;
        if root.n_items() == 0 {
            return;
        }
        let tree = self.tree.borrow();
        let gone = |k: &DupItem| {
            k.file_ref().is_none_or(|r| !tree.is_attached(r.dir) || tree.get(r.dir).files[r.idx as usize].removed)
        };
        let mut i = 0;
        while i < root.n_items() {
            let g = root.item(i).and_downcast::<DupItem>().unwrap();
            if let Some(c) = g.children() {
                let mut j = 0;
                while j < c.n_items() {
                    if c.item(j).and_downcast::<DupItem>().is_some_and(|k| gone(&k)) {
                        c.remove(j);
                    } else {
                        j += 1;
                    }
                }
                if c.n_items() < 2 {
                    root.remove(i);
                    continue;
                }
                g.update_group();
            }
            i += 1;
        }
        drop(tree);
        self.dups_summary(0);
        self.dups_update_buttons();
    }
}

fn update_trash_button(b: &gtk::Button, root: &gio::ListStore) {
    let (mut n, mut size) = (0u64, 0u64);
    for g in root.iter::<DupItem>().flatten() {
        for k in g.children().into_iter().flat_map(|c| c.iter::<DupItem>().flatten().collect::<Vec<_>>()) {
            if k.marked() {
                n += 1;
                size += k.size();
            }
        }
    }
    b.set_sensitive(n > 0);
    b.set_label(&if n > 0 { format!("Move {n} Marked to Trash ({})", human(size)) } else { "Move Marked to Trash".to_string() });
}
