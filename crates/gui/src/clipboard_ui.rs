//! Cut, Copy and Paste of files and folders, on the system clipboard in the same format as
//! GNOME Files (`x-special/gnome-copied-files`), so DiskLens and Files can paste each other's
//! items. Pasting never overwrites (see `disklens_core::fileops`). A child module of `window`.

use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use disklens_core::fileops;
use disklens_core::util::fmt_int;
use disklens_core::{Tree, NONE};
use gtk::{gdk, gio, glib};

use super::actions::{common_ancestor, Target};
use super::State;

const GNOME_FILES: &str = "x-special/gnome-copied-files";
const URI_LIST: &str = "text/uri-list";

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
    // from the right-click menu: act on what was right-clicked (and the rest of the selection)
    add("ctx-cut", &[], |st| {
        let t = st.ctx_targets.borrow().clone();
        st.put_on_clipboard(&t, true);
    });
    add("ctx-copy", &[], |st| {
        let t = st.ctx_targets.borrow().clone();
        st.put_on_clipboard(&t, false);
    });
    add("ctx-paste", &[], |st| {
        let dest = st.ctx_targets.borrow().first().map(|t| t.folder());
        if let Some(d) = dest {
            st.paste_into(d);
        }
    });
    // keyboard: the selection on the Folder/Biggest tab, and the folder being shown
    add("cut", &["<Control>x"], |st| {
        let t = st.selected_targets();
        st.put_on_clipboard(&t, true);
    });
    add("copy", &["<Control>c"], |st| {
        let t = st.selected_targets();
        st.put_on_clipboard(&t, false);
    });
    add("paste", &["<Control>v"], |st| st.paste_into(st.current.get()));
}

/// The clipboard holds files or folders (from DiskLens, GNOME Files or another file manager).
pub(super) fn clipboard_has_files(win: &impl IsA<gtk::Widget>) -> bool {
    let f = win.clipboard().formats();
    f.contain_mime_type(GNOME_FILES) || f.contain_mime_type(URI_LIST)
}

/// "cut"/"copy" and the local paths, from either clipboard format.
fn parse_clipboard(text: &str, mime: &str) -> (bool, Vec<PathBuf>) {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let cut = if mime == GNOME_FILES { lines.next() == Some("cut") } else { false };
    let paths = lines.filter(|l| !l.starts_with('#')).filter_map(|u| gio::File::for_uri(u).path()).collect();
    (cut, paths)
}

impl State {
    fn put_on_clipboard(&self, targets: &[Target], cut: bool) {
        if targets.is_empty() {
            return;
        }
        if self.is_remote() {
            self.toast("NAS scans are read-only: copy and move files on the NAS itself");
            return;
        }
        if cut && targets.contains(&Target::Dir(Tree::ROOT)) {
            self.toast("The scanned folder itself can't be cut");
            return;
        }
        let paths: Vec<PathBuf> = {
            let tree = self.tree.borrow();
            targets.iter().map(|t| t.path(&tree)).collect()
        };
        let uris: Vec<String> = paths.iter().map(|p| gio::File::for_path(p).uri().to_string()).collect();
        let gnome = format!("{}\n{}", if cut { "cut" } else { "copy" }, uris.join("\n"));
        let uri_list: String = uris.iter().map(|u| format!("{u}\r\n")).collect();
        let text = paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n");
        let bytes = |s: &str| glib::Bytes::from(s.as_bytes());
        let provider = gdk::ContentProvider::new_union(&[
            gdk::ContentProvider::for_bytes(GNOME_FILES, &bytes(&gnome)),
            gdk::ContentProvider::for_bytes(URI_LIST, &bytes(&uri_list)),
            gdk::ContentProvider::for_bytes("text/plain;charset=utf-8", &bytes(&text)),
        ]);
        if self.ui.window.clipboard().set_content(Some(&provider)).is_err() {
            self.toast("Couldn't use the clipboard");
            return;
        }
        let what = if paths.len() == 1 {
            format!("“{}”", paths[0].file_name().unwrap_or_default().to_string_lossy())
        } else {
            format!("{} items", fmt_int(paths.len() as u64))
        };
        self.toast(&if cut { format!("Cut {what}: paste into a folder to move") } else { format!("Copied {what}") });
    }

