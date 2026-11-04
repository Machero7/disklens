//! The main window: start page, sidebar folder tree, folder header with usage strip,
//! Contents list, and the scan loop that keeps them live.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use disklens_core::scan::{Msg, ScanOptions, Scanner, TryRecvError};
use disklens_core::util::{ext_of, fmt_int, human, NO_EXT, RAR_PARTS};
use disklens_core::{Tree, NONE};
use gtk::{gio, glib};

#[path = "actions.rs"]
mod actions;

use crate::mounts;
use crate::pages::Pages;
use crate::treemap;
use crate::row::RowObject;

/// Same colours as `.accent-N` in style.css, for the cairo-drawn usage strip.
const ACCENTS: [(f64, f64, f64); 4] = [
    (0x35 as f64 / 255.0, 0x84 as f64 / 255.0, 0xe4 as f64 / 255.0),
    (0x2e as f64 / 255.0, 0xc2 as f64 / 255.0, 0x7e as f64 / 255.0),
    (0xff as f64 / 255.0, 0x78 as f64 / 255.0, 0x00 as f64 / 255.0),
    (0x91 as f64 / 255.0, 0x41 as f64 / 255.0, 0xac as f64 / 255.0),
];
const TOP_COLOURED: usize = 4;

pub fn now() -> i64 {
    glib::real_time() / 1_000_000
}

pub struct Ui {
    pub window: adw::ApplicationWindow,
    stack: gtk::Stack,
    drive_list: gtk::ListBox,
    drive_pop_list: gtk::ListBox,
    split: adw::OverlaySplitView,
    side_title: adw::WindowTitle,
    side_root: gio::ListStore,
    side_view: gtk::ListView,
    side_sorter: gtk::CustomSorter,
    view_stack: adw::ViewStack,
    folder_title: gtk::Label,
    folder_sub: gtk::Label,
    strip: gtk::DrawingArea,
    legend: gtk::Box,
    contents_view: gtk::ColumnView,
    contents_sort: gtk::SortListModel,
    contents_scroll: gtk::ScrolledWindow,
    progress: gtk::ProgressBar,
    stop_btn: gtk::Button,
    rescan_btn: gtk::Button,
    up_btn: gtk::Button,
    pages: Pages,
    map: gtk::DrawingArea,
}

struct Contents {
    dir: u32,
    store: gio::ListStore,
    synced: usize,
    files_added: bool,
}

pub struct State {
    pub ui: Ui,
    tree: RefCell<Tree>,
    scanner: RefCell<Option<Scanner>>,
    root_dev: Cell<Option<u64>>,
    current: Cell<u32>,
    side_model: OnceCell<gtk::TreeListModel>,
    side_sort: OnceCell<gtk::SortListModel>,
    side_sel: OnceCell<gtk::SingleSelection>,
    side_stores: RefCell<HashMap<u32, (gio::ListStore, usize)>>,
    contents: RefCell<Option<Contents>>,
    strip_data: RefCell<Vec<(u64, Option<usize>, String)>>,
    legend_key: RefCell<String>,
    kinds: RefCell<HashMap<String, (String, gio::Icon)>>,
    errors: RefCell<Vec<(PathBuf, String)>>,
    ondisk: Cell<bool>,
    show_hidden: Cell<bool>,
    one_fs: Cell<bool>,
    syncing: Cell<bool>,
    ticking: Cell<bool>,
    tick_n: Cell<u64>,
    last_scan: RefCell<String>,
    /// The stats pages need recomputing; time (µs) they were last filled.
    stats_dirty: Cell<bool>,
    stats_at: Cell<i64>,
    /// Treemap: tiles from the last paint, hovered tile, and a cached render keyed by
    /// (folder, generation, width, height, scale, dark).
    map_hits: RefCell<Vec<treemap::Hit>>,
    map_hover: Cell<Option<usize>>,
    map_gen: Cell<u64>,
    map_cache: RefCell<Option<((u32, u64, i32, i32, i32, bool), gtk::cairo::ImageSurface)>>,
    /// What the open context menu acts on.
    ctx_targets: RefCell<Vec<actions::Target>>,
    /// Last trashed paths and their folders, for Undo / Ctrl+Z.
    last_trash: RefCell<Option<(Vec<PathBuf>, Vec<u32>)>>,
    trash_toast: RefCell<Option<adw::Toast>>,
}

fn visible(state: &State, name: &[u8]) -> bool {
    state.show_hidden.get() || name.first() != Some(&b'.')
}

// ------------------------------------------------------------------ construction

