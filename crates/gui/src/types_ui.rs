//! Actions on a file type (a row on the Types tab): show only that type's files on the
//! Biggest tab, or move every file of that type in the current folder to the Trash.
//! A child module of `window`, so it can use `State`.

use std::rc::Rc;

use adw::prelude::*;
use disklens_core::stats;
use disklens_core::util::{fmt_int, human};
use disklens_core::NONE;
use gtk::gio;

use super::actions::{show_menu, Target};
use super::State;
use crate::row::RowObject;

pub fn add_actions(st: &Rc<State>) {
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
    add("type-show", |st| {
        if let Some((key, desc)) = st.ctx_type.borrow().clone() {
            st.show_type(key, desc);
        }
    });
    add("type-trash", |st| {
        if let Some((key, desc)) = st.ctx_type.borrow().clone() {
            st.trash_type(&key, &desc);
        }
    });
    add("type-filter-clear", |st| st.set_type_filter(None));

    // double-click a type: show its files
    let w = Rc::downgrade(st);
    st.ui.pages.types.view.connect_activate(move |_, pos| {
        let Some(st) = w.upgrade() else { return };
        let Some(row) = st.ui.pages.types.sorted.item(pos).and_downcast::<RowObject>() else { return };
        if let Some(key) = row.type_key() {
            st.show_type(key, row.name());
        }
    });
}

fn type_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    let s1 = gio::Menu::new();
    s1.append(Some("Show Files"), Some("win.type-show"));
    let s2 = gio::Menu::new();
    s2.append(Some("Move All to Trash…"), Some("win.type-trash"));
    menu.append_section(None, &s1);
    menu.append_section(None, &s2);
    menu
}

impl State {
    /// Right-click on a row of the Types tab.
    pub(super) fn type_context(self: &Rc<Self>, cell: &gtk::Widget, key: String, desc: String, x: f64, y: f64) {
        let Some(list) = cell.ancestor(gtk::ColumnView::static_type()) else { return };
        let (px, py) = cell
            .compute_point(&list, &gtk::graphene::Point::new(x as f32, y as f32))
            .map(|p| (p.x() as f64, p.y() as f64))
            .unwrap_or((x, y));
        if let Some(a) = self.ui.window.lookup_action("type-trash").and_downcast::<gio::SimpleAction>() {
            a.set_enabled(!self.is_scanning() && !self.is_remote());
        }
        *self.ctx_type.borrow_mut() = Some((key, desc));
        show_menu(&list, &type_menu(), px, py);
    }

    /// Biggest tab, limited to one type, in the current folder and below.
    pub(super) fn show_type(&self, key: String, desc: String) {
        self.set_type_filter(Some((key, desc)));
        self.set_page("biggest");
    }

    /// Limit the Biggest tab to one type (`None` shows every type again).
    pub(super) fn set_type_filter(&self, filter: Option<(String, String)>) {
        match &filter {
            Some((key, desc)) => {
                self.ui.pages.banner.set_title(&format!("Showing only {desc} files ({key})"));
                self.ui.pages.banner.set_revealed(true);
            }
            None => self.ui.pages.banner.set_revealed(false),
        }
        *self.type_filter.borrow_mut() = filter;
        self.stats_dirty.set(true);
        self.refresh_stats(true);
    }

    /// Move every file of type `key` under the current folder to the Trash, after asking.
    fn trash_type(self: &Rc<Self>, key: &str, desc: &str) {
        if self.is_remote() {
            self.toast("NAS scans are read-only: delete files on the NAS itself");
            return;
        }
        let dir = self.current.get();
        if dir == NONE {
            return;
        }
        let (targets, size, folder): (Vec<Target>, u64, String) = {
            let tree = self.tree.borrow();
            let refs = stats::files_of_type(&tree, dir, key, self.show_hidden.get());
            let ondisk = self.ondisk.get();
            let size = refs.iter().map(|r| tree.get(r.dir).files[r.idx as usize].value(ondisk)).sum();
            let folder = if tree.get(dir).parent == NONE { self.root_label() } else { tree.get(dir).name_str().into_owned() };
            (refs.into_iter().map(|r| Target::File(r.dir, r.idx)).collect(), size, folder)
        };
        if targets.is_empty() {
            self.toast(&format!("No {desc} files left in {folder}"));
            return;
        }
        let n = targets.len() as u64;
        let heading = if n == 1 {
            format!("Move the only {desc} file to the Trash?")
        } else {
            format!("Move all {} {desc} files to the Trash?", fmt_int(n))
        };
        let intro = format!(
            "Every {desc} file ({key}) in {folder} and its subfolders: {} file{}, {}.",
            fmt_int(n),
            if n == 1 { "" } else { "s" },
            human(size)
        );
        self.trash_titled(targets, Some(heading), Some(intro));
    }
}
