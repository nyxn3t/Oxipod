mod device;

use adw::prelude::*;
use gtk::{gio, glib};

const APP_ID: &str = "io.github.nyxn3t.Oxipod";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &adw::Application) {
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

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&stack));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Oxipod")
        .default_width(600)
        .default_height(450)
        .content(&toolbar)
        .build();

    // The volume monitor tells us when drives are mounted or unmounted.
    let monitor = gio::VolumeMonitor::get();
    let refresh = move |monitor: &gio::VolumeMonitor| {
        show_ipods(monitor, &device_list, &stack);
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
fn show_ipods(monitor: &gio::VolumeMonitor, list: &gtk::ListBox, stack: &gtk::Stack) {
    let ipods = device::find_ipods(monitor);

    list.remove_all();
    for ipod in &ipods {
        let subtitle = format!(
            "{} free of {} · {}",
            glib::format_size(ipod.free_bytes),
            glib::format_size(ipod.total_bytes),
            ipod.mount_path.display()
        );
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&ipod.name))
            .subtitle(glib::markup_escape_text(&subtitle))
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("multimedia-player-symbolic"));
        list.append(&row);
    }

    stack.set_visible_child_name(if ipods.is_empty() { "empty" } else { "devices" });
}