    /// Paste the clipboard's files into folder `dest` of the scan.
    fn paste_into(self: &Rc<Self>, dest: u32) {
        if dest == NONE {
            return;
        }
        if self.is_remote() {
            self.toast("NAS scans are read-only: copy and move files on the NAS itself");
            return;
        }
        if self.is_scanning() {
            self.toast("Wait for the scan to finish before pasting");
            return;
        }
        if !clipboard_has_files(&self.ui.window) {
            self.toast("Nothing to paste: cut or copy some files first");
            return;
        }
        let dest_path = self.tree.borrow().path(dest);
        let cb = self.ui.window.clipboard();
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok((stream, mime)) = cb.read_future(&[GNOME_FILES, URI_LIST], glib::Priority::DEFAULT).await else { return };
            let mut data = Vec::new();
            while let Ok(chunk) = stream.read_bytes_future(64 * 1024, glib::Priority::DEFAULT).await {
                if chunk.is_empty() {
                    break;
                }
                data.extend_from_slice(&chunk);
            }
            let (cut, sources) = parse_clipboard(&String::from_utf8_lossy(&data), mime.as_str());
            let Some(st) = w.upgrade() else { return };
            if sources.is_empty() {
                st.toast("Nothing to paste");
                return;
            }
            st.run_paste(sources, dest_path, cut).await;
        });
    }

    async fn run_paste(self: &Rc<Self>, sources: Vec<PathBuf>, dest: PathBuf, cut: bool) {
        self.ui.progress.set_visible(true);
        self.ui.progress.pulse();
        let (srcs, d) = (sources.clone(), dest.clone());
        let results: Vec<Result<fileops::Pasted, String>> = gio::spawn_blocking(move || {
            srcs.iter()
                .map(|s| {
                    let r = if cut { fileops::move_into(s, &d) } else { fileops::copy_into(s, &d) };
                    r.map_err(|e| format!("{}: {e}", s.display()))
                })
                .collect()
        })
        .await
        .unwrap_or_default();
        self.ui.progress.set_visible(false);

        let done: Vec<&fileops::Pasted> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
        let errors: Vec<&String> = results.iter().filter_map(|r| r.as_ref().err()).collect();
        let renamed = done.iter().filter(|p| p.renamed).count();
        if cut && errors.is_empty() {
            // the originals are gone, so the clipboard no longer points at anything
            self.ui.window.clipboard().set_content(None::<&gdk::ContentProvider>).ok();
        }

        // refresh the smallest part of the scan holding the destination and every source folder
        let folders: Vec<u32> = {
            let tree = self.tree.borrow();
            let mut v: Vec<u32> = tree.dir_for_path(&dest).into_iter().collect();
            if cut {
                v.extend(sources.iter().filter_map(|s| s.parent()).filter_map(|p| tree.dir_for_path(p)));
            }
            v
        };
        let common = { common_ancestor(&self.tree.borrow(), &folders) };
        if common != NONE && !done.is_empty() {
            self.rescan(common);
        }

        let n = done.len();
        let mut msg = match (n, cut) {
            (0, _) => String::new(),
            (1, true) => format!("Moved “{}”", done[0].to.file_name().unwrap_or_default().to_string_lossy()),
            (1, false) => format!("Copied “{}”", done[0].to.file_name().unwrap_or_default().to_string_lossy()),
            (n, true) => format!("Moved {} items", fmt_int(n as u64)),
            (n, false) => format!("Copied {} items", fmt_int(n as u64)),
        };
        if renamed > 0 {
            msg.push_str(&format!(" · {renamed} renamed so nothing was overwritten"));
        }
        if !msg.is_empty() {
            self.toast(&msg);
        }
        if !errors.is_empty() {
            let mut body = errors.iter().take(12).map(|e| e.as_str()).collect::<Vec<_>>().join("\n");
            if errors.len() > 12 {
                body.push_str(&format!("\n… and {} more", errors.len() - 12));
            }
            let dlg = adw::AlertDialog::new(Some(if cut { "Some items couldn't be moved" } else { "Some items couldn't be copied" }), Some(&body));
            dlg.add_response("ok", "_OK");
            dlg.present(Some(&self.ui.window));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_formats() {
        let (cut, p) = parse_clipboard("cut\nfile:///home/u/a%20b.txt\nfile:///home/u/dir", GNOME_FILES);
        assert!(cut);
        assert_eq!(p, vec![PathBuf::from("/home/u/a b.txt"), PathBuf::from("/home/u/dir")]);
        let (cut, p) = parse_clipboard("copy\nfile:///x\n", GNOME_FILES);
        assert!(!cut && p == vec![PathBuf::from("/x")]);
        let (cut, p) = parse_clipboard("# comment\r\nfile:///y\r\n", URI_LIST);
        assert!(!cut && p == vec![PathBuf::from("/y")]);
    }
}
