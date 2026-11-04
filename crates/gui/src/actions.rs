//! Right-click menus and what they do: open, show in the file manager, terminal, copy path,
//! go to folder, rescan, and Move to Trash (with confirmation, Undo, and a permanent-delete
//! fallback for drives without a Trash). A child module of `window`, so it can use `State`.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use disklens_core::util::human;
use disklens_core::{trash, Tree, NONE};
use gtk::{gio, glib};

use super::State;
use crate::row::RowObject;

/// A folder (by id) or a file (folder id, index into its files).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Dir(u32),
    File(u32, u32),
}

impl Target {
    pub fn from_row(r: &RowObject) -> Option<Self> {
        if r.dir() == NONE {
            return None; // summary row (file type / age group)
        }
        Some(match r.file_index() {
            None => Target::Dir(r.dir()),
            Some(i) => Target::File(r.dir(), i),
        })
    }

    pub fn path(&self, tree: &Tree) -> PathBuf {
        match *self {
            Target::Dir(d) => tree.path(d),
            Target::File(d, i) => tree.file_path(d, &tree.get(d).files[i as usize]),
        }
    }

    /// The folder itself, or the folder a file is in.
    pub fn folder(&self) -> u32 {
        match *self {
            Target::Dir(d) | Target::File(d, _) => d,
        }
    }

    /// The folder the item lives in (NONE for the scan root).
    pub fn parent(&self, tree: &Tree) -> u32 {
        match *self {
            Target::Dir(d) => tree.get(d).parent,
            Target::File(d, _) => d,
        }
    }

    pub fn size(&self, tree: &Tree, ondisk: bool) -> u64 {
        match *self {
            Target::Dir(d) => tree.get(d).value(ondisk),
            Target::File(d, i) => tree.get(d).files[i as usize].value(ondisk),
        }
    }
}

thread_local! {
    /// The window's state, for right-click gestures created inside list factories.
    static CURRENT: RefCell<Weak<State>> = RefCell::new(Weak::new());
}

pub fn set_current(st: &Rc<State>) {
    CURRENT.with(|c| *c.borrow_mut() = Rc::downgrade(st));
}

/// Right-click (or long-press) on a list cell opens the context menu for its row.
pub fn attach_ctx(widget: &impl IsA<gtk::Widget>, li: &gtk::ListItem) {
    let click = gtk::GestureClick::builder().button(3).build();
    let li_weak = li.downgrade();
    click.connect_pressed(move |g, _, x, y| {
        let (Some(li), Some(st)) = (li_weak.upgrade(), CURRENT.with(|c| c.borrow().upgrade())) else { return };
        let Some(item) = li.item() else { return };
        let row = match item.downcast::<gtk::TreeListRow>() {
            Ok(tr) => tr.item().and_downcast::<RowObject>(),
            Err(item) => item.downcast::<RowObject>().ok(),
        };
        let (Some(row), Some(w)) = (row, g.widget()) else { return };
        g.set_state(gtk::EventSequenceState::Claimed);
        st.row_context(&w, li.position(), &row, x, y);
    });
    widget.as_ref().add_controller(click);
}

const TERMINALS: &[&[&str]] = &[
    &["ptyxis", "--new-window", "--working-directory={}"],
    &["gnome-terminal", "--working-directory={}"],
    &["kgx", "--working-directory={}"],
    &["konsole", "--workdir", "{}"],
    &["xfce4-terminal", "--working-directory={}"],
    &["x-terminal-emulator"],
];

fn ctx_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    let s1 = gio::Menu::new();
    s1.append(Some("Open"), Some("win.ctx-open"));
    s1.append(Some("Show in File Manager"), Some("win.ctx-reveal"));
    s1.append(Some("Open Terminal Here"), Some("win.ctx-terminal"));
    s1.append(Some("Copy Path"), Some("win.ctx-copy"));
    let s2 = gio::Menu::new();
    s2.append(Some("Go to Folder"), Some("win.ctx-goto"));
    s2.append(Some("Rescan"), Some("win.ctx-rescan"));
    let s3 = gio::Menu::new();
    s3.append(Some("Move to Trash"), Some("win.ctx-trash"));
    for s in [s1, s2, s3] {
        menu.append_section(None, &s);
    }
    menu
}