pub fn build(app: &adw::Application) -> Rc<State> {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("DiskLens")
        .default_width(1320)
        .default_height(820)
        .width_request(360)
        .height_request(400)
        .build();

    // ---- start page
    let drive_list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).build();
    drive_list.add_css_class("boxed-list");
    let choose = gtk::Button::builder()
        .label("Choose folder…")
        .halign(gtk::Align::Center)
        .action_name("win.open")
        .build();
    choose.add_css_class("pill");
    choose.add_css_class("suggested-action");
    let start_box = gtk::Box::new(gtk::Orientation::Vertical, 24);
    start_box.append(&drive_list);
    start_box.append(&choose);
    let clamp = adw::Clamp::builder().maximum_size(560).child(&start_box).build();
    let status = adw::StatusPage::builder()
        .icon_name("drive-harddisk-symbolic")
        .title("Find what's using your disk")
        .description("Pick a drive or a folder to scan.")
        .child(&clamp)
        .build();
    let start_tv = adw::ToolbarView::new();
    let start_hb = adw::HeaderBar::new();
    start_hb.pack_end(&menu_button());
    start_tv.add_top_bar(&start_hb);
    start_tv.set_content(Some(&gtk::ScrolledWindow::builder().child(&status).vexpand(true).build()));

    // ---- sidebar
    let side_title = adw::WindowTitle::new("DiskLens", "");
    let side_hb = adw::HeaderBar::new();
    side_hb.set_title_widget(Some(&side_title));
    let drive_pop_list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).build();
    drive_pop_list.add_css_class("boxed-list");
    let pop_box = gtk::Box::new(gtk::Orientation::Vertical, 10);
    pop_box.set_margin_top(6);
    pop_box.set_margin_bottom(6);
    pop_box.set_margin_start(6);
    pop_box.set_margin_end(6);
    pop_box.set_width_request(380);
    pop_box.append(&drive_pop_list);
    let pop_choose = gtk::Button::builder().label("Choose folder…").action_name("win.open").build();
    pop_box.append(&pop_choose);
    let drive_pop = gtk::Popover::builder().child(&pop_box).build();
    let open_btn = gtk::MenuButton::builder()
        .icon_name("drive-harddisk-symbolic")
        .tooltip_text("Scan a drive or folder")
        .popover(&drive_pop)
        .build();
    side_hb.pack_start(&open_btn);

    let side_root = gio::ListStore::new::<RowObject>();
    let side_sorter = gtk::CustomSorter::new(|a, b| {
        let (a, b) = (a.downcast_ref::<RowObject>().unwrap(), b.downcast_ref::<RowObject>().unwrap());
        b.size().cmp(&a.size()).then_with(|| a.sort_name().cmp(&b.sort_name())).into()
    });
    let side_view = gtk::ListView::builder().factory(&side_factory()).build();
    side_view.add_css_class("navigation-sidebar");
    let side_tv = adw::ToolbarView::new();
    side_tv.add_top_bar(&side_hb);
    side_tv.set_content(Some(&gtk::ScrolledWindow::builder().child(&side_view).vexpand(true).build()));

    // ---- content
    let view_stack = adw::ViewStack::new();
    let switcher = adw::ViewSwitcher::builder().stack(&view_stack).policy(adw::ViewSwitcherPolicy::Wide).build();
    let content_hb = adw::HeaderBar::new();
    content_hb.set_title_widget(Some(&switcher));
    let toggle = gtk::ToggleButton::builder().icon_name("sidebar-show-symbolic").tooltip_text("Show folders").build();
    let up_btn = gtk::Button::builder()
        .icon_name("go-up-symbolic")
        .tooltip_text("Parent folder (Alt+Up)")
        .action_name("win.up")
        .build();
    content_hb.pack_start(&toggle);
    content_hb.pack_start(&up_btn);
    content_hb.pack_end(&menu_button());
    let stop_btn = gtk::Button::builder()
        .icon_name("process-stop-symbolic")
        .tooltip_text("Stop scan (Esc)")
        .action_name("win.stop")
        .visible(false)
        .build();
    let rescan_btn = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Rescan this folder (F5)")
        .action_name("win.rescan")
        .build();
    content_hb.pack_end(&stop_btn);
    content_hb.pack_end(&rescan_btn);
    let progress = gtk::ProgressBar::builder().visible(false).pulse_step(0.08).build();
    progress.add_css_class("osd");

    // folder header + usage strip + legend
    let folder_title = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Middle).build();
    folder_title.add_css_class("folder-title");
    let folder_sub = gtk::Label::builder().xalign(0.0).build();
    folder_sub.add_css_class("dim-label");
    folder_sub.add_css_class("numeric");
    let title_row = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    title_row.append(&folder_title);
    title_row.append(&folder_sub);
    folder_sub.set_valign(gtk::Align::Baseline);
    folder_title.set_valign(gtk::Align::Baseline);
    let strip = gtk::DrawingArea::builder().content_height(14).hexpand(true).build();
    let legend = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    let header = gtk::Box::new(gtk::Orientation::Vertical, 10);
    header.set_margin_top(18);
    header.set_margin_start(18);
    header.set_margin_end(18);
    header.set_margin_bottom(6);
    header.append(&title_row);
    header.append(&strip);
    header.append(&legend);

    let contents_view = gtk::ColumnView::builder().reorderable(false).build();
    let contents_sort = gtk::SortListModel::new(None::<gio::ListModel>, contents_view.sorter());
    contents_view.set_model(Some(&gtk::MultiSelection::new(Some(contents_sort.clone()))));
    add_contents_columns(&contents_view);
    let contents_scroll = gtk::ScrolledWindow::builder().child(&contents_view).vexpand(true).build();
    contents_scroll.add_css_class("card");
    contents_scroll.add_css_class("contents-card");
    contents_scroll.set_overflow(gtk::Overflow::Hidden);
    contents_scroll.set_margin_start(12);
    contents_scroll.set_margin_end(12);
    contents_scroll.set_margin_bottom(12);
    let contents_page = gtk::Box::new(gtk::Orientation::Vertical, 6);
    contents_page.append(&header);
    contents_page.append(&contents_scroll);

    view_stack.add_titled_with_icon(&contents_page, Some("contents"), "Folder", "folder-symbolic");
    let pages = Pages::new();
    view_stack.add_titled_with_icon(&pages.biggest.widget, Some("biggest"), "Biggest", "view-sort-descending-symbolic");
    view_stack.add_titled_with_icon(&pages.types.widget, Some("types"), "Types", "text-x-generic-symbolic");
    view_stack.add_titled_with_icon(&pages.ages.widget, Some("ages"), "Ages", "document-open-recent-symbolic");
    let map = gtk::DrawingArea::builder().hexpand(true).vexpand(true).has_tooltip(true).build();
    let map_hint = gtk::Label::builder()
        .label("Each tile's area is its size. Click a folder to open it.")
        .xalign(0.0)
        .wrap(true)
        .build();
    map_hint.add_css_class("dim-label");
    map_hint.set_margin_top(14);
    map_hint.set_margin_start(18);
    map_hint.set_margin_end(18);
    map.set_margin_start(12);
    map.set_margin_end(12);
    map.set_margin_bottom(12);
    let map_page = gtk::Box::new(gtk::Orientation::Vertical, 10);
    map_page.append(&map_hint);
    map_page.append(&map);
    view_stack.add_titled_with_icon(&map_page, Some("map"), "Map", "view-grid-symbolic");
    for (name, title, icon) in [
        ("dupes", "Copies", "edit-copy-symbolic"),
    ] {
        let placeholder = adw::StatusPage::builder()
            .icon_name(icon)
            .title(title)
            .description("Coming in the next build step.")
            .build();
        view_stack.add_titled_with_icon(&placeholder, Some(name), title, icon);
    }

    let switcher_bar = adw::ViewSwitcherBar::builder().stack(&view_stack).build();
    let content_tv = adw::ToolbarView::new();
    content_tv.add_top_bar(&content_hb);
    content_tv.add_top_bar(&progress);
    content_tv.add_bottom_bar(&switcher_bar);
    content_tv.set_content(Some(&view_stack));

    let split = adw::OverlaySplitView::builder()
        .sidebar(&side_tv)
        .content(&content_tv)
        .min_sidebar_width(260.0)
        .max_sidebar_width(340.0)
        .sidebar_width_fraction(0.23)
        .build();
    split.bind_property("show-sidebar", &toggle, "active").bidirectional().sync_create().build();

    let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).build();
    stack.add_named(&start_tv, Some("start"));
    stack.add_named(&split, Some("main"));
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&stack));
    window.set_content(Some(&toasts));

    // narrow windows: overlay sidebar, switcher at the bottom
    let bp = adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 780sp").unwrap());
    bp.add_setter(&split, "collapsed", Some(&true.to_value()));
    bp.add_setter(&switcher_bar, "reveal", Some(&true.to_value()));
    bp.add_setter(&content_hb, "title-widget", Some(&None::<gtk::Widget>.to_value()));
    let columns = contents_view.columns();
    for i in 0..columns.n_items() {
        let col = columns.item(i).and_downcast::<gtk::ColumnViewColumn>().unwrap();
        if matches!(col.title().as_deref(), Some("Files") | Some("Modified") | Some("Type")) {
            bp.add_setter(&col, "visible", Some(&false.to_value()));
        }
    }
    for page in [&pages.biggest, &pages.types, &pages.ages] {
        for col in &page.narrow_hide {
            bp.add_setter(col, "visible", Some(&false.to_value()));
        }
    }
    window.add_breakpoint(bp);

    let ui = Ui {
        window,
        stack,
        drive_list,
        drive_pop_list,
        split,
        side_title,
        side_root,
        side_view,
        side_sorter,
        view_stack,
        folder_title,
        folder_sub,
        strip,
        legend,
        contents_view,
        contents_sort,
        contents_scroll,
        progress,
        stop_btn,
        rescan_btn,
        up_btn,
        pages,
        map,
    };
    let state = Rc::new(State {
        ui,
        tree: RefCell::new(Tree::new(Path::new("/"))),
        scanner: RefCell::new(None),
        root_dev: Cell::new(None),
        current: Cell::new(NONE),
        side_model: OnceCell::new(),
        side_sort: OnceCell::new(),
        side_sel: OnceCell::new(),
        side_stores: RefCell::new(HashMap::new()),
        contents: RefCell::new(None),
        strip_data: RefCell::new(Vec::new()),
        legend_key: RefCell::new(String::new()),
        kinds: RefCell::new(HashMap::new()),
        errors: RefCell::new(Vec::new()),
        ondisk: Cell::new(false),
        show_hidden: Cell::new(true),
        one_fs: Cell::new(true),
        syncing: Cell::new(false),
        ticking: Cell::new(false),
        tick_n: Cell::new(0),
        last_scan: RefCell::new(String::new()),
        stats_dirty: Cell::new(true),
        stats_at: Cell::new(0),
        map_hits: RefCell::new(Vec::new()),
        map_hover: Cell::new(None),
        map_gen: Cell::new(0),
        map_cache: RefCell::new(None),
        ctx_targets: RefCell::new(Vec::new()),
        last_trash: RefCell::new(None),
        trash_toast: RefCell::new(None),
    });
    wire(&state, &drive_pop);
    actions::set_current(&state);
    actions::add_actions(&state);
    actions::attach_map_ctx(&state);
    state
}

