//! The page for one iPod: its songs and playlists, adding and deleting songs,
//! editing playlists, and ejecting.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use gtk::gdk_pixbuf::Pixbuf;
use gtk::{gdk, gio, glib};

use crate::convert;
use crate::gpod::{self, Database, PlaylistId, Track, TrackId};

/// Shown at this size in the song list.
const COVER_SIZE: i32 = 40;

/// Everything on the page, and the open database.
///
/// The page is shared with GTK's signal handlers, so it lives in an `Rc`
/// (a reference-counted pointer: several owners, freed when the last one is
/// gone). Values that change are in a `Cell` (for small copyable values) or a
/// `RefCell` (for anything else); both allow changing them through a shared
/// reference, which Rust normally forbids.
pub struct Library {
    db: RefCell<Database>,
    mount_path: PathBuf,
    page: adw::NavigationPage,
    navigation: adw::NavigationView,
    actions: gio::SimpleActionGroup,
    toasts: adw::ToastOverlay,
    toolbar: adw::ToolbarView,
    add_button: gtk::Button,
    eject_button: gtk::Button,
    progress: gtk::ProgressBar,
    progress_label: gtk::Label,
    songs_group: adw::PreferencesGroup,
    songs: gtk::ListBox,
    playlists: gtk::ListBox,
    /// The playlist page that is open, if any, so it can be refreshed.
    open_playlist: RefCell<Option<OpenPlaylist>>,
    covers: RefCell<HashMap<TrackId, Option<gdk::Texture>>>,
    /// True while songs are being added; other changes wait until it's done.
    busy: Cell<bool>,
}

struct OpenPlaylist {
    id: PlaylistId,
    page: adw::NavigationPage,
    group: adw::PreferencesGroup,
    list: gtk::ListBox,
}

impl Library {
    pub fn new(mount_path: &Path, navigation: &adw::NavigationView) -> Result<Rc<Self>, gpod::Error> {
        let db = Database::open(mount_path)?;
        let name = db.name().unwrap_or_else(|| "iPod".into());

        let songs = boxed_list("No songs yet. Drop music files here, or use the + button.");
        let songs_group = adw::PreferencesGroup::new();
        songs_group.add(&songs);
        let songs_page = adw::PreferencesPage::new();
        songs_page.add(&songs_group);

        let playlists = boxed_list("No playlists yet");
        let new_playlist_button = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("New Playlist")
            .css_classes(["flat"])
            .build();
        let playlists_group = adw::PreferencesGroup::builder().title("Playlists").build();
        playlists_group.set_header_suffix(Some(&new_playlist_button));
        playlists_group.add(&playlists);
        let playlists_page = adw::PreferencesPage::new();
        playlists_page.add(&playlists_group);

        let tabs = adw::ViewStack::new();
        tabs.add_titled_with_icon(&songs_page, Some("songs"), "Songs", "folder-music-symbolic");
        tabs.add_titled_with_icon(&playlists_page, Some("playlists"), "Playlists", "view-list-symbolic");

        let add_button = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add Songs")
            .build();
        let eject_button = gtk::Button::builder()
            .icon_name("media-eject-symbolic")
            .tooltip_text("Eject")
            .build();
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(
            &adw::ViewSwitcher::builder()
                .stack(&tabs)
                .policy(adw::ViewSwitcherPolicy::Wide)
                .build(),
        ));
        header.pack_start(&add_button);
        header.pack_end(&eject_button);