/// Register the `win.ctx-*` and `win.trash-selection` actions.
pub fn add_actions(st: &Rc<State>) {
    let win = &st.ui.window;
    let add = |name: &str, accels: &[&str], f: fn(&Rc<State>)| {
        let a = gio::SimpleAction::new(name, None);
        let w = Rc::downgrade(st);
        a.connect_activate(move |_, _| {
            if let Some(st) = w.upgrade() {
                f(&st)
            }
        });
        win.add_action(&a);
        if !accels.is_empty() {
            win.application().unwrap().set_accels_for_action(&format!("win.{name}"), accels);
        }
    };
    add("ctx-open", &[], |st| st.with_first(|st, t| st.open_default(t)));
    add("ctx-reveal", &[], |st| st.with_first(|st, t| st.reveal(t)));
    add("ctx-terminal", &[], |st| st.with_first(|st, t| st.open_terminal(t)));
    add("ctx-copy", &[], |st| {
        let paths: Vec<String> = {
            let tree = st.tree.borrow();
            st.ctx_targets.borrow().iter().map(|t| t.path(&tree).display().to_string()).collect()
        };
        st.ui.window.clipboard().set_text(&paths.join("\n"));
        st.toast(if paths.len() == 1 { "Path copied" } else { "Paths copied" });
    });
    add("ctx-goto", &[], |st| {
        st.with_first(|st, t| {
            st.show_folder(t.folder(), false);
            st.set_page("contents");
        })
    });
    add("ctx-rescan", &[], |st| st.with_first(|st, t| st.rescan(t.folder())));
    add("ctx-trash", &[], |st| {
        let targets = st.ctx_targets.borrow().clone();
        st.trash(targets);
    });
    add("undo", &["<Control>z"], |st| {
        let last = st.last_trash.borrow_mut().take();
        match last {
            Some((paths, parents)) => st.undo_trash(paths, parents),
            None => st.toast("Nothing to undo"),
        }
    });
    add("trash-selection", &["Delete"], |st| {
        let targets = st.selected_targets();
        st.trash(targets);
    });
}

impl State {
    fn with_first(self: &Rc<Self>, f: impl FnOnce(&Rc<Self>, Target)) {
        let t = self.ctx_targets.borrow().first().copied();
        if let Some(t) = t {
            f(self, t);
        }
    }

    fn set_action_enabled(&self, name: &str, on: bool) {
        if let Some(a) = self.ui.window.lookup_action(name).and_downcast::<gio::SimpleAction>() {
            a.set_enabled(on);
        }
    }

    /// Show the menu for `targets` at (x, y) in `anchor`'s coordinates.
    pub fn popup_menu(self: &Rc<Self>, anchor: &gtk::Widget, targets: Vec<Target>, x: f64, y: f64) {
        if targets.is_empty() {
            return;
        }
        let scanning = self.is_scanning();
        let has_root = targets.iter().any(|t| *t == Target::Dir(Tree::ROOT));
        self.set_action_enabled("ctx-rescan", !scanning);
        self.set_action_enabled("ctx-trash", !scanning && !has_root);
        *self.ctx_targets.borrow_mut() = targets;
        let pop = gtk::PopoverMenu::from_model(Some(&ctx_menu()));
        pop.set_parent(anchor);
        pop.set_has_arrow(false);
        pop.set_halign(gtk::Align::Start);
        pop.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        pop.connect_closed(|p| {
            let p = p.clone();
            // unparent after the menu item's action has run
            glib::idle_add_local_once(move || p.unparent());
        });
        pop.popup();
    }

