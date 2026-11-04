//! CSV export, the Preferences dialog, the keyboard-shortcuts window and saving settings.
//! A child module of `window`, so it can use `State`.

use std::io::Write;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use super::actions::Target;
use super::State;
use crate::row::RowObject;

/// Quote a CSV field when it needs it (RFC 4180).
pub fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn iso_time(t: i64) -> String {
    glib::DateTime::from_unix_local(t)
        .ok()
        .and_then(|d| d.format("%Y-%m-%d %H:%M:%S").ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

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
        win.application().unwrap().set_accels_for_action(&format!("win.{name}"), accels);
    };
    add("export", &["<Control>e"], |st| st.export_csv());
    add("preferences", &["<Control>comma"], |st| st.show_preferences());
    add("shortcuts", &["<Control>question", "<Control>F1"], |st| st.show_shortcuts());
}

impl State {
    /// Save the list on the Folder or Biggest tab, in its current sort order.
    fn export_csv(self: &Rc<Self>) {
        let page = self.ui.view_stack.visible_child_name().unwrap_or_default();
        let (model, what): (gtk::SortListModel, &str) = match page.as_str() {
            "biggest" => (self.ui.pages.biggest.sorted.clone(), "biggest"),
            _ => (self.ui.contents_sort.clone(), "contents"),
        };
        if self.current.get() == disklens_core::NONE || model.n_items() == 0 {
            self.toast("Nothing to export yet");
            return;
        }
        let name = {
            let tree = self.tree.borrow();
            let d = tree.get(self.current.get());
            let n = if d.parent == disklens_core::NONE { "root".to_string() } else { d.name_str().into_owned() };
            format!("disklens-{n}-{what}.csv")
        };
        let dlg = gtk::FileDialog::builder().title("Export to CSV").initial_name(name).build();
        let w = Rc::downgrade(self);
        dlg.save(Some(&self.ui.window), None::<&gio::Cancellable>, move |res| {
            let (Some(st), Ok(f)) = (w.upgrade(), res) else { return };
            let Some(path) = f.path() else { return };
            match st.write_csv(&model, &path) {
                Ok(n) => st.toast(&format!("Exported {n} rows to {}", path.file_name().unwrap_or_default().to_string_lossy())),
                Err(e) => st.toast(&format!("Export failed: {e}")),
            }
        });
    }

    fn write_csv(&self, model: &gtk::SortListModel, path: &std::path::Path) -> std::io::Result<usize> {
        let tree = self.tree.borrow();
        let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
        writeln!(out, "Name,Type,Size (bytes),Size on disk (bytes),Files,Folders,Modified,Path")?;
        let mut n = 0;
        for i in 0..model.n_items() {
            let Some(row) = model.item(i).and_downcast::<RowObject>() else { continue };
            let Some(t) = Target::from_row(&row) else { continue };
            let p = t.path(&tree).display().to_string();
            let fields: Vec<String> = match t {
                Target::Dir(d) => {
                    let d = tree.get(d);
                    vec![
                        row.name(),
                        "Folder".into(),
                        d.size.to_string(),
                        d.alloc.to_string(),
                        d.nfiles.to_string(),
                        d.ndirs.to_string(),
                        iso_time(d.mtime),
                        p,
                    ]
                }
                Target::File(d, idx) => {
                    let f = &tree.get(d).files[idx as usize];
                    vec![row.name(), row.kind_text(), f.size.to_string(), f.alloc.to_string(), String::new(), String::new(), iso_time(f.mtime), p]
                }
            };
            let line: Vec<String> = fields.iter().map(|s| csv_field(s)).collect();
            writeln!(out, "{}", line.join(","))?;
            n += 1;
        }
        out.flush()?;
        Ok(n)
    }

    /// For the screenshot walk-through: write the Folder list to `path`.
    pub fn export_contents_to(&self, path: &std::path::Path) -> std::io::Result<usize> {
        self.write_csv(&self.ui.contents_sort.clone(), path)
    }