fn menu_button() -> gtk::MenuButton {
    let menu = gio::Menu::new();
    let view = gio::Menu::new();
    view.append(Some("Show size on disk"), Some("win.ondisk"));
    view.append(Some("Show hidden files"), Some("win.hidden"));
    view.append(Some("Stay on one filesystem"), Some("win.onefs"));
    menu.append_section(None, &view);
    let other = gio::Menu::new();
    other.append(Some("Scan folder…"), Some("win.open"));
    other.append(Some("About DiskLens"), Some("win.about"));
    menu.append_section(None, &other);
    gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).primary(true).build()
}

/// Sidebar row: expander › [folder icon] name ........ size
fn side_factory() -> gtk::SignalListItemFactory {
    let f = gtk::SignalListItemFactory::new();
    f.connect_setup(|_, obj| {
        let li = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let icon = gtk::Image::from_icon_name(crate::row::FOLDER_ICON);
        let name = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::Middle).build();
        let size = gtk::Label::new(None);
        size.add_css_class("dim-label");
        size.add_css_class("numeric");
        size.add_css_class("side-size");
        let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        b.append(&icon);
        b.append(&name);
        b.append(&size);
        let exp = gtk::TreeExpander::builder().child(&b).indent_for_icon(true).build();
        li.set_child(Some(&exp));
        actions::attach_ctx(&exp, li);
        let item = li.property_expression("item");
        item.bind(&exp, "list-row", gtk::Widget::NONE);
        let row = item.chain_property::<gtk::TreeListRow>("item");
        row.chain_property::<RowObject>("name").bind(&name, "label", gtk::Widget::NONE);
        row.chain_property::<RowObject>("size-text").bind(&size, "label", gtk::Widget::NONE);
        row.chain_property::<RowObject>("gicon").bind(&icon, "gicon", gtk::Widget::NONE);
    });
    f
}

pub(crate) fn sorter(cmp: impl Fn(&RowObject, &RowObject) -> std::cmp::Ordering + 'static) -> gtk::CustomSorter {
    gtk::CustomSorter::new(move |a, b| {
        cmp(a.downcast_ref::<RowObject>().unwrap(), b.downcast_ref::<RowObject>().unwrap()).into()
    })
}

pub(crate) fn label_cell(prop: &'static str, xalign: f32, dim: bool) -> gtk::SignalListItemFactory {
    let f = gtk::SignalListItemFactory::new();
    f.connect_setup(move |_, obj| {
        let li = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let l = gtk::Label::builder().xalign(xalign).ellipsize(gtk::pango::EllipsizeMode::End).build();
        l.add_css_class("numeric");
        if dim {
            l.add_css_class("dim-label");
        }
        li.set_child(Some(&l));
        actions::attach_ctx(&l, li);
        li.property_expression("item").chain_property::<RowObject>(prop).bind(&l, "label", gtk::Widget::NONE);
    });
    f
}

/// Name cell: coloured icon + name.
pub(crate) fn name_cell() -> gtk::SignalListItemFactory {
    let name_f = gtk::SignalListItemFactory::new();
    name_f.connect_setup(|_, obj| {
        let li = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let icon = gtk::Image::new();
        let wrap = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        wrap.append(&icon);
        let name = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Middle).build();
        let b = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        b.append(&wrap);
        b.append(&name);
        li.set_child(Some(&b));
        actions::attach_ctx(&b, li);
        let item = li.property_expression("item");
        item.chain_property::<RowObject>("gicon").bind(&icon, "gicon", gtk::Widget::NONE);
        item.chain_property::<RowObject>("accent").bind(&wrap, "css-classes", gtk::Widget::NONE);
        item.chain_property::<RowObject>("name").bind(&name, "label", gtk::Widget::NONE);
    });
    name_f
}