    /// Right-click on a list row. In multi-selection lists the click joins (or replaces)
    /// the selection, like in Files; the menu then acts on every selected row.
    fn row_context(self: &Rc<Self>, cell: &gtk::Widget, pos: u32, row: &RowObject, x: f64, y: f64) {
        let Some(t) = Target::from_row(row) else { return };
        let list: Option<gtk::Widget> = cell
            .ancestor(gtk::ColumnView::static_type())
            .or_else(|| cell.ancestor(gtk::ListView::static_type()));
        let Some(list) = list else { return };
        let mut targets = vec![t];
        if let Some(cv) = list.downcast_ref::<gtk::ColumnView>() {
            if let Some(sel) = cv.model() {
                if !sel.is_selected(pos) {
                    sel.select_item(pos, true);
                }
                targets = selected_in(&sel);
            }
        }
        let (px, py) = cell
            .compute_point(&list, &gtk::graphene::Point::new(x as f32, y as f32))
            .map(|p| (p.x() as f64, p.y() as f64))
            .unwrap_or((x, y));
        self.popup_menu(&list, targets, px, py);
    }

    /// Selected rows of the list on the visible tab (Folder or Biggest), for the Delete key.
    fn selected_targets(&self) -> Vec<Target> {
        let view = match self.ui.view_stack.visible_child_name().as_deref() {
            Some("contents") => &self.ui.contents_view,
            Some("biggest") => &self.ui.pages.biggest.view,
            _ => return Vec::new(),
        };
        if self.ui.stack.visible_child_name().as_deref() != Some("main") {
            return Vec::new();
        }
        view.model().map(|m| selected_in(&m)).unwrap_or_default()
    }

    /// For the screenshot walk-through: trash a child of the scan root without asking.
    pub fn trash_child_now(self: &Rc<Self>, name: &str) {
        let t = {
            let tree = self.tree.borrow();
            let r = tree.get(Tree::ROOT);
            r.dirs
                .iter()
                .find(|&&c| &*tree.get(c).name == name.as_bytes())
                .map(|&c| Target::Dir(c))
                .or_else(|| r.files.iter().position(|f| r.file_name(f) == name).map(|i| Target::File(Tree::ROOT, i as u32)))
        };
        if let Some(t) = t {
            let p = t.path(&self.tree.borrow());
            self.run_delete(vec![t], vec![p], false);
        }
    }

    /// For the screenshot walk-through: open the context menu on the Folder list's first row.
    pub fn menu_on_first_row(self: &Rc<Self>) {
        let row = self.ui.contents_sort.item(0).and_downcast::<RowObject>();
        if let Some(t) = row.as_ref().and_then(Target::from_row) {
            let anchor = self.ui.contents_view.clone().upcast::<gtk::Widget>();
            self.popup_menu(&anchor, vec![t], 160.0, 70.0);
        }
    }

    fn open_default(&self, t: Target) {
        let path = t.path(&self.tree.borrow());
        if let Err(e) = gio::AppInfo::launch_default_for_uri(&gio::File::for_path(&path).uri(), None::<&gio::AppLaunchContext>) {
            self.toast(&format!("Couldn't open: {}", e.message()));
        }
    }