    pub fn show_preferences(self: &Rc<Self>) {
        let dlg = adw::PreferencesDialog::builder().title("Preferences").build();
        let page = adw::PreferencesPage::new();
        let sizes = adw::PreferencesGroup::builder().title("Sizes").build();
        let scanning = adw::PreferencesGroup::builder().title("Scanning").build();
        let rows = [
            (&sizes, "ondisk", "Show size on disk", "Count the space files actually take up on the drive instead of their length. Differs for sparse and very small files."),
            (&scanning, "hidden", "Show hidden files", "Include files and folders whose names start with a dot."),
            (&scanning, "onefs", "Stay on one drive", "Don't follow into other drives and network shares mounted inside the scanned folder. Applies to the next scan."),
        ];
        for (group, action, title, subtitle) in rows {
            let state = self.ui.window.lookup_action(action).and_then(|a| a.state()).and_then(|v| v.get::<bool>()).unwrap_or(false);
            let row = adw::SwitchRow::builder().title(title).subtitle(subtitle).active(state).build();
            let win = self.ui.window.clone();
            row.connect_active_notify(move |r| {
                if let Some(a) = win.lookup_action(action) {
                    a.change_state(&r.is_active().to_variant());
                }
            });
            group.add(&row);
        }
        page.add(&sizes);
        page.add(&scanning);
        dlg.add(&page);
        dlg.present(Some(&self.ui.window));
    }

    #[allow(deprecated)] // GtkShortcutsWindow: still the standard window on GTK 4.18
    pub fn show_shortcuts(&self) {
        let shortcuts = [
            ("General", vec![("<Control>o", "Scan a folder"), ("F5", "Rescan this folder"), ("Escape", "Stop the scan"), ("<Control>comma", "Preferences"), ("<Control>question", "Keyboard shortcuts")]),
            ("Navigation", vec![("<Alt>Up", "Parent folder"), ("Return", "Open the selected folder or file")]),
            ("Files", vec![("Delete", "Move selection to the Trash"), ("<Control>z", "Undo the last Move to Trash"), ("<Control>e", "Export the list to CSV")]),
        ];
        let mut xml = String::from(
            r#"<interface><object class="GtkShortcutsWindow" id="w"><property name="modal">1</property><child><object class="GtkShortcutsSection"><property name="section-name">main</property>"#,
        );
        for (title, items) in shortcuts {
            xml.push_str(&format!(r#"<child><object class="GtkShortcutsGroup"><property name="title">{title}</property>"#));
            for (accel, label) in items {
                xml.push_str(&format!(
                    r#"<child><object class="GtkShortcutsShortcut"><property name="accelerator">{}</property><property name="title">{label}</property></object></child>"#,
                    glib::markup_escape_text(accel)
                ));
            }
            xml.push_str("</object></child>");
        }
        xml.push_str("</object></child></object></interface>");
        let b = gtk::Builder::from_string(&xml);
        if let Some(w) = b.object::<gtk::ShortcutsWindow>("w") {
            w.set_transient_for(Some(&self.ui.window));
            w.present();
        }
    }

    /// Remember window size and view options (called when the window closes).
    pub fn save_settings(&self) {
        let mut s = self.settings.borrow_mut();
        let (w, h) = self.ui.window.default_size();
        if !self.ui.window.is_maximized() && w > 0 && h > 0 {
            s.set("width", w);
            s.set("height", h);
        }
        s.set("ondisk", self.ondisk.get());
        s.set("hidden", self.show_hidden.get());
        s.set("onefs", self.one_fs.get());
        s.save();
    }
}

#[cfg(test)]
mod tests {
    use super::csv_field;

    #[test]
    fn csv_quoting() {
        assert_eq!(csv_field("plain.txt"), "plain.txt");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("line\nbreak"), "\"line\nbreak\"");
    }
}
