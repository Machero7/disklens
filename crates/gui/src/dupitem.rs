//! `DupItem`: a row on the Copies tab, either a group of identical files or one file in it.

use std::cell::{Cell, OnceCell, RefCell};

use disklens_core::stats::FileRef;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

mod imp {
    use super::*;

    #[derive(glib::Properties, Default)]
    #[properties(wrapper_type = super::DupItem)]
    pub struct DupItem {
        #[property(get, set)]
        pub name: RefCell<String>,
        #[property(get, set)]
        pub size_text: RefCell<String>,
        #[property(get, set)]
        pub reclaim_text: RefCell<String>,
        #[property(get, set)]
        pub folder: RefCell<String>,
        #[property(get, set)]
        pub mtime_text: RefCell<String>,
        /// Marked for Move to Trash (files only).
        #[property(get, set)]
        pub marked: Cell<bool>,
        #[property(get)]
        pub is_group: Cell<bool>,

        pub file: Cell<Option<FileRef>>,
        pub size: Cell<u64>,
        pub mtime: Cell<i64>,
        pub sort_key: RefCell<String>,
        pub children: OnceCell<gio::ListStore>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DupItem {
        const NAME: &'static str = "DiskLensDupItem";
        type Type = super::DupItem;
    }

    #[glib::derived_properties]
    impl ObjectImpl for DupItem {}
}

glib::wrapper! {
    pub struct DupItem(ObjectSubclass<imp::DupItem>);
}

impl DupItem {
    pub fn group(size: u64) -> Self {
        let g: Self = glib::Object::new();
        let imp = g.imp();
        imp.is_group.set(true);
        imp.size.set(size);
        let _ = imp.children.set(gio::ListStore::new::<DupItem>());
        g.set_size_text(disklens_core::util::human(size));
        g
    }

    pub fn file(r: FileRef, name: String, folder: String, size: u64, mtime: i64, mtime_text: String) -> Self {
        let f: Self = glib::Object::new();
        let imp = f.imp();
        imp.file.set(Some(r));
        imp.size.set(size);
        imp.mtime.set(mtime);
        *imp.sort_key.borrow_mut() = format!("{folder}/{name}");
        f.set_name(name);
        f.set_folder(folder);
        f.set_mtime_text(mtime_text);
        f.set_size_text(disklens_core::util::human(size));
        f
    }

    pub fn children(&self) -> Option<gio::ListStore> {
        self.imp().children.get().cloned()
    }

    pub fn file_ref(&self) -> Option<FileRef> {
        self.imp().file.get()
    }

    pub fn size(&self) -> u64 {
        self.imp().size.get()
    }

    pub fn mtime(&self) -> i64 {
        self.imp().mtime.get()
    }

    pub fn sort_key(&self) -> String {
        self.imp().sort_key.borrow().clone()
    }

    /// Refresh a group's label and reclaimable space from its current members.
    pub fn update_group(&self) {
        let n = self.children().map(|c| c.n_items()).unwrap_or(0) as u64;
        self.set_name(format!("{n} identical files"));
        self.set_reclaim_text(disklens_core::util::human(self.size() * n.saturating_sub(1)));
    }
}
