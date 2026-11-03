//! Biggest Files, File Types and File Ages: lists computed from the current folder's
//! whole subtree with [`disklens_core::stats::compute`]. They are filled only while
//! their tab is showing, and at most about once a second during a scan.

use adw::prelude::*;
use disklens_core::stats::{self, AGE_BUCKETS};
use disklens_core::util::{fmt_int, human};
use disklens_core::Tree;
use gtk::gio;

use crate::row::RowObject;
use crate::window::{label_cell, name_cell, share_cell, sorter};

/// How many files the Biggest page lists.
pub const TOP_N: usize = 1000;

pub struct ListPage {
    pub widget: gtk::Box,
    pub view: gtk::ColumnView,
    pub sorted: gtk::SortListModel,
    summary: gtk::Label,
    store: gio::ListStore,
    /// Columns hidden in narrow windows.
    pub narrow_hide: Vec<gtk::ColumnViewColumn>,
}

pub enum Col {
    Name(&'static str),
    Size,
    Share,
    Files,
    Detail(&'static str, i32),
    Modified,
    Type,
}

impl ListPage {
    /// `default_size_sort`: sort by size, largest first (otherwise keep insertion order).
    fn new(cols: &[Col], default_size_sort: bool, order_sort_name: bool) -> Self {
        let summary = gtk::Label::builder().xalign(0.0).wrap(true).build();
        summary.add_css_class("dim-label");
        summary.add_css_class("numeric");
        summary.set_margin_top(14);
        summary.set_margin_start(18);
        summary.set_margin_end(18);

        let view = gtk::ColumnView::builder().reorderable(false).build();
        let sorted = gtk::SortListModel::new(None::<gio::ListModel>, view.sorter());
        view.set_model(Some(&gtk::MultiSelection::new(Some(sorted.clone()))));
        let mut narrow_hide = Vec::new();
        let mut size_col = None;
        for c in cols {
            let (title, factory, srt, width, expand, narrow): (&str, _, _, i32, bool, bool) = match c {
                Col::Name(t) => (
                    t,
                    name_cell(),
                    if order_sort_name {
                        sorter(|a, b| a.sort_mtime().cmp(&b.sort_mtime()))
                    } else {
                        sorter(|a, b| a.sort_name().cmp(&b.sort_name()))
                    },
                    0,
                    true,
                    false,
                ),
                Col::Size => ("Size", label_cell("size-text", 1.0, false), sorter(|a, b| a.size().cmp(&b.size())), 95, false, false),
                Col::Share => ("Share", share_cell(), sorter(|a, b| a.size().cmp(&b.size())), 190, false, false),
                Col::Files => ("Files", label_cell("files-text", 1.0, true), sorter(|a, b| a.sort_files().cmp(&b.sort_files())), 85, false, true),
                Col::Detail(t, w) => (t, label_cell("detail", 0.0, true), sorter(|a, b| a.detail().cmp(&b.detail())), *w, *w == 0, true),
                Col::Modified => ("Modified", label_cell("mtime-text", 0.0, true), sorter(|a, b| a.sort_mtime().cmp(&b.sort_mtime())), 115, false, true),
                Col::Type => ("Type", label_cell("kind-text", 0.0, true), sorter(|a, b| a.kind_text().cmp(&b.kind_text())), 170, false, true),
            };
            let col = gtk::ColumnViewColumn::new(Some(title), Some(factory));
            col.set_sorter(Some(&srt));
            col.set_resizable(true);
            col.set_expand(expand);
            if width > 0 {
                col.set_fixed_width(width);
            }
            view.append_column(&col);
            if narrow {
                narrow_hide.push(col.clone());
            }
            if matches!(c, Col::Size) {
                size_col = Some(col);
            }
        }
        if default_size_sort {
            view.sort_by_column(size_col.as_ref(), gtk::SortType::Descending);
        }

        let scroll = gtk::ScrolledWindow::builder().child(&view).vexpand(true).build();
        scroll.add_css_class("card");
        scroll.add_css_class("contents-card");
        scroll.set_overflow(gtk::Overflow::Hidden);
        scroll.set_margin_start(12);
        scroll.set_margin_end(12);
        scroll.set_margin_bottom(12);
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 10);
        widget.append(&summary);
        widget.append(&scroll);
        let store = gio::ListStore::new::<RowObject>();
        sorted.set_model(Some(&store));
        ListPage { widget, view, sorted, summary, store, narrow_hide }
    }

    fn set_rows(&self, rows: &[RowObject], summary: &str) {
        self.store.splice(0, self.store.n_items(), rows);
        self.summary.set_label(summary);
    }
}

pub struct Pages {
    pub biggest: ListPage,
    pub types: ListPage,
    pub ages: ListPage,
}

impl Pages {
    pub fn new() -> Self {
        Pages {
            biggest: ListPage::new(
                &[Col::Name("Name"), Col::Size, Col::Share, Col::Detail("Folder", 0), Col::Modified, Col::Type],
                true,
                false,
            ),
            types: ListPage::new(&[Col::Name("Type"), Col::Detail("Extension", 120), Col::Size, Col::Share, Col::Files], true, false),
            ages: ListPage::new(&[Col::Name("Last modified"), Col::Size, Col::Share, Col::Files], false, true),
        }
    }