        // Shown at the bottom while songs are being added.
        let progress_label = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .build();
        let progress = gtk::ProgressBar::new();
        let progress_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        progress_box.append(&progress_label);
        progress_box.append(&progress);

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&tabs));

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&toasts));
        toolbar.add_bottom_bar(&progress_box);
        toolbar.set_reveal_bottom_bars(false);

        let page = adw::NavigationPage::new(&toolbar, &name);
        let actions = gio::SimpleActionGroup::new();
        page.insert_action_group("library", Some(&actions));

        let library = Rc::new(Library {
            db: RefCell::new(db),
            mount_path: mount_path.to_path_buf(),
            page,
            navigation: navigation.clone(),
            actions,
            toasts,
            toolbar,
            add_button,
            eject_button,
            progress,
            progress_label,
            songs_group,
            songs,
            playlists,
            open_playlist: RefCell::new(None),
            covers: RefCell::new(HashMap::new()),
            busy: Cell::new(false),
        });

        library.connect_signals(&new_playlist_button);
        library.add_actions();
        library.refresh();
        Ok(library)
    }

    pub fn page(&self) -> &adw::NavigationPage {
        &self.page
    }

    pub fn mount_path(&self) -> &Path {
        &self.mount_path
    }

    /// Signal handlers hold a `Weak` reference: one that doesn't keep the
    /// library alive. Otherwise the page would own handlers that own the
    /// library that owns the page, a cycle that is never freed. `upgrade`
    /// turns it back into an `Rc`, or `None` if the library is already gone.
    fn connect_signals(self: &Rc<Self>, new_playlist_button: &gtk::Button) {
        let this = Rc::downgrade(self);
        self.add_button.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.choose_files();
            }
        });

        let this = Rc::downgrade(self);
        self.eject_button.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.eject();
            }
        });

        let this = Rc::downgrade(self);
        new_playlist_button.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.ask_playlist_name(None);
            }
        });

        let this = Rc::downgrade(self);
        self.playlists.connect_row_activated(move |_, row| {
            if let Some(this) = this.upgrade() {
                // Each playlist row's widget name holds the playlist id.
                if let Ok(id) = row.widget_name().parse() {
                    this.show_playlist(id);
                }
            }
        });

        // Dropping files or folders anywhere on the page adds them.
        let drop_target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let this = Rc::downgrade(self);
        drop_target.connect_drop(move |_, value, _, _| {
            let (Some(this), Ok(files)) = (this.upgrade(), value.get::<gdk::FileList>()) else {
                return false;
            };
            let paths = files.files().iter().filter_map(|file| file.path()).collect();
            glib::spawn_future_local(this.import(paths));
            true
        });
        self.toasts.add_controller(drop_target);
    }

    /// The actions behind the song and playlist menus. A menu item names an
    /// action ("library.delete-song") and a target value (the song's id).
    fn add_actions(self: &Rc<Self>) {
        self.add_action::<TrackId>("delete-song", |this, id| this.confirm_delete(id));
        self.add_action::<TrackId>("new-playlist-with-song", |this, id| {
            this.ask_playlist_name(Some(id));
        });
        self.add_action::<(PlaylistId, TrackId)>("add-to-playlist", |this, (playlist, id)| {
            this.change(|db| db.add_to_playlist(playlist, id));
        });
        self.add_action::<(PlaylistId, TrackId)>("remove-from-playlist", |this, (playlist, id)| {
            this.change(|db| db.remove_from_playlist(playlist, id));
        });
        self.add_action::<PlaylistId>("delete-playlist", |this, id| this.confirm_delete_playlist(id));
    }

    /// Adds an action whose parameter is a `T`, like a u64 id.
    ///
    /// `T: StaticVariantType + FromVariant` is a trait bound: it only accepts
    /// types that can be stored in a GLib Variant, which is how GTK passes
    /// menu targets around.
    fn add_action<T>(self: &Rc<Self>, name: &str, run: impl Fn(&Rc<Self>, T) + 'static)
    where
        T: glib::variant::StaticVariantType + glib::variant::FromVariant,
    {
        let action = gio::SimpleAction::new(name, Some(&T::static_variant_type()));
        let this = Rc::downgrade(self);
        action.connect_activate(move |_, parameter| {
            let value = parameter.and_then(T::from_variant);
            if let (Some(this), Some(value)) = (this.upgrade(), value) {
                run(&this, value);
            }
        });
        self.actions.add_action(&action);
    }

    /// Rebuilds every list from the database.
    fn refresh(&self) {
        let db = self.db.borrow();
        let mut tracks = db.tracks();
        tracks.sort_by_key(sort_key);
        let playlists = db.playlists();
        let info = db.device_info();
        drop(db); // done reading; song_row borrows the database again

        let total: Duration = tracks.iter().map(|track| track.length).sum();
        self.songs_group
            .set_title(&format!("{} Songs · {}", tracks.len(), format_duration(total)));
        let model = [info.generation, info.model_number].into_iter().flatten().collect::<Vec<_>>();
        self.songs_group.set_description(Some(&glib::markup_escape_text(&model.join(", "))));

        self.songs.remove_all();
        for track in &tracks {
            self.songs.append(&self.song_row(track, &playlists, None));
        }

        self.playlists.remove_all();
        for playlist in &playlists {
            let row = adw::ActionRow::builder()
                .title(&playlist.name)
                .subtitle(songs_count(playlist.track_ids.len()))
                .use_markup(false)
                .activatable(true)
                .build();
            row.set_widget_name(&playlist.id.to_string());
            row.add_prefix(&gtk::Image::from_icon_name("view-list-symbolic"));
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            self.playlists.append(&row);
        }

        // Refresh the open playlist page too, or close it if it was deleted.
        let mut playlist_deleted = false;
        if let Some(open) = self.open_playlist.borrow().as_ref() {
            match playlists.iter().find(|playlist| playlist.id == open.id) {
                Some(playlist) => {
                    open.page.set_title(&playlist.name);
                    open.group.set_title(&songs_count(playlist.track_ids.len()));
                    open.list.remove_all();
                    for id in &playlist.track_ids {
                        if let Some(track) = tracks.iter().find(|track| track.id == *id) {
                            open.list.append(&self.song_row(track, &playlists, Some(playlist.id)));
                        }
                    }
                }
                None => playlist_deleted = true,
            }
        }
        // Closing the page runs its "hidden" handler, which changes
        // open_playlist, so this has to wait until the borrow above has ended.
        if playlist_deleted {
            self.navigation.pop_to_page(&self.page);
        }
    }

    /// A song in a list, with its cover and a menu. `in_playlist` is set on
    /// a playlist page, to offer "Remove from Playlist".
    fn song_row(&self, track: &Track, playlists: &[gpod::Playlist], in_playlist: Option<PlaylistId>) -> adw::ActionRow {
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

        let cover = match self.cover(track.id) {
            Some(texture) => gtk::Image::from_paintable(Some(&texture)),
            None => gtk::Image::from_icon_name("audio-x-generic-symbolic"),
        };
        cover.set_pixel_size(COVER_SIZE);
        row.add_prefix(&cover);

        let length = gtk::Label::builder()
            .label(format_duration(track.length))
            .css_classes(["dim-label", "numeric"])
            .build();
        row.add_suffix(&length);

        let menu_button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .menu_model(&song_menu(track.id, playlists, in_playlist))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .tooltip_text("More")
            .build();
        row.add_suffix(&menu_button);
        row
    }

    /// The song's cover as a texture GTK can draw, loaded once and cached.
    fn cover(&self, id: TrackId) -> Option<gdk::Texture> {
        // `entry(...).or_insert_with(...)` looks the id up and only loads the
        // cover if it isn't in the cache yet.
        self.covers
            .borrow_mut()
            .entry(id)
            .or_insert_with(|| {
                // Twice the size, so it stays sharp on high-resolution screens.
                let pixbuf = self.db.borrow().cover(id, COVER_SIZE * 2)?;
                Some(texture_from_pixbuf(&pixbuf))
            })
            .clone()
    }

    /// Runs `edit` on the database, saves it to the iPod and refreshes the
    /// page. Errors are shown as a toast.
    fn change(&self, edit: impl FnOnce(&mut Database) -> Result<(), gpod::Error>) {
        if self.busy.get() {
            self.toast("Wait until the songs are added");
            return;
        }
        // The block limits how long the database is borrowed: refresh below
        // borrows it again, and RefCell panics on overlapping mutable borrows.
        let result = {
            let mut db = self.db.borrow_mut();
            edit(&mut db).and_then(|()| db.save())
        };
        self.refresh();
        if let Err(error) = result {
            self.toast(&format!("Couldn't save the changes: {error}"));
        }
    }

    fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(&glib::markup_escape_text(message)));
    }

    fn choose_files(self: &Rc<Self>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Music"));
        filter.add_mime_type("audio/*");
        for suffix in ["webm", "mkv", "mka", "mp4", "m4a", "opus"] {
            filter.add_suffix(suffix);
        }
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder()
            .title("Add Songs")
            .accept_label("Add")
            .filters(&filters)
            .build();

        let window = self.page.root().and_downcast::<gtk::Window>();
        let this = self.clone();
        glib::spawn_future_local(async move {
            // An error here usually means the dialog was cancelled.
            let Ok(files) = dialog.open_multiple_future(window.as_ref()).await else {
                return;
            };
            let paths = (0..files.n_items())
                .filter_map(|i| files.item(i).and_downcast::<gio::File>())
                .filter_map(|file| file.path())
                .collect();
            this.import(paths).await;
        });
    }

    /// Adds files (and the music in folders) to the iPod, one at a time.
    ///
    /// This is an `async fn`: it runs on the main thread, but every `.await`
    /// gives GTK a chance to redraw and handle clicks while ffmpeg converts
    /// or a file is copied on a background thread.
    async fn import(self: Rc<Self>, paths: Vec<PathBuf>) {
        let files = convert::music_files(paths);
        if files.is_empty() {
            self.toast("No music files found");
            return;
        }
        if self.busy.replace(true) {
            self.toast("Already adding songs");
            return;
        }
        self.add_button.set_sensitive(false);
        self.eject_button.set_sensitive(false);
        self.progress.set_fraction(0.0);
        self.toolbar.set_reveal_bottom_bars(true);

        let mut added = 0;
        let mut failed = Vec::new();
        for (i, file) in files.iter().enumerate() {
            let name = file.file_name().unwrap_or_default().to_string_lossy().into_owned();
            self.progress_label.set_text(&format!("Adding {} of {}: {name}", i + 1, files.len()));
            match self.import_file(file.clone()).await {
                Ok(()) => added += 1,
                Err(error) => failed.push(format!("{name}: {error}")),
            }
            self.progress.set_fraction((i + 1) as f64 / files.len() as f64);
        }

        self.progress_label.set_text("Saving…");
        let saved = self.db.borrow_mut().save();

        self.busy.set(false);
        self.add_button.set_sensitive(true);
        self.eject_button.set_sensitive(true);
        self.toolbar.set_reveal_bottom_bars(false);
        self.refresh();

        if let Err(error) = saved {
            self.toast(&format!("Couldn't save the music database: {error}"));
        } else if failed.is_empty() {
            self.toast(&format!("Added {}", songs_count(added)));
        } else {
            for failure in &failed {
                eprintln!("Couldn't add {failure}");
            }
            let first = &failed[0];
            let more = if failed.len() > 1 { format!(" (and {} more)", failed.len() - 1) } else { String::new() };
            self.toast(&format!("Added {}, couldn't add {first}{more}", songs_count(added)));
        }
    }

    async fn import_file(&self, file: PathBuf) -> Result<(), String> {
        // spawn_blocking runs the closure on a background thread. Everything
        // it uses is moved into it, so `file` can't be used here afterwards.
        // The outer Result is Err if the thread panicked.
        let prepared = gio::spawn_blocking(move || convert::prepare(&file))
            .await
            .map_err(|_| "the conversion crashed".to_string())?
            .map_err(|error| error.to_string())?;

        let pending = self
            .db
            .borrow_mut()
            .add_track(&prepared.track, &prepared.file)
            .map_err(|error| error.to_string())?;

        let (from, to) = (prepared.file.clone(), pending.destination.clone());
        let copied = gio::spawn_blocking(move || std::fs::copy(from, to)).await;

        let mut db = self.db.borrow_mut();
        let result = match copied {
            Ok(Ok(_)) => db.finish_copy(&pending).map_err(|error| error.to_string()),
            Ok(Err(error)) => Err(format!("copying failed: {error}")),
            Err(_) => Err("copying crashed".to_string()),
        };
        if result.is_err() {
            db.cancel_copy(&pending);
        }
        result
        // `prepared` is dropped here, which deletes the converted temp file.
    }

    fn confirm_delete(self: &Rc<Self>, id: TrackId) {
        let title = self
            .db
            .borrow()
            .tracks()
            .into_iter()
            .find(|track| track.id == id)
            .and_then(|track| track.title)
            .unwrap_or_else(|| "This song".into());
        let dialog = adw::AlertDialog::new(
            Some("Delete Song?"),
            Some(&format!("“{title}” will be removed from the iPod and from all its playlists.")),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        let this = Rc::downgrade(self);
        dialog.connect_response(Some("delete"), move |_, _| {
            if let Some(this) = this.upgrade() {
                this.change(|db| db.remove_track(id));
                this.covers.borrow_mut().remove(&id);
            }
        });
        dialog.present(Some(&self.page));
    }

    fn confirm_delete_playlist(self: &Rc<Self>, id: PlaylistId) {
        let name = self
            .db
            .borrow()
            .playlists()
            .into_iter()
            .find(|playlist| playlist.id == id)
            .map(|playlist| playlist.name)
            .unwrap_or_default();
        let dialog = adw::AlertDialog::new(
            Some("Delete Playlist?"),
            Some(&format!("“{name}” will be deleted. The songs in it stay on the iPod.")),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        let this = Rc::downgrade(self);
        dialog.connect_response(Some("delete"), move |_, _| {
            if let Some(this) = this.upgrade() {
                this.change(|db| db.remove_playlist(id));
            }
        });
        dialog.present(Some(&self.page));
    }

    /// Asks for a name and creates a playlist, with `first_song` in it if set.
    fn ask_playlist_name(self: &Rc<Self>, first_song: Option<TrackId>) {
        // activates_default: pressing Enter picks the default response, "Create".
        let entry = adw::EntryRow::builder().title("Name").activates_default(true).build();
        let list = boxed_list("");
        list.append(&entry);

        let dialog = adw::AlertDialog::new(Some("New Playlist"), None);
        dialog.set_extra_child(Some(&list));
        dialog.add_responses(&[("cancel", "Cancel"), ("create", "Create")]);
        dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
        dialog.set_response_enabled("create", false);
        dialog.set_default_response(Some("create"));
        dialog.set_close_response("cancel");

        // Only allow "Create" when a name was typed.
        let dialog_ref = dialog.downgrade();
        entry.connect_changed(move |entry| {
            if let Some(dialog) = dialog_ref.upgrade() {
                dialog.set_response_enabled("create", !entry.text().trim().is_empty());
            }
        });
        let this = Rc::downgrade(self);
        let name_entry = entry.clone();
        dialog.connect_response(Some("create"), move |_, _| {
            let Some(this) = this.upgrade() else {
                return;
            };
            let name = name_entry.text().trim().to_string();
            this.change(|db| {
                let playlist = db.create_playlist(&name)?;
                match first_song {
                    Some(song) => db.add_to_playlist(playlist, song),
                    None => Ok(()),
                }
            });
        });
        dialog.present(Some(&self.page));
        entry.grab_focus();
    }

    /// Opens a page with the songs in a playlist.
    fn show_playlist(self: &Rc<Self>, id: PlaylistId) {
        let list = boxed_list("This playlist is empty. Add songs with their ⋮ menu.");
        let group = adw::PreferencesGroup::new();
        group.add(&list);
        let content = adw::PreferencesPage::new();
        content.add(&group);

        let delete_button = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Delete Playlist")
            .action_name("library.delete-playlist")
            .action_target(&id.to_variant())
            .build();
        let header = adw::HeaderBar::new();
        header.pack_end(&delete_button);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&content));

        let page = adw::NavigationPage::new(&toolbar, "Playlist");
        // The song menus on this page use the same actions as the main page.
        page.insert_action_group("library", Some(&self.actions));

        let this: Weak<Self> = Rc::downgrade(self);
        page.connect_hidden(move |_| {
            if let Some(this) = this.upgrade() {
                this.open_playlist.replace(None);
            }
        });

        self.open_playlist.replace(Some(OpenPlaylist { id, page: page.clone(), group, list }));
        self.refresh();
        self.navigation.push(&page);
    }

    fn eject(self: &Rc<Self>) {
        let file = gio::File::for_path(&self.mount_path);
        let this = self.clone();
        glib::spawn_future_local(async move {
            let result = async {
                // Looking up the mount is quick (gio already knows it); the
                // eject itself can take a few seconds, so that one is async.
                let mount = file.find_enclosing_mount(gio::Cancellable::NONE)?;
                mount
                    .eject_with_operation_future(gio::MountUnmountFlags::NONE, None::<&gio::MountOperation>)
                    .await
            }
            .await;
            // On success the iPod disappears and the window goes back to the
            // device list by itself.
            if let Err(error) = result {
                this.toast(&format!("Couldn't eject: {}", error.message()));
            }
        });
    }
}

