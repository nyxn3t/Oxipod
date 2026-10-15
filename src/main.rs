mod convert;
mod covers;
mod device;
mod duplicates;
mod eject;
mod export;
mod gpod;
mod library;
mod settings;
mod songs;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use library::Library;

const APP_ID: &str = "io.github.nyxn3t.Oxipod";

/// The iPod page that is open, if any. Shared between the signal handlers
/// below, hence `Rc<RefCell<…>>`: shared ownership, changeable contents.
type OpenLibrary = Rc<RefCell<Option<Rc<Library>>>>;

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &adw::Application) {
    // Use the app icon for the window (once it's installed).
    gtk::Window::set_default_icon_name(APP_ID);

    // Shown when no iPod is connected.
    let empty_page = adw::StatusPage::builder()
        .icon_name("multimedia-player-symbolic")
        .title("No iPod Connected")
        .description("Plug in your iPod and open it in your file manager")
        .build();

    // One row per connected iPod.
    let device_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .valign(gtk::Align::Start)
        .build();
    let device_page = adw::Clamp::builder()
        .child(&device_list)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();

    let stack = gtk::Stack::new();
    stack.add_named(&empty_page, Some("empty"));
    stack.add_named(&device_page, Some("devices"));

    let header = adw::HeaderBar::new();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));

    // The navigation view holds a stack of pages: the device list at the
    // bottom, an iPod's page on top of it, and maybe a playlist on top of
    // that. It adds the back buttons by itself.
    let navigation = adw::NavigationView::new();
    let root_page = adw::NavigationPage::new(&toolbar, "Oxipod");
    root_page.set_tag(Some("root"));
    navigation.add(&root_page);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Oxipod")
        .default_width(640)
        .default_height(640)
        .content(&navigation)
        .build();

    // Close the open library (and its database) once its page is gone.
    let open_library: OpenLibrary = Rc::default();
    let open = open_library.clone();
    navigation.connect_popped(move |_, page| {
        let mut open = open.borrow_mut();
        if open.as_ref().is_some_and(|library| library.page() == page) {
            *open = None;
        }
    });

    // The volume monitor tells us when drives are mounted or unmounted.
    let monitor = gio::VolumeMonitor::get();
    let list = device_list.clone();
    // The "Copy All" action opens an iPod's page too, so it needs these.
    let (nav, open) = (navigation.clone(), open_library.clone());
    let refresh = move |monitor: &gio::VolumeMonitor| {
        show_ipods(monitor, &device_list, &stack, &navigation, &open_library);
    };
    refresh(&monitor);

    // The ⋮ menu on an iPod in the list. One action serves every row: the
    // row's menu item passes along which iPod it is for, as a pair of texts
    // (its mount path and its current name), the "target" of the action.
    let actions = gio::SimpleActionGroup::new();
    window.insert_action_group("device", Some(&actions));
    let rename = gio::SimpleAction::new("rename", Some(&<(String, String)>::static_variant_type()));
    {
        let window = window.clone();
        let refresh = refresh.clone();
        // After a rename the list is rebuilt, to show the new name. This
        // closure is that step, so the dialog code doesn't need to know how.
        let reload: Rc<dyn Fn()> = Rc::new(move || refresh(&gio::VolumeMonitor::get()));
        rename.connect_activate(move |_, target| {
            if let Some((mount_path, name)) = target.and_then(|target| target.get::<(String, String)>()) {
                ask_new_name(&window, &list, PathBuf::from(mount_path), name, reload.clone());
            }
        });
    }
    actions.add_action(&rename);

    // "Copy All to Computer…": opens the iPod's page and starts copying
    // everything on it. The target is the iPod's mount path.
    let copy_all = gio::SimpleAction::new("copy-all", Some(glib::VariantTy::STRING));
    {
        let window = window.clone();
        copy_all.connect_activate(move |_, target| {
            if let Some(mount_path) = target.and_then(|target| target.str())
                && let Some(library) = open_ipod(Path::new(mount_path), &nav, &open, &window)
            {
                library.copy_all();
            }
        });
    }
    actions.add_action(&copy_all);

    // `refresh` is moved into the first handler, so the others get clones.
    // A closure can be cloned when everything it captures can be cloned,
    // and GTK widgets are reference-counted, so cloning one is cheap.
    let on_change = refresh.clone();
    monitor.connect_mount_added(move |monitor, _| on_change(monitor));
    let on_change = refresh.clone();
    monitor.connect_mount_removed(move |monitor, _| on_change(monitor));
    monitor.connect_mount_changed(move |monitor, _| refresh(monitor));

    // The monitor stops sending signals once nothing holds a reference to it,
    // so the window keeps one for as long as it is open.
    window.connect_destroy(move |_| {
        let _ = &monitor;
    });

    window.present();
}

/// Opens the page of the iPod mounted at `mount_path`, or explains why that
/// isn't possible. `parent` is the widget an error dialog belongs to.
///
/// `impl IsA<gtk::Widget>` means "any kind of widget": a row, a window...
fn open_ipod(
    mount_path: &Path,
    navigation: &adw::NavigationView,
    open_library: &OpenLibrary,
    parent: &impl IsA<gtk::Widget>,
) -> Option<Rc<Library>> {
    match Library::new(mount_path, navigation) {
        Ok(library) => {
            navigation.push(library.page());
            open_library.replace(Some(library.clone()));
            Some(library)
        }
        Err(error) => {
            let dialog = adw::AlertDialog::new(Some("Can't Read the Music Database"), Some(&error.to_string()));
            dialog.add_response("close", "Close");
            dialog.present(Some(parent));
            None
        }
    }
}