    /// Ask the file manager to show (and select) the item; fall back to opening its folder.
    fn reveal(self: &Rc<Self>, t: Target) {
        let (path, folder) = {
            let tree = self.tree.borrow();
            (t.path(&tree), tree.path(t.folder()))
        };
        let uri = gio::File::for_path(&path).uri().to_string();
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let ok = match gio::bus_get_future(gio::BusType::Session).await {
                Ok(bus) => bus
                    .call_future(
                        Some("org.freedesktop.FileManager1"),
                        "/org/freedesktop/FileManager1",
                        "org.freedesktop.FileManager1",
                        "ShowItems",
                        Some(&(vec![uri], "").to_variant()),
                        None,
                        gio::DBusCallFlags::NONE,
                        3000,
                    )
                    .await
                    .is_ok(),
                Err(_) => false,
            };
            if !ok {
                let r = gio::AppInfo::launch_default_for_uri(&gio::File::for_path(&folder).uri(), None::<&gio::AppLaunchContext>);
                if let (Err(e), Some(st)) = (r, w.upgrade()) {
                    st.toast(&format!("Couldn't open the file manager: {}", e.message()));
                }
            }
        });
    }

    fn open_terminal(&self, t: Target) {
        let folder = self.tree.borrow().path(t.folder());
        for cmd in TERMINALS {
            if glib::find_program_in_path(cmd[0]).is_some() {
                let dir = folder.display().to_string();
                let args: Vec<String> = cmd[1..].iter().map(|a| a.replace("{}", &dir)).collect();
                if std::process::Command::new(cmd[0]).args(&args).current_dir(&folder).spawn().is_ok() {
                    return;
                }
            }
        }
        self.toast("No terminal found – install Ptyxis or GNOME Terminal");
    }

    // ---------------------------------------------------------------- trash

    pub fn trash(self: &Rc<Self>, targets: Vec<Target>) {
        if targets.is_empty() {
            return;
        }
        if self.is_scanning() {
            self.toast("Wait for the scan to finish (or stop it) before deleting");
            return;
        }
        let targets: Vec<Target> = targets.into_iter().filter(|t| *t != Target::Dir(Tree::ROOT)).collect();
        if targets.is_empty() {
            return;
        }
        let (paths, total) = {
            let tree = self.tree.borrow();
            let ondisk = self.ondisk.get();
            (
                targets.iter().map(|t| t.path(&tree)).collect::<Vec<_>>(),
                targets.iter().map(|t| t.size(&tree, ondisk)).sum::<u64>(),
            )
        };
        let heading = if paths.len() == 1 {
            format!("Move “{}” to the Trash?", paths[0].file_name().unwrap_or_default().to_string_lossy())
        } else {
            format!("Move {} items to the Trash?", paths.len())
        };
        let mut body: String = paths.iter().take(8).map(|p| format!("{}\n", p.display())).collect();
        if paths.len() > 8 {
            body.push_str(&format!("… and {} more\n", paths.len() - 8));
        }
        body.push_str(&format!("\n{} will be moved to the Trash. You can undo this.", human(total)));
        let dlg = adw::AlertDialog::new(Some(&heading), Some(&body));
        dlg.add_responses(&[("cancel", "_Cancel"), ("trash", "_Move to Trash")]);
        dlg.set_response_appearance("trash", adw::ResponseAppearance::Destructive);
        dlg.set_default_response(Some("cancel"));
        dlg.set_close_response("cancel");
        let w = Rc::downgrade(self);
        dlg.connect_response(None, move |_, resp| {
            if let (Some(st), "trash") = (w.upgrade(), resp) {
                st.run_delete(targets.clone(), paths.clone(), false);
            }
        });
        dlg.present(Some(&self.ui.window));
    }

    /// Trash (or permanently delete) in a worker thread, then update the tree and views.
    fn run_delete(self: &Rc<Self>, targets: Vec<Target>, paths: Vec<PathBuf>, permanent: bool) {
        let w = Rc::downgrade(self);
        self.ui.progress.set_visible(true);
        self.ui.progress.pulse();
        glib::spawn_future_local(async move {
            let jobs = paths.clone();
            // per item: None = done, Some((message, trash unsupported))
            let results: Vec<Option<(String, bool)>> = gio::spawn_blocking(move || {
                jobs.iter()
                    .map(|p| {
                        if permanent {
                            let r = if p.is_dir() && !p.is_symlink() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
                            r.err().map(|e| (e.to_string(), false))
                        } else {
                            gio::File::for_path(p)
                                .trash(None::<&gio::Cancellable>)
                                .err()
                                .map(|e| (e.message().to_string(), e.matches(gio::IOErrorEnum::NotSupported)))
                        }
                    })
                    .collect()
            })
            .await
            .unwrap_or_default();
            if let Some(st) = w.upgrade() {
                st.ui.progress.set_visible(false);
                st.after_delete(targets, paths, results, permanent);
            }
        });
    }

    fn after_delete(self: &Rc<Self>, targets: Vec<Target>, paths: Vec<PathBuf>, results: Vec<Option<(String, bool)>>, permanent: bool) {
        let mut done = Vec::new();
        let mut no_trash = Vec::new();
        let mut failed = Vec::new();
        for ((t, p), r) in targets.iter().zip(&paths).zip(&results) {
            match r {
                None => done.push((*t, p.clone())),
                Some((_, true)) if !permanent => no_trash.push((*t, p.clone())),
                Some((msg, _)) => failed.push(format!("{}: {msg}", p.display())),
            }
        }
        let freed: u64 = {
            let tree = self.tree.borrow();
            done.iter().map(|(t, _)| t.size(&tree, self.ondisk.get())).sum()
        };
        let parents: Vec<u32> = {
            let tree = self.tree.borrow();
            done.iter().map(|(t, _)| t.parent(&tree)).collect()
        };
        for (t, _) in &done {
            self.remove_from_views(*t);
        }
        self.sync_views();
        self.update_status(true);

        if !done.is_empty() {
            let text = if done.len() == 1 {
                let name = done[0].1.file_name().unwrap_or_default().to_string_lossy().into_owned();
                if permanent { format!("Deleted “{name}”") } else { format!("Moved “{name}” to the Trash") }
            } else if permanent {
                format!("Deleted {} items ({})", done.len(), human(freed))
            } else {
                format!("Moved {} items ({}) to the Trash", done.len(), human(freed))
            };
            let toast = adw::Toast::builder().title(text).timeout(10).build();
            if !permanent {
                toast.set_button_label(Some("Undo"));
                let w = Rc::downgrade(self);
                let restore: Vec<PathBuf> = done.iter().map(|(_, p)| p.clone()).collect();
                *self.last_trash.borrow_mut() = Some((restore, parents));
                toast.connect_button_clicked(move |_| {
                    if let Some(st) = w.upgrade() {
                        if let Some((p, d)) = st.last_trash.borrow_mut().take() {
                            st.undo_trash(p, d);
                        }
                    }
                });
            }
            if let Some(o) = self.ui.window.content().and_downcast::<adw::ToastOverlay>() {
                o.add_toast(toast.clone());
            }
            if let Some(old) = self.trash_toast.replace(Some(toast)) {
                old.dismiss();
            }
        }
        if !failed.is_empty() {
            let mut body = failed.iter().take(15).cloned().collect::<Vec<_>>().join("\n");
            if failed.len() > 15 {
                body.push_str(&format!("\n… and {} more", failed.len() - 15));
            }
            adw::AlertDialog::builder()
                .heading("Some items couldn't be removed")
                .body(body)
                .close_response("ok")
                .build()
                .tap(|d| d.add_response("ok", "_OK"))
                .present(Some(&self.ui.window));
        }
        if !no_trash.is_empty() {
            let total: u64 = {
                let tree = self.tree.borrow();
                no_trash.iter().map(|(t, _)| t.size(&tree, self.ondisk.get())).sum()
            };
            let dlg = adw::AlertDialog::new(
                Some("This drive has no Trash"),
                Some(&format!(
                    "{} item(s), {}, can't be moved to the Trash here (typical for network shares).\n\nDelete them permanently instead? This can't be undone.",
                    no_trash.len(),
                    human(total)
                )),
            );
            dlg.add_responses(&[("cancel", "_Cancel"), ("delete", "_Delete Permanently")]);
            dlg.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
            dlg.set_default_response(Some("cancel"));
            dlg.set_close_response("cancel");
            let w = Rc::downgrade(self);
            dlg.connect_response(None, move |_, resp| {
                if let (Some(st), "delete") = (w.upgrade(), resp) {
                    let (t, p): (Vec<_>, Vec<_>) = no_trash.iter().cloned().unzip();
                    st.run_delete(t, p, true);
                }
            });
            dlg.present(Some(&self.ui.window));
        }
    }

    /// Take a deleted item out of the tree and every list showing it.
    fn remove_from_views(self: &Rc<Self>, t: Target) {
        let parent = { t.parent(&self.tree.borrow()) };
        let removed = {
            let mut tree = self.tree.borrow_mut();
            match t {
                Target::Dir(d) => tree.remove_dir(d),
                Target::File(d, i) => tree.remove_file(d, i),
            }
        };
        if !removed {
            return;
        }
        let ndirs = { self.tree.borrow().get(parent).dirs.len() };
        let matches = |r: &RowObject| Target::from_row(r) == Some(t);
        if let Target::Dir(_) = t {
            if let Some((store, synced)) = self.side_stores.borrow_mut().get_mut(&parent) {
                remove_where(store, &matches);
                *synced = (*synced).min(ndirs);
            }
        }
        let current = self.current.get();
        if let Some(c) = self.contents.borrow_mut().as_mut() {
            if c.dir == parent {
                remove_where(&c.store, &matches);
                c.synced = c.synced.min(ndirs);
            }
        }
        if let Target::Dir(d) = t {
            let inside = { self.tree.borrow().is_inside(current, d) };
            if inside {
                self.show_folder(parent, false);
            }
        }
    }

    /// Put trashed items back, then rescan the folder that holds them all.
    fn undo_trash(self: &Rc<Self>, paths: Vec<PathBuf>, parents: Vec<u32>) {
        if let Some(t) = self.trash_toast.take() {
            t.dismiss();
        }
        if self.is_scanning() {
            self.toast("Wait for the scan to finish before undoing");
            return;
        }
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let errors: Vec<String> = gio::spawn_blocking(move || {
                paths
                    .iter()
                    .filter_map(|p| trash::restore(p, &trash::trash_dirs_for(p)).err().map(|e| e.to_string()))
                    .collect()
            })
            .await
            .unwrap_or_default();
            let Some(st) = w.upgrade() else { return };
            let common = { common_ancestor(&st.tree.borrow(), &parents) };
            if common != NONE {
                st.rescan(common);
            }
            if errors.is_empty() {
                st.toast("Restored from the Trash");
            } else {
                st.toast(&format!("Couldn't restore: {}", errors.join("; ")));
            }
        });
    }
}

