mod device;
mod gpod;

use std::path::Path;
use std::time::Duration;

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

    // The navigation view holds a stack of pages: the device list at the
    // bottom, and a song list pushed on top when an iPod is clicked. It adds
    // the back button by itself.
    let navigation = adw::NavigationView::new();
    let root_page = page_with_header("Oxipod", &stack);
    root_page.set_tag(Some("root"));
    navigation.add(&root_page);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Oxipod")
        .default_width(600)
        .default_height(550)
        .content(&navigation)
        .build();

    // The volume monitor tells us when drives are mounted or unmounted.
    let monitor = gio::VolumeMonitor::get();
    let refresh = move |monitor: &gio::VolumeMonitor| {
        show_ipods(monitor, &device_list, &stack, &navigation);
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
) {
    let ipods = device::find_ipods(monitor);

    // The iPod was unplugged while its songs were shown: go back to the list.
    if ipods.is_empty() {
        navigation.pop_to_tag("root");
    }

    list.remove_all();
    for ipod in ipods.iter() {
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

        // Each closure needs its own copy of the path and the navigation view.
        let mount_path = ipod.mount_path.clone();
        let navigation = navigation.clone();
        row.connect_activated(move |_| {
            navigation.push(&songs_page(&mount_path));
        });
        list.append(&row);
    }

    stack.set_visible_child_name(if ipods.is_empty() { "empty" } else { "devices" });
}

/// The page that lists every song on the iPod mounted at `mount_path`.
fn songs_page(mount_path: &Path) -> adw::NavigationPage {
    let db = match gpod::Database::open(mount_path) {
        Ok(db) => db,
        Err(error) => {
            let status = adw::StatusPage::builder()
                .icon_name("dialog-error-symbolic")
                .title("Can't Read the Music Database")
                .description(glib::markup_escape_text(&error.to_string()))
                .build();
            return page_with_header("Error", &status);
        }
    };

    let tracks = db.tracks();
    let info = db.device_info();
    let name = db.name().unwrap_or_else(|| "iPod".into());
    // `db` is dropped here, at the end of the function, which frees
    // libgpod's memory. `tracks` holds copies, so it stays valid.

    let total: Duration = tracks.iter().map(|track| track.length).sum();
    let model = [info.generation, info.model_number]
        .into_iter()
        .flatten() // skip the parts libgpod didn't know
        .collect::<Vec<_>>()
        .join(", ");

    let group = adw::PreferencesGroup::builder()
        .title(format!("{} Songs · {}", tracks.len(), format_duration(total)))
        .description(glib::markup_escape_text(&model))
        .build();
    for track in &tracks {
        let subtitle = [track.artist.as_deref(), track.album.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" — ");
        let row = adw::ActionRow::builder()
            .title(track.title.as_deref().unwrap_or("Unknown Title"))
            .subtitle(subtitle)
            .use_markup(false)
            .build();
        let length = gtk::Label::builder()
            .label(format_duration(track.length))
            .css_classes(["dim-label", "numeric"])
            .build();
        row.add_suffix(&length);
        group.add(&row);
    }

    let page = adw::PreferencesPage::new();
    page.add(&group);
    page_with_header(&name, &page)
}

/// Wraps `content` in a navigation page with a header bar.
fn page_with_header(title: &str, content: &impl IsA<gtk::Widget>) -> adw::NavigationPage {
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(content));
    adw::NavigationPage::new(&toolbar, title)
}

/// Formats a length as "3:05", or "1:02:07" when it's an hour or longer.
fn format_duration(length: Duration) -> String {
    let seconds = length.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}
