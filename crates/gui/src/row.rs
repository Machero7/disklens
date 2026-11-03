//! `RowObject`: one folder or file in the sidebar tree or the Contents list.
//! Display properties are refreshed from the [`Tree`] on every tick while scanning;
//! widgets bind to them with property expressions, so they update live.

use std::cell::{Cell, RefCell};

use disklens_core::util::{fmt_int, human};
use disklens_core::{Tree, NONE};
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::gio;

mod imp {
    use super::*;

    #[derive(glib::Properties, Default)]
    #[properties(wrapper_type = super::RowObject)]
    pub struct RowObject {
        #[property(get, set)]
        pub name: RefCell<String>,
        #[property(get, set)]
        pub size_text: RefCell<String>,
        #[property(get, set)]
        pub fraction: Cell<f64>,
        #[property(get, set)]
        pub pct_text: RefCell<String>,
        #[property(get, set)]
        pub files_text: RefCell<String>,
        #[property(get, set)]
        pub mtime_text: RefCell<String>,
        #[property(get, set)]
        pub kind_text: RefCell<String>,
        #[property(get, set, nullable)]
        pub gicon: RefCell<Option<gio::Icon>>,
        /// CSS classes for the accent wrapper around the icon and bar ("accent-0".."accent-3", "accent-none").
        #[property(get, set)]
        pub accent: RefCell<Vec<String>>,

        pub dir: Cell<u32>,
        pub file: Cell<u32>,
        pub parent: Cell<u32>,
        pub sort_name: RefCell<String>,
        pub sort_size: Cell<u64>,
        pub sort_files: Cell<i64>,
        pub sort_mtime: Cell<i64>,
        pub cache: Cell<Option<(u64, u64, u64, i64, bool)>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RowObject {
        const NAME: &'static str = "DiskLensRow";
        type Type = super::RowObject;
    }

    #[glib::derived_properties]
    impl ObjectImpl for RowObject {}
}

glib::wrapper! {
    pub struct RowObject(ObjectSubclass<imp::RowObject>);
}

pub const FOLDER_ICON: &str = "folder-symbolic";
const WARN_ICON: &str = "dialog-warning-symbolic";

impl RowObject {
    fn blank(dir: u32, file: u32, parent: u32, name: String) -> Self {
        let r: Self = glib::Object::new();
        let imp = r.imp();
        imp.dir.set(dir);
        imp.file.set(file);
        imp.parent.set(parent);
        *imp.sort_name.borrow_mut() = name.to_lowercase();
        r.set_name(name);
        r.set_accent(vec!["accent-none".to_string()]);
        r
    }

    /// A folder row. `full_path` labels it with the whole path (the scan root).
    pub fn for_dir(tree: &Tree, dir: u32, parent: u32, full_path: bool, ondisk: bool, now: i64) -> Self {
        let d = tree.get(dir);
        let name = if full_path { tree.path(dir).display().to_string() } else { d.name_str().into_owned() };
        let r = Self::blank(dir, NONE, parent, name);
        r.set_kind_text("Folder");
        r.set_gicon(Some(gio::ThemedIcon::new(FOLDER_ICON).upcast::<gio::Icon>()));
        r.refresh(tree, ondisk, now);
        r
    }

    pub fn for_file(tree: &Tree, parent: u32, idx: u32, kind: (String, gio::Icon), ondisk: bool, now: i64) -> Self {
        let d = tree.get(parent);
        let f = &d.files[idx as usize];
        let r = Self::blank(parent, idx, parent, d.file_name(f).into_owned());
        r.set_kind_text(kind.0);
        r.set_gicon(Some(kind.1));
        r.refresh(tree, ondisk, now);
        r
    }

    pub fn is_dir(&self) -> bool {
        self.imp().file.get() == NONE
    }

    /// The folder this row shows (for folders) or lives in (for files).
    pub fn dir(&self) -> u32 {
        self.imp().dir.get()
    }

    pub fn file_index(&self) -> Option<u32> {
        let f = self.imp().file.get();
        (f != NONE).then_some(f)
    }

    pub fn size(&self) -> u64 {
        self.imp().sort_size.get()
    }

    pub fn sort_name(&self) -> String {
        self.imp().sort_name.borrow().clone()
    }

    pub fn sort_files(&self) -> i64 {
        self.imp().sort_files.get()
    }

    pub fn sort_mtime(&self) -> i64 {
        self.imp().sort_mtime.get()
    }

    pub fn refresh(&self, tree: &Tree, ondisk: bool, now: i64) {
        let imp = self.imp();
        let (v, nfiles, mtime, error) = match self.file_index() {
            None => {
                let d = tree.get(imp.dir.get());
                (d.value(ondisk), d.nfiles, d.mtime, d.error.is_some())
            }
            Some(i) => {
                let f = &tree.get(imp.dir.get()).files[i as usize];
                (f.value(ondisk), 0, f.mtime, false)
            }
        };
        let parent = imp.parent.get();
        let pv = if parent == NONE { v } else { tree.get(parent).value(ondisk) };
        let key = (v, pv, nfiles, mtime, error);
        if imp.cache.get() == Some(key) {
            return;
        }
        imp.cache.set(Some(key));
        imp.sort_size.set(v);
        imp.sort_mtime.set(mtime);
        imp.sort_files.set(if self.is_dir() { nfiles as i64 } else { -1 });
        let frac = if pv > 0 { v as f64 / pv as f64 } else if parent == NONE { 1.0 } else { 0.0 };
        self.set_size_text(human(v));
        self.set_fraction(frac.min(1.0));
        self.set_pct_text(format!("{:.1} %", frac * 100.0));
        self.set_mtime_text(relative_date(mtime, now));
        if self.is_dir() {
            self.set_files_text(fmt_int(nfiles));
            if error {
                self.set_kind_text("No access");
                self.set_gicon(Some(gio::ThemedIcon::new(WARN_ICON).upcast::<gio::Icon>()));
            }
        }
    }

    /// Colour slot shared by the usage strip, the bar and the icon.
    pub fn set_rank(&self, rank: Option<usize>) {
        let class = match rank {
            Some(r) => format!("accent-{r}"),
            None => "accent-none".to_string(),
        };
        if self.imp().accent.borrow().first() != Some(&class) {
            self.set_accent(vec![class]);
        }
    }
}

/// "Today", "Yesterday", "3 days ago", "Oct 12", "2024-05-03".
pub fn relative_date(t: i64, now: i64) -> String {
    if t <= 0 {
        return String::new();
    }
    let (Ok(then), Ok(today)) = (glib::DateTime::from_unix_local(t), glib::DateTime::from_unix_local(now)) else {
        return String::new();
    };
    let days = today.ymd();
    let td = then.ymd();
    if td == days {
        return "Today".into();
    }
    let delta = (now - t) / 86_400;
    if delta <= 1 && then.day_of_year() + 1 == today.day_of_year() && td.0 == days.0 {
        return "Yesterday".into();
    }
    if delta < 7 {
        return format!("{} days ago", delta.max(2));
    }
    if td.0 == days.0 {
        format!("{} {}", then.format("%b").unwrap_or_default(), td.2)
    } else {
        then.format("%Y-%m-%d").map(|s| s.to_string()).unwrap_or_default()
    }
}