    pub fn is_stats_page(name: &str) -> bool {
        matches!(name, "biggest" | "types" | "ages")
    }

    /// Recompute all three lists for the subtree under `root`.
    #[allow(clippy::too_many_arguments)]
    pub fn fill(
        &self,
        tree: &Tree,
        root: u32,
        ondisk: bool,
        show_hidden: bool,
        now: i64,
        kind: &dyn Fn(&str) -> (String, gio::Icon),
    ) {
        let st = stats::compute(tree, root, ondisk, TOP_N, show_hidden, now);
        let root_path = tree.path(root);
        let root_name = if tree.get(root).parent == disklens_core::NONE {
            root_path.display().to_string()
        } else {
            tree.get(root).name_str().into_owned()
        };

        // Biggest files
        let rows: Vec<RowObject> = st
            .top
            .iter()
            .map(|r| {
                let d = tree.get(r.dir);
                let f = &d.files[r.idx as usize];
                let row = RowObject::for_file_in(tree, r.dir, r.idx, root, kind(&d.file_name(f)), ondisk, now);
                let rel = tree.path(r.dir);
                let rel = rel.strip_prefix(&root_path).unwrap_or(&rel);
                row.set_rank(Some(0));
                row.set_detail(if rel.as_os_str().is_empty() { ".".to_string() } else { rel.display().to_string() });
                row
            })
            .collect();
        let top_sum: u64 = rows.iter().map(|r| r.size()).sum();
        let pct = if st.total > 0 { top_sum as f64 * 100.0 / st.total as f64 } else { 0.0 };
        let nfiles: u64 = st.exts.values().map(|e| e.count).sum();
        let text = if rows.len() as u64 >= nfiles {
            format!("All {} files in {root_name}, biggest first.", fmt_int(nfiles))
        } else {
            format!(
                "The {} biggest of {} files in {root_name} take {} – {pct:.0} % of its {}.",
                fmt_int(rows.len() as u64),
                fmt_int(nfiles),
                human(top_sum),
                human(st.total)
            )
        };
        self.biggest.set_rows(&rows, &text);

        // File types, biggest first; the top four get the accent colours
        let mut exts: Vec<_> = st.exts.iter().collect();
        exts.sort_by(|a, b| b.1.size.cmp(&a.1.size).then_with(|| a.0.cmp(b.0)));
        let rows: Vec<RowObject> = exts
            .iter()
            .enumerate()
            .map(|(i, (ext, e))| {
                let (desc, icon) = kind(&e.sample);
                let row = RowObject::summary(&desc, ext, "text-x-generic-symbolic", e.size, st.total, e.count, i as i64);
                row.set_gicon(Some(icon));
                row.set_rank((i < 4 && e.size > 0).then_some(i));
                row
            })
            .collect();
        self.types.set_rows(&rows, &format!("{} file types in {root_name}.", fmt_int(rows.len() as u64)));

        // File ages, oldest group last
        let rows: Vec<RowObject> = AGE_BUCKETS
            .iter()
            .zip(st.ages.iter())
            .enumerate()
            .map(|(i, ((label, _), (size, count)))| {
                let row = RowObject::summary(label, "", "document-open-recent-symbolic", *size, st.total, *count, i as i64);
                row.set_rank(Some(0));
                row
            })
            .collect();
        let old: u64 = st.ages.iter().skip(4).map(|a| a.0).sum();
        self.ages.set_rows(
            &rows,
            &format!("Files in {root_name} by when they were last changed. {} hasn't been touched in over 2 years.", human(old)),
        );
    }
}