/// Rebuilds the device list and switches between the list and the empty page.
fn show_ipods(
    monitor: &gio::VolumeMonitor,
    list: &gtk::ListBox,
    stack: &gtk::Stack,
    navigation: &adw::NavigationView,
    open_library: &OpenLibrary,
) {
    let ipods = device::find_ipods(monitor);

    // The open iPod was unplugged or ejected: go back to the device list.
    // The borrow ends with this statement, before pop_to_tag runs the popped
    // handler that borrows `open_library` again.
    let unplugged = open_library
        .borrow()
        .as_ref()
        .is_some_and(|library| !ipods.iter().any(|ipod| ipod.mount_path == library.mount_path()));
    if unplugged {
        navigation.pop_to_tag("root");
    }

    list.remove_all();
    for ipod in &ipods {
        // The mount path has the user's name in it (/run/media/<user>/…), so
        // it's only in the tooltip: screenshots of the row stay anonymous.
        let subtitle = format!(
            "{} free of {}",
            glib::format_size(ipod.free_bytes),
            glib::format_size(ipod.total_bytes)
        );
        let row = adw::ActionRow::builder()
            .title(&ipod.name)
            .subtitle(subtitle)
            .tooltip_text(ipod.mount_path.display().to_string())
            .use_markup(false)
            .activatable(true)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("multimedia-player-symbolic"));

        // Settings for this iPod. Clicking the button doesn't open the iPod:
        // a MenuButton keeps its clicks to itself.
        let menu = gio::Menu::new();
        let copy_all = gio::MenuItem::new(Some("Copy All to Computer…"), None);
        let path_target = ipod.mount_path.to_string_lossy().to_variant();
        copy_all.set_action_and_target_value(Some("device.copy-all"), Some(&path_target));
        menu.append_item(&copy_all);
        let rename = gio::MenuItem::new(Some("Rename…"), None);
        let target = (ipod.mount_path.to_string_lossy().into_owned(), ipod.name.clone()).to_variant();
        rename.set_action_and_target_value(Some("device.rename"), Some(&target));
        menu.append_item(&rename);
        let menu_button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .menu_model(&menu)
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .tooltip_text("Settings")
            .build();
        row.add_suffix(&menu_button);
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));

        // Each closure needs its own copies of what it uses.
        let mount_path = ipod.mount_path.clone();
        let navigation = navigation.clone();
        let open_library = open_library.clone();
        row.connect_activated(move |row| {
            open_ipod(&mount_path, &navigation, &open_library, row);
        });
        list.append(&row);
    }

    stack.set_visible_child_name(if ipods.is_empty() { "empty" } else { "devices" });
}

/// Asks for a new name for an iPod and renames it. `reload` rebuilds the
/// device list afterwards.
///
/// `Rc<dyn Fn()>` is "any function without parameters, shared by several
/// owners": the dialog's button handler is called later, so it has to own
/// what it will need.
fn ask_new_name(
    window: &adw::ApplicationWindow,
    list: &gtk::ListBox,
    mount_path: PathBuf,
    current: String,
    reload: Rc<dyn Fn()>,
) {
    let entry = adw::EntryRow::builder().title("Name").text(&current).activates_default(true).build();
    let fields = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    fields.append(&entry);

    let dialog = adw::AlertDialog::new(Some("Rename iPod"), Some("This is the name iTunes and the iPod itself show."));
    dialog.set_extra_child(Some(&fields));
    dialog.add_responses(&[("cancel", "Cancel"), ("rename", "Rename")]);
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
    // Nothing has changed yet, so there is nothing to rename.
    dialog.set_response_enabled("rename", false);
    dialog.set_default_response(Some("rename"));
    dialog.set_close_response("cancel");

    // "Rename" only works when there is a new name.
    let dialog_ref = dialog.downgrade();
    let old_name = current.clone();
    entry.connect_changed(move |entry| {
        if let Some(dialog) = dialog_ref.upgrade() {
            let text = entry.text();
            let name = text.trim();
            dialog.set_response_enabled("rename", !name.is_empty() && name != old_name);
        }
    });

    let name_entry = entry.clone();
    let (window_ref, list) = (window.clone(), list.clone());
    dialog.connect_response(Some("rename"), move |_, _| {
        let new_name = name_entry.text().trim().to_string();
        let (mount_path, window, list, reload) =
            (mount_path.clone(), window_ref.clone(), list.clone(), reload.clone());
        // Renaming takes a second or two. This future keeps GTK responsive
        // while it waits, like adding songs does.
        glib::spawn_future_local(async move {
            // Nobody should open the iPod while its database is being written.
            list.set_sensitive(false);
            let result = device::rename(mount_path, new_name).await;
            list.set_sensitive(true);
            match result {
                Ok(()) => reload(),
                Err(error) => {
                    let dialog = adw::AlertDialog::new(Some("Couldn't Rename the iPod"), Some(&error.to_string()));
                    dialog.add_response("close", "Close");
                    dialog.present(Some(&window));
                }
            }
        });
    });
    dialog.present(Some(window));
    entry.grab_focus();
}
