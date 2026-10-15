mod backup;
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

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};

use library::Library;
use settings::Settings;

const APP_ID: &str = "io.github.nyxn3t.Oxipod";

/// The iPod page that is open, if any. Shared between the signal handlers
/// below, hence `Rc<RefCell<…>>`: shared ownership, changeable contents.
type OpenLibrary = Rc<RefCell<Option<Rc<Library>>>>;

/// What the device list needs to open an iPod, and to offer a backup first.
/// Cloning it is cheap: every part is a reference to the same thing.
#[derive(Clone)]
struct Context {
    window: adw::ApplicationWindow,
    navigation: adw::NavigationView,
    open_library: OpenLibrary,
    /// The iPods (by mount path) that were backed up since the app started:
    /// they aren't offered another backup right away.
    backed_up: Rc<RefCell<HashSet<PathBuf>>>,
}

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
    let main_menu = gio::Menu::new();
    main_menu.append(Some("Offer a Backup When Opening an iPod"), Some("win.ask-backup"));
    let about_section = gio::Menu::new();
    about_section.append(Some("About Oxipod"), Some("win.about"));
    main_menu.append_section(None, &about_section);
    header.pack_end(
        &gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&main_menu)
            .primary(true)
            .tooltip_text("Main Menu")
            .build(),
    );
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

    // The switch in the main menu. A stateful action with a true/false state
    // is drawn as a check mark, and activating it flips the state.
    let ask_backup = gio::SimpleAction::new_stateful("ask-backup", None, &Settings::load().ask_for_backup().to_variant());
    ask_backup.connect_change_state(|action, state| {
        if let Some(ask) = state.and_then(|state| state.get::<bool>()) {
            action.set_state(&ask.to_variant());
            Settings::load().set_ask_for_backup(ask);
        }
    });
    window.add_action(&ask_backup);
    let about = gio::SimpleAction::new("about", None);
    {
        let window = window.clone();
        about.connect_activate(move |_, _| show_about(&window));
    }
    window.add_action(&about);

    let context = Context {
        window: window.clone(),
        navigation: navigation.clone(),
        open_library,
        backed_up: Rc::default(),
    };

    // The volume monitor tells us when drives are mounted or unmounted.
    let monitor = gio::VolumeMonitor::get();
    let list = device_list.clone();
    let refresh = {
        let context = context.clone();
        move |monitor: &gio::VolumeMonitor| show_ipods(monitor, &device_list, &stack, &context)
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
        let context = context.clone();
        copy_all.connect_activate(move |_, target| {
            if let Some(mount_path) = target.and_then(|target| target.str())
                && let Some(library) = open_ipod(Path::new(mount_path), &context)
            {
                library.copy_all();
            }
        });
    }
    actions.add_action(&copy_all);

    // "Back Up…": the same backup that is offered when an iPod is opened, but
    // on request. The target is the mount path and the name of the iPod.
    let back_up = gio::SimpleAction::new("back-up", Some(&<(String, String)>::static_variant_type()));
    {
        let context = context.clone();
        back_up.connect_activate(move |_, target| {
            if let Some((mount_path, name)) = target.and_then(|target| target.get::<(String, String)>()) {
                start_backup(&context, PathBuf::from(mount_path), name, false);
            }
        });
    }
    actions.add_action(&back_up);

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
/// isn't possible.
fn open_ipod(mount_path: &Path, context: &Context) -> Option<Rc<Library>> {
    match Library::new(mount_path, &context.navigation) {
        Ok(library) => {
            context.navigation.push(library.page());
            context.open_library.replace(Some(library.clone()));
            Some(library)
        }
        Err(error) => {
            show_error(context, "Can't Read the Music Database", &error.to_string());
            None
        }
    }
}