/// Right-click on the treemap.
pub fn attach_map_ctx(st: &Rc<State>) {
    let click = gtk::GestureClick::builder().button(3).build();
    let w = Rc::downgrade(st);
    click.connect_pressed(move |_, _, x, y| {
        let Some(st) = w.upgrade() else { return };
        let target = {
            let hits = st.map_hits.borrow();
            crate::treemap::hit(&hits, x, y).and_then(|i| {
                let h = &hits[i];
                if h.dir != NONE {
                    Some(Target::Dir(h.dir))
                } else {
                    h.file.map(|f| Target::File(h.parent, f))
                }
            })
        };
        if let Some(t) = target {
            let anchor = st.ui.map.clone().upcast::<gtk::Widget>();
            st.popup_menu(&anchor, vec![t], x, y);
        }
    });
    st.ui.map.add_controller(click);
}

fn selected_in(sel: &gtk::SelectionModel) -> Vec<Target> {
    let bits = sel.selection();
    let mut v = Vec::new();
    if let Some((mut iter, first)) = gtk::BitsetIter::init_first(&bits) {
        let mut pos = Some(first);
        while let Some(p) = pos {
            if let Some(t) = sel.item(p).and_downcast::<RowObject>().as_ref().and_then(Target::from_row) {
                v.push(t);
            }
            pos = iter.next();
        }
    }
    v
}

fn remove_where(store: &gio::ListStore, pred: &dyn Fn(&RowObject) -> bool) {
    if let Some(i) = (0..store.n_items()).find(|&i| store.item(i).and_downcast::<RowObject>().is_some_and(|r| pred(&r))) {
        store.remove(i);
    }
}

/// Deepest folder containing all of `dirs` (NONE if the list is empty).
fn common_ancestor(tree: &Tree, dirs: &[u32]) -> u32 {
    let chain = |d: u32| {
        let mut v = Vec::new();
        let mut n = d;
        while n != NONE {
            v.push(n);
            n = tree.get(n).parent;
        }
        v.reverse();
        v
    };
    let mut iter = dirs.iter().filter(|&&d| d != NONE);
    let Some(&first) = iter.next() else { return NONE };
    let mut common = chain(first);
    for &d in iter {
        let c = chain(d);
        let n = common.iter().zip(&c).take_while(|(a, b)| a == b).count();
        common.truncate(n);
    }
    common.last().copied().unwrap_or(NONE)
}

/// Small helper so a builder chain can call a `&self` method and keep going.
trait Tap: Sized {
    fn tap(self, f: impl FnOnce(&Self)) -> Self {
        f(&self);
        self
    }
}
impl<T> Tap for T {}
