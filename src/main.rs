mod convert;
mod device;
mod eject;
mod gpod;
mod library;

use std::cell::RefCell;
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
    let refresh = move |monitor: &gio::VolumeMonitor| {
        show_ipods(monitor, &device_list, &stack, &navigation, &open_library);
    };
    refresh(&monitor);

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
        let subtitle = format!(
            "{} free of {} · {}",
            glib::format_size(ipod.free_bytes),
            glib::format_size(ipod.total_bytes),
            ipod.mount_path.display()
        );
        let row = adw::ActionRow::builder()
            .title(&ipod.name)
            .subtitle(subtitle)
            .use_markup(false)
            .activatable(true)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("multimedia-player-symbolic"));
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));

        // Each closure needs its own copies of what it uses.
        let mount_path = ipod.mount_path.clone();
        let navigation = navigation.clone();
        let open_library = open_library.clone();
        row.connect_activated(move |row| match Library::new(&mount_path, &navigation) {
            Ok(library) => {
                navigation.push(library.page());
                open_library.replace(Some(library));
            }
            Err(error) => {
                let dialog = adw::AlertDialog::new(
                    Some("Can't Read the Music Database"),
                    Some(&error.to_string()),
                );
                dialog.add_response("close", "Close");
                dialog.present(Some(row));
            }
        });
        list.append(&row);
    }

    stack.set_visible_child_name(if ipods.is_empty() { "empty" } else { "devices" });
}