/// The About window. Its "Report an Issue" button opens the project's issue
/// page, and its troubleshooting section has what a bug report should say.
fn show_about(window: &adw::ApplicationWindow) {
    let about = adw::AboutDialog::builder()
        .application_name("Oxipod")
        .application_icon(APP_ID)
        .developer_name("nyxn3t")
        .version(env!("CARGO_PKG_VERSION"))
        .comments("Put music on your old iPod")
        .website("https://github.com/nyxn3t/Oxipod")
        .issue_url("https://github.com/nyxn3t/Oxipod/issues")
        .license_type(gtk::License::Gpl30)
        .copyright("© 2026 nyxn3t")
        .debug_info(debug_info())
        .debug_info_filename("oxipod-troubleshooting.txt")
        .build();
    about.present(Some(window));
}

/// What a bug report should say about the app, the computer and the iPods
/// that are plugged in. No names or paths: people paste this into issues.
fn debug_info() -> String {
    let setting = |name: &str| std::env::var(name).unwrap_or_else(|_| "unknown".into());
    let mut lines = vec![
        format!(
            "Oxipod {}{}",
            env!("CARGO_PKG_VERSION"),
            // Inside a Flatpak sandbox this file always exists.
            if Path::new("/.flatpak-info").exists() { " (Flatpak)" } else { "" }
        ),
        format!(
            "GTK {}.{}.{}, libadwaita {}.{}.{}",
            gtk::major_version(),
            gtk::minor_version(),
            gtk::micro_version(),
            adw::major_version(),
            adw::minor_version(),
            adw::micro_version()
        ),
        format!("Desktop: {}, {}", setting("XDG_CURRENT_DESKTOP"), setting("XDG_SESSION_TYPE")),
    ];
    let ipods = device::find_ipods(&gio::VolumeMonitor::get());
    if ipods.is_empty() {
        lines.push("No iPod connected".into());
    }
    for (number, ipod) in ipods.iter().enumerate() {
        lines.push(format!("iPod {}: {}", number + 1, device::describe(ipod)));
    }
    lines.join("\n")
}

/// A message with a Close button.
fn show_error(context: &Context, heading: &str, message: &str) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(message));
    dialog.add_response("close", "Close");
    dialog.present(Some(&context.window));
}

/// What happens when an iPod is clicked in the list: first the offer of a
/// backup (unless the person said "don't ask again", or made one just now),
/// then the iPod opens.
fn open_ipod_after_offer(context: &Context, mount_path: PathBuf, name: String) {
    let no_offer = !Settings::load().ask_for_backup() || context.backed_up.borrow().contains(&mount_path);
    // The offer says how big the backup is. If that can't be worked out there
    // is nothing to back up either, and opening the iPod will show what's wrong.
    let size = backup::plan(&mount_path).map(|items| items.iter().map(|item| item.size).sum::<u64>());
    let size = match size {
        Ok(size) if !no_offer => size,
        _ => {
            open_ipod(&mount_path, context);
            return;
        }
    };

    let body = format!(
        "Oxipod changes the iPod's song database, a format Apple never documented. A backup of it \
         is cheap insurance. It holds the song list and the covers ({}), not the music.",
        glib::format_size(size)
    );
    let dialog = adw::AlertDialog::new(Some("Back Up Your iPod First?"), Some(&body));
    let dont_ask = gtk::CheckButton::builder().label("Don't ask again").halign(gtk::Align::Center).build();
    dialog.set_extra_child(Some(&dont_ask));
    dialog.add_responses(&[("skip", "Not Now"), ("backup", "Back Up…")]);
    dialog.set_response_appearance("backup", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("backup"));
    // Closing the dialog (Esc) is the same as "Not Now": it's a reminder, not a gate.
    dialog.set_close_response("skip");

    let context_for_response = context.clone();
    dialog.connect_response(None, move |_, response| {
        let context = &context_for_response;
        if dont_ask.is_active()
            && let Some(action) = context.window.lookup_action("ask-backup")
        {
            // Changing the state of the main menu's switch also saves it.
            action.change_state(&false.to_variant());
        }
        match response {
            "backup" => start_backup(context, mount_path.clone(), name.clone(), true),
            _ => {
                open_ipod(&mount_path, context);
            }
        }
    });
    dialog.present(Some(&context.window));
}