/// Share cell: coloured rounded bar + percentage.
pub(crate) fn share_cell() -> gtk::SignalListItemFactory {
    let share_f = gtk::SignalListItemFactory::new();
    share_f.connect_setup(|_, obj| {
        let li = obj.downcast_ref::<gtk::ListItem>().unwrap();
        let bar = gtk::ProgressBar::builder().hexpand(true).valign(gtk::Align::Center).build();
        let wrap = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        wrap.set_hexpand(true);
        wrap.append(&bar);
        let pct = gtk::Label::builder().xalign(1.0).width_chars(6).build();
        pct.add_css_class("dim-label");
        pct.add_css_class("numeric");
        let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        b.append(&wrap);
        b.append(&pct);
        li.set_child(Some(&b));
        actions::attach_ctx(&b, li);
        let item = li.property_expression("item");
        item.chain_property::<RowObject>("fraction").bind(&bar, "fraction", gtk::Widget::NONE);
        item.chain_property::<RowObject>("pct-text").bind(&pct, "label", gtk::Widget::NONE);
        // "share" + accent-N: the class list is rebuilt from the row's accent
        let classes = item.chain_property::<RowObject>("accent");
        gtk::ClosureExpression::new::<Vec<String>>(
            [classes],
            glib::closure!(|_: Option<glib::Object>, accent: Vec<String>| {
                let mut v = vec!["share".to_string()];
                v.extend(accent);
                v
            }),
        )
        .bind(&wrap, "css-classes", gtk::Widget::NONE);
    });
    share_f
}

fn add_contents_columns(view: &gtk::ColumnView) {
    let (name_f, share_f) = (name_cell(), share_cell());
    let cols: [(&str, gtk::SignalListItemFactory, gtk::CustomSorter, i32, bool); 6] = [
        ("Name", name_f, sorter(|a, b| a.sort_name().cmp(&b.sort_name())), 0, true),
        ("Size", label_cell("size-text", 1.0, false), sorter(|a, b| a.size().cmp(&b.size())), 95, false),
        ("Share", share_f, sorter(|a, b| a.size().cmp(&b.size())), 190, false),
        ("Files", label_cell("files-text", 1.0, true), sorter(|a, b| a.sort_files().cmp(&b.sort_files())), 85, false),
        ("Modified", label_cell("mtime-text", 0.0, true), sorter(|a, b| a.sort_mtime().cmp(&b.sort_mtime())), 115, false),
        (
            "Type",
            label_cell("kind-text", 0.0, true),
            sorter(|a, b| (!a.is_dir(), a.kind_text()).cmp(&(!b.is_dir(), b.kind_text()))),
            170,
            false,
        ),
    ];
    let mut size_col = None;
    for (title, factory, sorter, width, expand) in cols {
        let c = gtk::ColumnViewColumn::new(Some(title), Some(factory));
        c.set_sorter(Some(&sorter));
        c.set_resizable(true);
        c.set_expand(expand);
        if width > 0 {
            c.set_fixed_width(width);
        }
        view.append_column(&c);
        if title == "Size" {
            size_col = Some(c);
        }
    }
    view.sort_by_column(size_col.as_ref(), gtk::SortType::Descending);
}

// ------------------------------------------------------------------ signals and actions

