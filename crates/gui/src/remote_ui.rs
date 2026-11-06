//! Scanning a NAS over SSH: the "Scan NAS" dialog, starting and rescanning remote scans,
//! and labels. A child module of `window`, so it can use `State`.
//!
//! Remote scans are read-only: actions that would touch the files (open, terminal, trash,
//! duplicate search) are switched off, because they'd run on this computer, not the NAS.

use std::path::Path;
use std::rc::Rc;

use adw::prelude::*;
use disklens_core::remote::{self, RemoteSpec};
use disklens_core::scan::ScanOptions;
use disklens_core::{Tree, NONE};
use gtk::gio;

use super::State;

const HELP: &str = "DiskLens runs the scan on the NAS over SSH, so it's fast even for millions of files. \
It uses your SSH keys; it can't ask for a password.\n\n\
One-time setup on a Synology:\n\
• Control Panel → Terminal & SNMP → Enable SSH service\n\
• Control Panel → User & Group → Advanced → Enable user home service\n\
• then in a terminal on this computer: ssh-copy-id USER@NAS\n\n\
Shares are under /volume1 (for example /volume1/video).";

pub fn add_actions(st: &Rc<State>) {
    let a = gio::SimpleAction::new("open-remote", None);
    let w = Rc::downgrade(st);
    a.connect_activate(move |_, _| {
        if let Some(st) = w.upgrade() {
            st.ask_remote();
        }
    });
    st.ui.window.add_action(&a);
}

impl State {
    pub fn is_remote(&self) -> bool {
        self.remote.borrow().is_some()
    }

    /// "user@nas:/volume1" for a NAS scan, the folder path otherwise.
    pub(super) fn root_label(&self) -> String {
        match self.remote.borrow().as_ref() {
            Some(spec) => spec.label(),
            None => self.tree.borrow().path(Tree::ROOT).display().to_string(),
        }
    }

    /// The last NAS scanned, for the start page's drive list.
    pub(super) fn last_remote(&self) -> Option<RemoteSpec> {
        let s = self.settings.borrow();
        Some(RemoteSpec { host: Some(s.str("remote_host")?), path: s.str("remote_path").unwrap_or_else(|| "/volume1".into()) })
    }

    fn ask_remote(self: &Rc<Self>) {
        let last = self.last_remote();
        let host = adw::EntryRow::builder().title("SSH login (user@address or ~/.ssh/config name)").build();
        host.set_text(last.as_ref().and_then(|l| l.host.as_deref()).unwrap_or(""));
        let path = adw::EntryRow::builder().title("Folder on the NAS").build();
        path.set_text(last.as_ref().map(|l| l.path.as_str()).unwrap_or("/volume1"));
        let group = adw::PreferencesGroup::new();
        group.add(&host);
        group.add(&path);

        let dlg = adw::AlertDialog::new(Some("Scan a NAS over SSH"), Some(HELP));
        dlg.set_extra_child(Some(&group));
        dlg.add_responses(&[("cancel", "_Cancel"), ("scan", "_Scan")]);
        dlg.set_response_appearance("scan", adw::ResponseAppearance::Suggested);
        dlg.set_default_response(Some("scan"));
        dlg.set_close_response("cancel");
        let enable = {
            let (dlg, host, path) = (dlg.clone(), host.clone(), path.clone());
            move || dlg.set_response_enabled("scan", !host.text().trim().is_empty() && path.text().trim().starts_with('/'))
        };
        enable();
        let e = enable.clone();
        host.connect_changed(move |_| e());
        path.connect_changed(move |_| enable());
        let w = Rc::downgrade(self);
        dlg.connect_response(None, move |_, resp| {
            let Some(st) = w.upgrade() else { return };
            if resp != "scan" {
                return;
            }
            let spec = RemoteSpec { host: Some(host.text().trim().to_string()), path: path.text().trim().to_string() };
            st.start_remote_scan(spec);
        });
        dlg.present(Some(&self.ui.window));
    }

    pub fn start_remote_scan(self: &Rc<Self>, spec: RemoteSpec) {
        if let Some(s) = self.scanner.borrow().as_ref() {
            s.cancel();
        }
        *self.remote.borrow_mut() = Some(spec.clone());
        self.set_type_filter(None);
        self.root_dev.set(None);
        *self.tree.borrow_mut() = Tree::new(Path::new(&spec.path));
        self.side_stores.borrow_mut().clear();
        *self.contents.borrow_mut() = None;
        self.errors.borrow_mut().clear();
        *self.fatal.borrow_mut() = None;
        self.current.set(NONE);
        self.dups_reset();
        self.ui.stack.set_visible_child_name("main");
        self.reset_side_root();
        self.show_folder(Tree::ROOT, false);
        let opts = ScanOptions { one_fs: self.one_fs.get(), ..Default::default() };
        match remote::start(Tree::ROOT, 1, spec, opts) {
            Ok(s) => self.run_scanner(s),
            Err(e) => self.toast(&format!("Can't start ssh: {e}")),
        }
    }

    /// Rescan one folder of a NAS scan. Returns false for local scans.
    pub(super) fn rescan_remote(self: &Rc<Self>, dir: u32, next_id: u32) -> bool {
        let Some(spec) = self.remote.borrow().clone() else { return false };
        let path = self.tree.borrow().path(dir).display().to_string();
        let opts = ScanOptions { one_fs: self.one_fs.get(), ..Default::default() };
        match remote::start(dir, next_id, RemoteSpec { host: spec.host, path }, opts) {
            Ok(s) => self.run_scanner(s),
            Err(e) => self.toast(&format!("Can't rescan: {e}")),
        }
        true
    }

    /// After a NAS scan that worked: offer it on the start page next time.
    pub(super) fn remember_remote(&self) {
        if self.fatal.borrow().is_some() {
            return;
        }
        let Some(spec) = self.remote.borrow().clone() else { return };
        let mut s = self.settings.borrow_mut();
        if let Some(h) = &spec.host {
            s.set("remote_host", h.as_str());
        }
        s.set("remote_path", spec.path.as_str());
    }

    /// After a scan ends: explain a failed NAS connection.
    pub(super) fn show_fatal(&self) {
        let Some(msg) = self.fatal.borrow_mut().take() else { return };
        // nothing was scanned: go back to the start page rather than show an empty folder
        let empty = { let t = self.tree.borrow(); let r = t.get(Tree::ROOT); r.nfiles == 0 && r.ndirs == 0 };
        if empty {
            self.ui.stack.set_visible_child_name("start");
            self.ui.window.set_title(Some("DiskLens"));
        }
        let dlg = adw::AlertDialog::new(Some("The scan didn't work"), Some(&msg));
        dlg.add_response("ok", "_OK");
        dlg.set_body_use_markup(false);
        dlg.present(Some(&self.ui.window));
    }
}