/// Rebuilds the device list and switches between the list and the empty page.
fn show_ipods(monitor: &gio::VolumeMonitor, list: &gtk::ListBox, stack: &gtk::Stack, context: &Context) {
    let ipods = device::find_ipods(monitor);

    // The open iPod was unplugged or ejected: go back to the device list.
    // The borrow ends with this statement, before pop_to_tag runs the popped
    // handler that borrows `open_library` again.
    let unplugged = context
        .open_library
        .borrow()
        .as_ref()
        .is_some_and(|library| !ipods.iter().any(|ipod| ipod.mount_path == library.mount_path()));
    if unplugged {
        context.navigation.pop_to_tag("root");
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
        let target = (ipod.mount_path.to_string_lossy().into_owned(), ipod.name.clone()).to_variant();
        let back_up = gio::MenuItem::new(Some("Back Up…"), None);
        back_up.set_action_and_target_value(Some("device.back-up"), Some(&target));
        menu.append_item(&back_up);
        let copy_all = gio::MenuItem::new(Some("Copy All to Computer…"), None);
        let path_target = ipod.mount_path.to_string_lossy().to_variant();
        copy_all.set_action_and_target_value(Some("device.copy-all"), Some(&path_target));
        menu.append_item(&copy_all);
        let rename = gio::MenuItem::new(Some("Rename…"), None);
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
        let (mount_path, name, context) = (ipod.mount_path.clone(), ipod.name.clone(), context.clone());
        row.connect_activated(move |_| open_ipod_after_offer(&context, mount_path.clone(), name.clone()));
        list.append(&row);
    }

    stack.set_visible_child_name(if ipods.is_empty() { "empty" } else { "devices" });
}

/// How a backup ended.
enum BackupResult {
    Done(backup::Done),
    /// The person changed their mind: closed the folder chooser, or pressed Cancel.
    Cancelled,
    Failed(String),
}

/// Asks where to save a backup of the iPod, makes it, and says how it went.
/// With `open_after` the iPod opens afterwards: the backup was offered
/// because the person was opening the iPod.
fn start_backup(context: &Context, mount_path: PathBuf, name: String, open_after: bool) {
    let context = context.clone();
    // A backup takes a while. This future keeps GTK responsive while it waits,
    // like renaming does.
    glib::spawn_future_local(async move {
        match make_backup(&context, &mount_path, &name).await {
            BackupResult::Done(done) => {
                context.backed_up.borrow_mut().insert(mount_path.clone());
                show_backup_done(&context, mount_path, done, open_after);
            }
            BackupResult::Cancelled => {}
            BackupResult::Failed(message) => show_error(&context, "Couldn't Back Up the iPod", &message),
        }
    });
}