fn wire(state: &Rc<State>, drive_pop: &gtk::Popover) {
    let weak = Rc::downgrade(state);
    let ui = &state.ui;

    // sidebar tree model (needs the state to create child stores)
    let w = weak.clone();
    let model = gtk::TreeListModel::new(ui.side_root.clone(), false, false, move |item| {
        let st = w.upgrade()?;
        let row = item.downcast_ref::<RowObject>()?;
        st.side_children(row.dir())
    });
    let sort = gtk::SortListModel::new(Some(model.clone()), Some(gtk::TreeListRowSorter::new(Some(ui.side_sorter.clone()))));
    let sel = gtk::SingleSelection::new(Some(sort.clone()));
    ui.side_view.set_model(Some(&sel));
    let w = weak.clone();
    sel.connect_selected_item_notify(move |sel| {
        let Some(st) = w.upgrade() else { return };
        if st.syncing.get() {
            return;
        }
        if let Some(row) = sel.selected_item().and_downcast::<gtk::TreeListRow>() {
            if let Some(r) = row.item().and_downcast::<RowObject>() {
                if r.dir() != st.current.get() {
                    st.show_folder(r.dir(), true);
                    if st.ui.split.is_collapsed() {
                        st.ui.split.set_show_sidebar(false);
                    }
                }
            }
        }
    });
    let _ = state.side_model.set(model);
    let _ = state.side_sort.set(sort);
    let _ = state.side_sel.set(sel);

    let w = weak.clone();
    ui.contents_view.connect_activate(move |_, pos| {
        let Some(st) = w.upgrade() else { return };
        let Some(row) = st.ui.contents_sort.item(pos).and_downcast::<RowObject>() else { return };
        if row.is_dir() {
            st.show_folder(row.dir(), false);
        } else if let Some(i) = row.file_index() {
            let path = {
                let tree = st.tree.borrow();
                let d = tree.get(row.dir());
                tree.file_path(row.dir(), &d.files[i as usize])
            };
            let _ = gio::AppInfo::launch_default_for_uri(&gio::File::for_path(path).uri(), None::<&gio::AppLaunchContext>);
        }
    });

    // Biggest: activating a file opens its folder in Contents
    let w = weak.clone();
    ui.pages.biggest.view.connect_activate(move |_, pos| {
        let Some(st) = w.upgrade() else { return };
        let Some(row) = st.ui.pages.biggest.sorted.item(pos).and_downcast::<RowObject>() else { return };
        st.show_folder(row.dir(), false);
        st.set_page("contents");
    });
    let w = weak.clone();
    ui.view_stack.connect_visible_child_name_notify(move |_| {
        if let Some(st) = w.upgrade() {
            st.refresh_stats(true);
            st.ui.map.queue_draw();
        }
    });

    // treemap
    let w = weak.clone();
    ui.map.set_draw_func(move |_, cr, width, height| {
        if let Some(st) = w.upgrade() {
            st.draw_map(cr, width, height);
        }
    });
    let motion = gtk::EventControllerMotion::new();
    let w = weak.clone();
    motion.connect_motion(move |_, x, y| {
        if let Some(st) = w.upgrade() {
            let h = treemap::hit(&st.map_hits.borrow(), x, y);
            if st.map_hover.replace(h) != h {
                st.ui.map.queue_draw();
            }
        }
    });
    let w = weak.clone();
    motion.connect_leave(move |_| {
        if let Some(st) = w.upgrade() {
            if st.map_hover.replace(None).is_some() {
                st.ui.map.queue_draw();
            }
        }
    });
    ui.map.add_controller(motion);
    let click = gtk::GestureClick::builder().button(1).build();
    let w = weak.clone();
    click.connect_released(move |_, _, x, y| {
        let Some(st) = w.upgrade() else { return };
        let target = {
            let hits = st.map_hits.borrow();
            treemap::hit(&hits, x, y).map(|i| (hits[i].dir, hits[i].parent))
        };
        match target {
            Some((dir, _)) if dir != NONE => st.show_folder(dir, false),
            Some((_, parent)) if parent != st.current.get() => st.show_folder(parent, false),
            _ => {}
        }
    });
    ui.map.add_controller(click);
    let w = weak.clone();
    ui.map.connect_query_tooltip(move |_, x, y, _, tip| {
        let Some(st) = w.upgrade() else { return false };
        let hits = st.map_hits.borrow();
        let Some(h) = treemap::hit(&hits, x as f64, y as f64).map(|i| &hits[i]) else { return false };
        let tree = st.tree.borrow();
        let path = if h.dir != NONE {
            tree.path(h.dir).display().to_string()
        } else if h.file.is_some() {
            tree.path(h.parent).join(&h.label).display().to_string()
        } else {
            format!("{} in {}", h.label, tree.path(h.parent).display())
        };
        tip.set_text(Some(&format!("{path}\n{}", human(h.value))));
        true
    });

    // usage strip
    let w = weak.clone();
    ui.strip.set_draw_func(move |_, cr, width, height| {
        if let Some(st) = w.upgrade() {
            st.draw_strip(cr, width as f64, height as f64);
        }
    });

    // drive lists
    let w = weak.clone();
    drive_pop.connect_show(move |_| {
        if let Some(st) = w.upgrade() {
            st.fill_drives(&st.ui.drive_pop_list);
        }
    });
    let w = weak.clone();
    glib::idle_add_local_once(move || {
        if let Some(st) = w.upgrade() {
            st.fill_drives(&st.ui.drive_list);
        }
    });

    // actions
    let win = &ui.window;
    let act = |name: &str, accels: &[&str], f: Box<dyn Fn(&Rc<State>)>| {
        let a = gio::SimpleAction::new(name, None);
        let w = weak.clone();
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
    act("open", &["<Control>o"], Box::new(|st| st.choose_folder()));
    act("up", &["<Alt>Up"], Box::new(|st| st.go_up()));
    act("stop", &["Escape"], Box::new(|st| {
        if let Some(s) = st.scanner.borrow().as_ref() {
            s.cancel();
        }
    }));
    act("rescan", &["F5"], Box::new(|st| st.rescan(st.current.get())));
    act("about", &[], Box::new(|st| {
        adw::AboutDialog::builder()
            .application_name("DiskLens")
            .application_icon("drive-harddisk-symbolic")
            .version(env!("CARGO_PKG_VERSION"))
            .comments("See what's using your disk space.")
            .license_type(gtk::License::MitX11)
            .build()
            .present(Some(&st.ui.window));
    }));
    let toggle = |name: &str, initial: bool, f: Box<dyn Fn(&Rc<State>, bool)>| {
        let a = gio::SimpleAction::new_stateful(name, None, &initial.to_variant());
        let w = weak.clone();
        a.connect_change_state(move |a, v| {
            let Some(v) = v else { return };
            a.set_state(v);
            if let Some(st) = w.upgrade() {
                f(&st, v.get::<bool>().unwrap_or(false));
            }
        });
        win.add_action(&a);
    };
    toggle("ondisk", false, Box::new(|st, v| {
        st.ondisk.set(v);
        st.sync_views();
    }));
    toggle("hidden", true, Box::new(|st, v| {
        st.show_hidden.set(v);
        st.rebuild_views();
    }));
    toggle("onefs", true, Box::new(|st, v| st.one_fs.set(v)));
}

// ------------------------------------------------------------------ behaviour

impl State {
    fn kind(&self, name: &str) -> (String, gio::Icon) {
        let ext = ext_of(name);
        if let Some(k) = self.kinds.borrow().get(&ext) {
            return k.clone();
        }
        let k = if ext == NO_EXT {
            ("File".to_string(), gio::ThemedIcon::new("text-x-generic-symbolic").upcast())
        } else {
            let (ctype, _) = gio::content_type_guess(Some(name), None::<&[u8]>);
            let desc = if ext == RAR_PARTS {
                "RAR archive part".to_string()
            } else if ctype == "application/octet-stream" {
                format!("{} file", ext[1..].to_uppercase())
            } else {
                gio::content_type_get_description(&ctype).to_string()
            };
            (desc, gio::content_type_get_symbolic_icon(&ctype))
        };
        self.kinds.borrow_mut().insert(ext, k.clone());
        k
    }

    fn fill_drives(self: &Rc<Self>, list: &gtk::ListBox) {
        let (w, list) = (Rc::downgrade(self), list.clone());
        glib::spawn_future_local(async move {
            let mounts = gio::spawn_blocking(mounts::list).await.unwrap_or_default();
            if let Some(st) = w.upgrade() {
                st.show_drives(&list, mounts);
            }
        });
    }

    fn show_drives(self: &Rc<Self>, list: &gtk::ListBox, mounts: Vec<mounts::Mount>) {
        list.remove_all();
        let home = glib::home_dir();
        let mut entries: Vec<(String, String, String, Option<f64>)> = vec![(
            home.display().to_string(),
            "Home folder".into(),
            "user-home-symbolic".into(),
            None,
        )];
        for m in mounts {
            let used = m.total.saturating_sub(m.free);
            entries.push((
                m.path.clone(),
                format!("{} of {} used · {}", human(used), human(m.total), m.fstype),
                if m.is_network() { "folder-remote-symbolic" } else { "drive-harddisk-symbolic" }.into(),
                (m.total > 0).then(|| used as f64 / m.total as f64),
            ));
        }
        for (path, sub, icon, frac) in entries {
            let row = adw::ActionRow::builder().title(glib::markup_escape_text(&path)).subtitle(sub).activatable(true).build();
            row.add_prefix(&gtk::Image::from_icon_name(&icon));
            if let Some(f) = frac {
                let lb = gtk::LevelBar::builder().value(f).width_request(110).valign(gtk::Align::Center).build();
                row.add_suffix(&lb);
            }
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            let w = Rc::downgrade(self);
            row.connect_activated(move |r| {
                if let Some(st) = w.upgrade() {
                    if let Some(p) = r.ancestor(gtk::Popover::static_type()) {
                        p.downcast::<gtk::Popover>().unwrap().popdown();
                    }
                    st.start_scan(PathBuf::from(&path));
                }
            });
            list.append(&row);
        }
    }

    fn choose_folder(self: &Rc<Self>) {
        let dlg = gtk::FileDialog::builder().title("Choose a folder to scan").build();
        let w = Rc::downgrade(self);
        dlg.select_folder(Some(&self.ui.window), None::<&gio::Cancellable>, move |res| {
            if let (Some(st), Ok(f)) = (w.upgrade(), res) {
                if let Some(p) = f.path() {
                    st.start_scan(p);
                }
            }
        });
    }

    pub fn start_scan(self: &Rc<Self>, path: PathBuf) {
        let path = match std::fs::canonicalize(&path) {
            Ok(p) if p.is_dir() => p,
            _ => {
                self.toast(&format!("Can't scan {}", path.display()));
                return;
            }
        };
        if let Some(s) = self.scanner.borrow().as_ref() {
            s.cancel();
        }
        let dev = std::fs::metadata(&path).ok().map(|m| std::os::unix::fs::MetadataExt::dev(&m));
        self.root_dev.set(dev);
        *self.tree.borrow_mut() = Tree::new(&path);
        self.side_stores.borrow_mut().clear();
        *self.contents.borrow_mut() = None;
        self.errors.borrow_mut().clear();
        self.current.set(NONE);
        self.ui.stack.set_visible_child_name("main");
        self.reset_side_root();
        self.show_folder(Tree::ROOT, false);
        let opts = ScanOptions { one_fs: self.one_fs.get(), ..Default::default() };
        match Scanner::start(Tree::ROOT, path, 1, dev, opts) {
            Ok(s) => self.run_scanner(s),
            Err(e) => self.toast(&format!("Can't scan: {e}")),
        }
    }

    fn reset_side_root(&self) {
        let row = {
            let tree = self.tree.borrow();
            RowObject::for_dir(&tree, Tree::ROOT, NONE, true, self.ondisk.get(), now())
        };
        self.ui.side_root.remove_all();
        self.ui.side_root.append(&row);
        if let Some(r) = self.side_model.get().and_then(|m| m.row(0)) {
            r.set_expanded(true);
        }
    }

    fn rescan(self: &Rc<Self>, dir: u32) {
        if dir == NONE || self.scanner.borrow().is_some() {
            return;
        }
        let path = {
            let mut tree = self.tree.borrow_mut();
            tree.reset(dir);
            tree.path(dir)
        };
        if let Some((store, synced)) = self.side_stores.borrow_mut().get_mut(&dir) {
            store.remove_all();
            *synced = 0;
        }
        let inside = { self.tree.borrow().is_inside(self.current.get(), dir) };
        if inside {
            self.show_folder(dir, false);
        }
        let next = self.tree.borrow().next_id();
        let opts = ScanOptions { one_fs: self.one_fs.get(), ..Default::default() };
        match Scanner::start(dir, path, next, self.root_dev.get(), opts) {
            Ok(s) => self.run_scanner(s),
            Err(e) => self.toast(&format!("Can't rescan: {e}")),
        }
    }

    fn run_scanner(self: &Rc<Self>, s: Scanner) {
        *self.scanner.borrow_mut() = Some(s);
        self.ui.progress.set_visible(true);
        self.ui.stop_btn.set_visible(true);
        self.ui.rescan_btn.set_visible(false);
        if self.ticking.replace(true) {
            return;
        }
        let w = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(200), move || match w.upgrade() {
            Some(st) => st.tick(),
            None => glib::ControlFlow::Break,
        });
    }

    fn tick(self: &Rc<Self>) -> glib::ControlFlow {
        let mut done = false;
        {
            let sc = self.scanner.borrow();
            let Some(s) = sc.as_ref() else {
                self.ticking.set(false);
                return glib::ControlFlow::Break;
            };
            let mut tree = self.tree.borrow_mut();
            let mut errors = self.errors.borrow_mut();
            for _ in 0..200_000 {
                match s.rx.try_recv() {
                    Ok(Msg::Dir(r)) => tree.apply(r),
                    Ok(Msg::Error(p, e)) => errors.push((p, e)),
                    Ok(Msg::Done) | Err(TryRecvError::Disconnected) => {
                        done = true;
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                }
            }
        }
        let n = self.tick_n.get() + 1;
        self.tick_n.set(n);
        self.ui.progress.pulse();
        if done || n % 2 == 0 {
            self.sync_views();
        }
        self.update_status(done);
        if done {
            let s = self.scanner.borrow_mut().take();
            if let Some(s) = s {
                *self.last_scan.borrow_mut() = if s.is_cancelled() {
                    "stopped (partial)".into()
                } else {
                    format!("{:.1} s", s.elapsed().as_secs_f64())
                };
            }
            self.update_status(true);
            self.ui.progress.set_visible(false);
            self.ui.stop_btn.set_visible(false);
            self.ui.rescan_btn.set_visible(true);
            self.ticking.set(false);
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    }

    fn update_status(&self, done: bool) {
        let tree = self.tree.borrow();
        let root = tree.get(Tree::ROOT);
        let sub = if !done {
            let sc = self.scanner.borrow();
            let files = sc.as_ref().map(|s| s.progress.files.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(0);
            format!("Scanning… {} files", fmt_int(files))
        } else {
            let errs = self.errors.borrow().len();
            // file count is live, so it follows deletions
            let mut s = format!(
                "{} · {} files · {}",
                human(root.value(self.ondisk.get())),
                fmt_int(root.nfiles),
                self.last_scan.borrow()
            );
            if errs > 0 {
                s.push_str(&format!(" · ⚠ {errs}"));
            }
            s
        };
        self.ui.side_title.set_subtitle(&sub);
        let errs = self.errors.borrow().len();
        self.ui.side_title.set_tooltip_text(Some(&format!(
            "{}\n{} files · {} folders · {} unreadable",
            tree.path(Tree::ROOT).display(),
            fmt_int(root.nfiles),
            fmt_int(root.ndirs),
            errs
        )));
    }

    /// Child folder rows for the sidebar tree (TreeListModel create function).
    fn side_children(&self, dir: u32) -> Option<gio::ListModel> {
        let (rows, n, scanned) = {
            let tree = self.tree.borrow();
            let d = tree.get(dir);
            let (ondisk, now) = (self.ondisk.get(), now());
            let rows: Vec<RowObject> = d
                .dirs
                .iter()
                .filter(|&&c| visible(self, &tree.get(c).name))
                .map(|&c| RowObject::for_dir(&tree, c, dir, false, ondisk, now))
                .collect();
            (rows, d.dirs.len(), d.scanned)
        };
        if scanned && rows.is_empty() {
            return None;
        }
        let store = gio::ListStore::new::<RowObject>();
        store.extend_from_slice(&rows);
        self.side_stores.borrow_mut().insert(dir, (store.clone(), n));
        Some(store.upcast())
    }

    pub fn show_folder(self: &Rc<Self>, dir: u32, from_side: bool) {
        self.current.set(dir);
        let (rows, ndirs, scanned) = {
            let tree = self.tree.borrow();
            let d = tree.get(dir);
            let (ondisk, now) = (self.ondisk.get(), now());
            let mut rows: Vec<RowObject> = d
                .dirs
                .iter()
                .filter(|&&c| visible(self, &tree.get(c).name))
                .map(|&c| RowObject::for_dir(&tree, c, dir, false, ondisk, now))
                .collect();
            if d.scanned {
                for (i, f) in d.files.iter().enumerate() {
                    if !f.removed && visible(self, d.file_name_bytes(f)) {
                        let kind = self.kind(&d.file_name(f));
                        rows.push(RowObject::for_file(&tree, dir, i as u32, kind, ondisk, now));
                    }
                }
            }
            (rows, d.dirs.len(), d.scanned)
        };
        let store = gio::ListStore::new::<RowObject>();
        store.extend_from_slice(&rows);
        self.ui.contents_sort.set_model(Some(&store));
        *self.contents.borrow_mut() = Some(Contents { dir, store, synced: ndirs, files_added: scanned });
        self.update_header();
        self.ui.up_btn.set_sensitive(self.tree.borrow().get(dir).parent != NONE);
        if self.ui.contents_sort.n_items() > 0 {
            self.ui.contents_view.scroll_to(0, None, gtk::ListScrollFlags::NONE, None);
        }
        if !from_side {
            self.select_in_side(dir);
        }
        self.stats_dirty.set(true);
        self.refresh_stats(true);
        self.invalidate_map();
    }

    /// The tree or the view options changed: repaint the treemap if it's showing.
    fn invalidate_map(&self) {
        self.map_gen.set(self.map_gen.get() + 1);
        self.map_hover.set(None);
        if self.ui.view_stack.visible_child_name().as_deref() == Some("map") {
            self.ui.map.queue_draw();
        }
    }

    fn draw_map(&self, cr: &gtk::cairo::Context, width: i32, height: i32) {
        let dir = self.current.get();
        if dir == NONE || width <= 0 || height <= 0 {
            return;
        }
        let scale = self.ui.map.scale_factor();
        let dark = adw::StyleManager::default().is_dark();
        let key = (dir, self.map_gen.get(), width, height, scale, dark);
        let cached = matches!(self.map_cache.borrow().as_ref(), Some((k, _)) if *k == key);
        if !cached {
            let Ok(surf) = gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, width * scale, height * scale) else { return };
            surf.set_device_scale(scale as f64, scale as f64);
            let Ok(c) = gtk::cairo::Context::new(&surf) else { return };
            let tree = self.tree.borrow();
            let mut p = treemap::Painter {
                cr: &c,
                layout: self.ui.map.create_pango_layout(None),
                tree: &tree,
                ondisk: self.ondisk.get(),
                show_hidden: self.show_hidden.get(),
                hits: Vec::new(),
            };
            p.level(dir, 0.0, 0.0, width as f64, height as f64, 0, None);
            *self.map_hits.borrow_mut() = std::mem::take(&mut p.hits);
            drop(p);
            drop(c);
            *self.map_cache.borrow_mut() = Some((key, surf));
        }
        if let Some((_, surf)) = self.map_cache.borrow().as_ref() {
            let _ = cr.set_source_surface(surf, 0.0, 0.0);
            let _ = cr.paint();
        }
        if let Some(i) = self.map_hover.get() {
            if let Some(h) = self.map_hits.borrow().get(i) {
                let fg = self.ui.map.color();
                treemap::draw_hover(cr, h, (fg.red() as f64, fg.green() as f64, fg.blue() as f64));
            }
        }
    }

    /// Refill Biggest/Types/Ages if one is showing and the data changed.
    /// `now_ok=false` (live scan updates) limits refills to about once a second.
    fn refresh_stats(&self, now_ok: bool) {
        let page = self.ui.view_stack.visible_child_name().unwrap_or_default();
        if !Pages::is_stats_page(&page) || !self.stats_dirty.get() || self.current.get() == NONE {
            return;
        }
        let t = glib::monotonic_time();
        if !now_ok && t - self.stats_at.get() < 1_000_000 {
            return;
        }
        self.stats_dirty.set(false);
        let tree = self.tree.borrow();
        let kind = |name: &str| self.kind(name);
        self.ui.pages.fill(&tree, self.current.get(), self.ondisk.get(), self.show_hidden.get(), now(), &kind);
        // measured after filling, so a slow fill can't hog the main loop during a scan
        self.stats_at.set(glib::monotonic_time());
    }

    fn go_up(self: &Rc<Self>) {
        let parent = { self.tree.borrow().get(self.current.get()).parent };
        if self.current.get() != NONE && parent != NONE {
            self.show_folder(parent, false);
        }
    }

    fn select_in_side(&self, dir: u32) {
        let (Some(sort), Some(sel)) = (self.side_sort.get(), self.side_sel.get()) else { return };
        let mut chain = Vec::new();
        {
            let tree = self.tree.borrow();
            let mut n = dir;
            while n != NONE {
                chain.push(n);
                n = tree.get(n).parent;
            }
        }
        chain.reverse();
        self.syncing.set(true);
        let mut pos = None;
        for target in &chain {
            pos = (0..sort.n_items()).find(|&i| {
                sort.item(i)
                    .and_downcast::<gtk::TreeListRow>()
                    .and_then(|r| r.item().and_downcast::<RowObject>())
                    .is_some_and(|r| r.dir() == *target)
            });
            let Some(p) = pos else { break };
            if *target != dir {
                if let Some(r) = sort.item(p).and_downcast::<gtk::TreeListRow>() {
                    r.set_expanded(true);
                }
            }
        }
        if let Some(p) = pos {
            sel.set_selected(p);
            self.ui.side_view.scroll_to(p, gtk::ListScrollFlags::NONE, None);
        }
        self.syncing.set(false);
    }

    /// Pull new folders/files into the live stores and refresh every visible row.
    fn sync_views(&self) {
        let (ondisk, now) = (self.ondisk.get(), now());
        // new sidebar children (collected first: splicing can re-enter side_children)
        let mut pending = Vec::new();
        {
            let tree = self.tree.borrow();
            for (dir, (store, synced)) in self.side_stores.borrow_mut().iter_mut() {
                let d = tree.get(*dir);
                if d.dirs.len() > *synced {
                    let rows: Vec<RowObject> = d.dirs[*synced..]
                        .iter()
                        .filter(|&&c| visible(self, &tree.get(c).name))
                        .map(|&c| RowObject::for_dir(&tree, c, *dir, false, ondisk, now))
                        .collect();
                    *synced = d.dirs.len();
                    pending.push((store.clone(), rows));
                }
            }
        }
        for (store, rows) in pending {
            store.extend_from_slice(&rows);
        }
        let stores: Vec<gio::ListStore> = self.side_stores.borrow().values().map(|(s, _)| s.clone()).collect();
        {
            let tree = self.tree.borrow();
            for store in stores.iter().chain(std::iter::once(&self.ui.side_root)) {
                for r in store.iter::<RowObject>().flatten() {
                    r.refresh(&tree, ondisk, now);
                }
            }
        }

        // contents
        let mut add = Vec::new();
        let store = {
            let tree = self.tree.borrow();
            let mut c = self.contents.borrow_mut();
            let Some(c) = c.as_mut() else { return };
            let d = tree.get(c.dir);
            for &cd in &d.dirs[c.synced..] {
                if visible(self, &tree.get(cd).name) {
                    add.push(RowObject::for_dir(&tree, cd, c.dir, false, ondisk, now));
                }
            }
            c.synced = d.dirs.len();
            if d.scanned && !c.files_added {
                c.files_added = true;
                for (i, f) in d.files.iter().enumerate() {
                    if !f.removed && visible(self, d.file_name_bytes(f)) {
                        let kind = self.kind(&d.file_name(f));
                        add.push(RowObject::for_file(&tree, c.dir, i as u32, kind, ondisk, now));
                    }
                }
            }
            c.store.clone()
        };
        if !add.is_empty() {
            store.extend_from_slice(&add);
        }
        {
            let tree = self.tree.borrow();
            for r in store.iter::<RowObject>().flatten() {
                r.refresh(&tree, ondisk, now);
            }
        }
        self.update_header();

        // re-sort, keeping the list pinned to the top if it was there
        let at_top = self.ui.contents_scroll.vadjustment().value() < 1.0;
        self.ui.side_sorter.changed(gtk::SorterChange::Different);
        if let Some(s) = self.ui.contents_view.sorter() {
            s.changed(gtk::SorterChange::Different);
        }
        if at_top && self.ui.contents_sort.n_items() > 0 {
            self.ui.contents_view.scroll_to(0, None, gtk::ListScrollFlags::NONE, None);
        }
        self.stats_dirty.set(true);
        self.refresh_stats(self.scanner.borrow().is_none());
        self.invalidate_map();
    }

    /// Hidden-files toggle: rebuild the sidebar and Contents from scratch.
    fn rebuild_views(self: &Rc<Self>) {
        if self.current.get() == NONE {
            return;
        }
        let cur = self.current.get();
        self.side_stores.borrow_mut().clear();
        self.reset_side_root();
        self.show_folder(cur, false);
    }

    /// Title, counts, colour ranks, usage strip and legend for the current folder.
    fn update_header(&self) {
        let dir = self.current.get();
        if dir == NONE {
            return;
        }
        let ondisk = self.ondisk.get();
        {
            let tree = self.tree.borrow();
            let d = tree.get(dir);
            let title = if d.parent == NONE { tree.path(dir).display().to_string() } else { d.name_str().into_owned() };
            self.ui.folder_title.set_label(&title);
            self.ui.folder_sub.set_label(&format!(
                "{} · {} files · {} folders",
                human(d.value(ondisk)),
                fmt_int(d.nfiles),
                fmt_int(d.ndirs)
            ));
            self.ui.window.set_title(Some(&format!("{title} – DiskLens")));
        }
        let Some(store) = self.contents.borrow().as_ref().map(|c| c.store.clone()) else { return };
        let mut rows: Vec<RowObject> = store.iter::<RowObject>().flatten().collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.size()));
        let total: u64 = { self.tree.borrow().get(dir).value(ondisk) };
        let mut strip = Vec::new();
        let mut shown = 0u64;
        for (i, r) in rows.iter().enumerate() {
            let rank = (i < TOP_COLOURED && r.size() > 0 && total > 0 && r.size() * 100 >= total).then_some(i);
            r.set_rank(rank);
            if let Some(k) = rank {
                strip.push((r.size(), Some(k), r.name()));
                shown += r.size();
            }
        }
        if total > shown {
            strip.push((total - shown, None, "Other".to_string()));
        }
        let key: String = strip.iter().map(|(v, _, n)| format!("{n}{}", human(*v))).collect();
        *self.strip_data.borrow_mut() = strip;
        self.ui.strip.queue_draw();
        if *self.legend_key.borrow() != key {
            *self.legend_key.borrow_mut() = key;
            self.rebuild_legend();
        }
    }

    fn rebuild_legend(&self) {
        while let Some(c) = self.ui.legend.first_child() {
            self.ui.legend.remove(&c);
        }
        for (v, rank, name) in self.strip_data.borrow().iter() {
            let dot = gtk::Box::builder().valign(gtk::Align::Center).build();
            dot.add_css_class("dot");
            dot.add_css_class(&match rank {
                Some(k) => format!("accent-{k}"),
                None => "accent-none".into(),
            });
            let label = gtk::Label::builder()
                .label(format!("{name}  {}", human(*v)))
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .max_width_chars(26)
                .build();
            label.add_css_class("dim-label");
            label.add_css_class("numeric");
            let chip = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            chip.append(&dot);
            chip.append(&label);
            self.ui.legend.append(&chip);
        }
    }

    fn draw_strip(&self, cr: &gtk::cairo::Context, w: f64, h: f64) {
        let data = self.strip_data.borrow();
        let total: u64 = data.iter().map(|d| d.0).sum();
        let fg = self.ui.strip.color();
        let rr = |x: f64, width: f64| {
            let r = (h / 2.0).min(width / 2.0);
            cr.new_sub_path();
            cr.arc(x + width - r, r, r, -std::f64::consts::FRAC_PI_2, 0.0);
            cr.arc(x + width - r, h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
            cr.arc(x + r, h - r, r, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
            cr.arc(x + r, r, r, std::f64::consts::PI, 1.5 * std::f64::consts::PI);
            cr.close_path();
        };
        // empty track
        cr.set_source_rgba(fg.red() as f64, fg.green() as f64, fg.blue() as f64, 0.08);
        rr(0.0, w);
        let _ = cr.fill();
        if total == 0 {
            return;
        }
        let gap = 3.0;
        let usable = w - gap * (data.len().saturating_sub(1)) as f64;
        let mut x = 0.0;
        for (v, rank, _) in data.iter() {
            let width = (*v as f64 / total as f64 * usable).max(if *v > 0 { 2.0 } else { 0.0 });
            match rank {
                Some(k) => {
                    let (r, g, b) = ACCENTS[*k];
                    cr.set_source_rgb(r, g, b);
                }
                None => cr.set_source_rgba(fg.red() as f64, fg.green() as f64, fg.blue() as f64, 0.30),
            }
            rr(x, width);
            let _ = cr.fill();
            x += width + gap;
        }
    }

    pub fn toast(&self, text: &str) {
        if let Some(t) = self.ui.window.content().and_downcast::<adw::ToastOverlay>() {
            t.add_toast(adw::Toast::new(text));
        }
    }

    pub fn cancel_scan(&self) {
        if let Some(s) = self.scanner.borrow().as_ref() {
            s.cancel();
        }
    }

    pub fn is_scanning(&self) -> bool {
        self.scanner.borrow().is_some()
    }

    pub fn biggest_child_dir(&self) -> Option<u32> {
        let tree = self.tree.borrow();
        let d = tree.get(self.current.get());
        d.dirs.iter().copied().max_by_key(|&c| tree.get(c).value(self.ondisk.get()))
    }

    pub fn set_page(&self, name: &str) {
        self.ui.view_stack.set_visible_child_name(name);
    }
}
