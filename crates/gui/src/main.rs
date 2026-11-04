//! DiskLens: see what's using your disk space.
//!
//! Usage: disklens [PATH]
//!        disklens --shots DIR PATH   (scripted walk-through that saves PNG screenshots)

mod mounts;
mod pages;
mod treemap;
mod row;
mod settings;
mod window;

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::gdk::prelude::PaintableExt;
use gtk::{gio, glib};

const APP_ID: &str = "com.familjenhero.DiskLens";

fn main() -> glib::ExitCode {
    let mut args = std::env::args().skip(1);
    let mut shots: Option<PathBuf> = None;
    let mut path: Option<PathBuf> = None;
    while let Some(a) = args.next() {
        if a == "--shots" {
            shots = args.next().map(PathBuf::from);
        } else {
            path = Some(PathBuf::from(a));
        }
    }

    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_startup(|_| {
        let css = gtk::CssProvider::new();
        css.load_from_string(include_str!("style.css"));
        gtk::style_context_add_provider_for_display(
            &gtk::gdk::Display::default().expect("display"),
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
    app.connect_activate(move |app| {
        let st = window::build(app);
        // the window's close handler owns the state for the window's lifetime
        let keep = st.clone();
        st.ui.window.connect_close_request(move |_| {
            keep.cancel_scan();
            keep.save_settings();
            glib::Propagation::Proceed
        });
        st.ui.window.present();
        match (&shots, &path) {
            (Some(dir), Some(p)) => selftest(st, dir.clone(), p.clone()),
            (None, Some(p)) => st.start_scan(p.clone()),
            _ => {}
        }
    });
    app.run_with_args::<&str>(&[])
}

fn shot(st: &window::State, dir: &Path, name: &str) {
    let win = &st.ui.window;
    let paintable = gtk::WidgetPaintable::new(Some(win));
    let snap = gtk::Snapshot::new();
    paintable.snapshot(&snap, win.width() as f64, win.height() as f64);
    if let (Some(node), Some(r)) = (snap.to_node(), win.renderer()) {
        let p = dir.join(format!("{name}.png"));
        let _ = r.render_texture(&node, None).save_to_png(&p);
        println!("shot {}", p.display());
    }
}

type Step = Box<dyn Fn(&Rc<window::State>) -> bool>;

/// Scan, open the biggest folder, switch to dark and narrow layouts; screenshot each.
fn selftest(st: Rc<window::State>, dir: PathBuf, path: PathBuf) {
    let _ = std::fs::create_dir_all(&dir);
    let d = dir.clone();
    let steps: Vec<(u64, Step)> = vec![
        (2200, Box::new(move |s| { shot(s, &d, "r0_start"); true })),
        (100, Box::new(move |s| { s.start_scan(path.clone()); true })),
        (300, Box::new(|s| !s.is_scanning())),
        (700, { let d = dir.clone(); Box::new(move |s| { shot(s, &d, "r1_main"); true }) }),
        (100, Box::new(|s| { s.menu_on_first_row(); true })),
        (600, { let d = dir.clone(); Box::new(move |s| { shot(s, &d, "r3a_menu"); true }) }),
        (100, Box::new(|s| { gtk::prelude::WidgetExt::activate_action(&s.ui.window, "win.ctx-trash", None).ok(); true })),
        (700, { let d = dir.clone(); Box::new(move |s| { shot(s, &d, "r3b_confirm"); true }) }),
        (100, Box::new(|s| {
            // close the confirmation dialog, then trash a known folder directly
            if let Some(d) = s.ui.window.visible_dialog() { d.force_close(); }
            s.trash_child_now("Documents");
            true
        })),
        (900, { let d = dir.clone(); Box::new(move |s| { shot(s, &d, "r3c_trashed"); true }) }),
        (100, Box::new(|s| { gtk::prelude::WidgetExt::activate_action(&s.ui.window, "win.undo", None).ok(); true })),
        (300, Box::new(|s| !s.is_scanning())),
        (900, { let d = dir.clone(); Box::new(move |s| { shot(s, &d, "r3d_undone"); true }) }),
        (100, { let d = dir.clone(); Box::new(move |s| {
            match s.export_contents_to(&d.join("contents.csv")) {
                Ok(n) => println!("csv {n} rows"),
                Err(e) => println!("csv failed: {e}"),
            }
            s.show_preferences();
            true
        }) }),
        (700, { let d = dir.clone(); Box::new(move |s| { shot(s, &d, "r3e_prefs"); true }) }),
        (100, Box::new(|s| {
            if let Some(d) = s.ui.window.visible_dialog() { d.force_close(); }
            s.show_shortcuts();
            s.save_settings();
            true
        })),
        (1500, Box::new(|s| { for w in gtk::Window::list_toplevels() { if let Ok(w) = w.downcast::<gtk::Window>() { if w != *s.ui.window.upcast_ref::<gtk::Window>() { w.close(); } } } true })),
        (100, Box::new(|s| { s.set_page("biggest"); true })),
        (700, { let d = dir.clone(); Box::new(move |s| { shot(s, &d, "r2b_biggest"); true }) }),
        (100, Box::new(|s| { s.set_page("types"); true })),
        (700, { let d = dir.clone(); Box::new(move |s| { shot(s, &d, "r2c_types"); true }) }),
        (100, Box::new(|s| { s.set_page("ages"); true })),
        (700, { let d = dir.clone(); Box::new(move |s| { shot(s, &d, "r2d_ages"); true }) }),
        (100, Box::new(|s| { s.set_page("map"); true })),
        (700, { let d = dir.clone(); Box::new(move |s| { shot(s, &d, "r2e_map"); true }) }),
        (100, Box::new(|s| { s.set_page("contents"); true })),
        (100, Box::new(|s| {
            if let Some(c) = s.biggest_child_dir() {
                s.show_folder(c, false);
            }
            true
        })),
        (700, { let d = dir.clone(); Box::new(move |s| { shot(s, &d, "r2_subfolder"); true }) }),
        (100, Box::new(|_| {
            adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
            true
        })),
        (800, { let d = dir.clone(); Box::new(move |s| { shot(s, &d, "r3_dark"); true }) }),
        (100, Box::new(|s| { s.ui.window.set_default_size(720, 820); true })),
        (900, { let d = dir.clone(); Box::new(move |s| { shot(s, &d, "r4_narrow"); true }) }),
        (100, Box::new(|s| { s.set_page("map"); true })),
        (500, { let d = dir.clone(); Box::new(move |s| { shot(s, &d, "r5_narrow_tab"); true }) }),
        (100, Box::new(|s| { s.ui.window.application().unwrap().quit(); true })),
    ];
    run_steps(st, Rc::new(steps), 0);
}

fn run_steps(st: Rc<window::State>, steps: Rc<Vec<(u64, Step)>>, i: usize) {
    if i >= steps.len() {
        return;
    }
    let delay = steps[i].0;
    glib::timeout_add_local_once(Duration::from_millis(delay), move || {
        if (steps[i].1)(&st) {
            run_steps(st, steps, i + 1);
        } else {
            run_steps(st, steps, i); // condition not met yet: poll again
        }
    });
}