async fn make_backup(context: &Context, mount_path: &Path, name: &str) -> BackupResult {
    let items = match backup::plan(mount_path) {
        Ok(items) => items,
        Err(error) => return BackupResult::Failed(error.to_string()),
    };
    let total: u64 = items.iter().map(|item| item.size).sum();

    // Where to put it. The backup gets a folder of its own inside the one chosen.
    let chooser = gtk::FileDialog::builder().title("Choose Where to Save the Backup").accept_label("Back Up").build();
    if let Some(documents) = glib::user_special_dir(glib::UserDirectory::Documents) {
        chooser.set_initial_folder(Some(&gio::File::for_path(documents)));
    }
    // An error here means the chooser was closed without choosing.
    let Ok(folder) = chooser.select_folder_future(Some(&context.window)).await else {
        return BackupResult::Cancelled;
    };
    let Some(destination) = folder.path() else {
        return BackupResult::Failed("That folder can't be used".into());
    };
    if let Some(free) = device::free_space(&destination)
        && free < total
    {
        return BackupResult::Failed(format!(
            "Not enough room there: the backup needs {}, the folder has {} free",
            glib::format_size(total),
            glib::format_size(free)
        ));
    }

    let now = glib::DateTime::now_local().ok();
    let when = |format: &str| now.as_ref().and_then(|now| now.format(format).ok()).map(String::from).unwrap_or_default();
    let folder_name = backup::folder_name(name, &when("%Y-%m-%d %H%M"));
    let note = backup::note(name, &when("%-d %B %Y, %H:%M"));

    // The progress window. It can't be closed until the backup is over, but it
    // has a Cancel button.
    let bar = gtk::ProgressBar::builder().show_text(true).build();
    let cancel_button = gtk::Button::builder().label("Cancel").halign(gtk::Align::Center).build();
    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(18).build();
    content.append(&bar);
    content.append(&cancel_button);
    let dialog = adw::AlertDialog::new(Some("Backing Up…"), Some("Keep the iPod plugged in until this is done."));
    dialog.set_extra_child(Some(&content));
    dialog.set_can_close(false);

    // The thread that copies and this code both look at these. An `Arc` is
    // like an `Rc` that may be shared with another thread, and the atomic
    // types can be changed from several threads at once.
    let copied = Arc::new(AtomicU64::new(0));
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let cancel = cancel.clone();
        cancel_button.connect_clicked(move |button| {
            cancel.store(true, Ordering::Relaxed);
            button.set_sensitive(false);
        });
    }
    dialog.present(Some(&context.window));

    // Keeps the bar up to date, ten times a second, until the copying is over.
    let finished = Rc::new(Cell::new(false));
    glib::spawn_future_local({
        let (copied, finished) = (copied.clone(), finished.clone());
        async move {
            while !finished.get() {
                let done = copied.load(Ordering::Relaxed);
                bar.set_fraction(if total > 0 { done as f64 / total as f64 } else { 0.0 });
                bar.set_text(Some(&format!("{} of {}", glib::format_size(done), glib::format_size(total))));
                glib::timeout_future(Duration::from_millis(100)).await;
            }
        }
    });

    // Everything the thread uses is moved into it.
    let (ipod, thread_copied, thread_cancel) = (mount_path.to_path_buf(), copied.clone(), cancel.clone());
    let result = gio::spawn_blocking(move || {
        backup::back_up(&ipod, &items, &destination, &folder_name, &note, &thread_copied, &thread_cancel)
    })
    .await;
    finished.set(true);
    dialog.set_can_close(true);
    dialog.force_close();

    match result {
        Ok(Ok(done)) => BackupResult::Done(done),
        Ok(Err(_)) if cancel.load(Ordering::Relaxed) => BackupResult::Cancelled,
        Ok(Err(error)) => BackupResult::Failed(error.to_string()),
        Err(_) => BackupResult::Failed("the backup crashed".into()),
    }
}

/// Says where the backup is. With `open_after`, the iPod opens when the
/// message is closed.
fn show_backup_done(context: &Context, mount_path: PathBuf, done: backup::Done, open_after: bool) {
    let folder_name = done.folder.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let files = if done.files == 1 { "1 file".to_string() } else { format!("{} files", done.files) };
    let body = format!("{files} ({}) saved in the folder “{folder_name}”.", glib::format_size(done.bytes));
    let dialog = adw::AlertDialog::new(Some("Backup Done"), Some(&body));
    dialog.add_responses(&[("show", "Show Files"), ("continue", if open_after { "Open iPod" } else { "Close" })]);
    dialog.set_response_appearance("continue", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("continue"));
    dialog.set_close_response("continue");

    let context_for_response = context.clone();
    dialog.connect_response(None, move |_, response| {
        let context = &context_for_response;
        if response == "show" {
            let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&done.folder)));
            let window = context.window.clone();
            glib::spawn_future_local(async move {
                if let Err(error) = launcher.open_containing_folder_future(Some(&window)).await {
                    eprintln!("Couldn't show the folder: {error}");
                }
            });
        }
        if open_after {
            open_ipod(&mount_path, context);
        }
    });
    dialog.present(Some(&context.window));
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