/// The ⋮ menu of a song.
fn song_menu(id: TrackId, playlists: &[gpod::Playlist], in_playlist: Option<PlaylistId>) -> gio::Menu {
    let add_to = gio::Menu::new();
    for playlist in playlists.iter().filter(|playlist| Some(playlist.id) != in_playlist) {
        let item = gio::MenuItem::new(Some(&playlist.name), None);
        item.set_action_and_target_value(Some("library.add-to-playlist"), Some(&(playlist.id, id).to_variant()));
        add_to.append_item(&item);
    }
    let new_item = gio::MenuItem::new(Some("New Playlist…"), None);
    new_item.set_action_and_target_value(Some("library.new-playlist-with-song"), Some(&id.to_variant()));
    add_to.append_item(&new_item);

    let menu = gio::Menu::new();
    menu.append_submenu(Some("Add to Playlist"), &add_to);
    if let Some(playlist) = in_playlist {
        let item = gio::MenuItem::new(Some("Remove from Playlist"), None);
        item.set_action_and_target_value(Some("library.remove-from-playlist"), Some(&(playlist, id).to_variant()));
        menu.append_item(&item);
    }

    let delete = gio::Menu::new();
    let item = gio::MenuItem::new(Some("Delete from iPod…"), None);
    item.set_action_and_target_value(Some("library.delete-song"), Some(&id.to_variant()));
    delete.append_item(&item);
    menu.append_section(None, &delete);
    menu
}

fn boxed_list(placeholder: &str) -> gtk::ListBox {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    if !placeholder.is_empty() {
        let label = gtk::Label::builder()
            .label(placeholder)
            .wrap(true)
            .margin_top(18)
            .margin_bottom(18)
            .margin_start(12)
            .margin_end(12)
            .css_classes(["dim-label"])
            .build();
        list.set_placeholder(Some(&label));
    }
    list
}

/// Songs sorted by title, ignoring case, like the iPod does.
fn sort_key(track: &Track) -> String {
    track.title.as_deref().unwrap_or_default().to_lowercase()
}

fn songs_count(count: usize) -> String {
    if count == 1 { "1 song".into() } else { format!("{count} songs") }
}

fn texture_from_pixbuf(pixbuf: &Pixbuf) -> gdk::Texture {
    let format = if pixbuf.has_alpha() { gdk::MemoryFormat::R8g8b8a8 } else { gdk::MemoryFormat::R8g8b8 };
    gdk::MemoryTexture::new(
        pixbuf.width(),
        pixbuf.height(),
        format,
        &pixbuf.read_pixel_bytes(),
        pixbuf.rowstride() as usize,
    )
    .upcast()
}

/// Formats a length as "3:05", or "1:02:07" when it's an hour or longer.
pub fn format_duration(length: Duration) -> String {
    let seconds = length.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}
