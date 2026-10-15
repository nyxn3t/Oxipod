//! The page for one iPod: its songs and playlists, adding and deleting songs,
//! editing playlists, and ejecting.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::gdk_pixbuf::Pixbuf;
use gtk::{gdk, gio, glib};

use crate::convert;
use crate::eject;
use crate::gpod::{self, Database, PlaylistId, Track, TrackId};
use crate::songs::{SongList, SortMode};

/// Shown at this size in the song list.
pub const COVER_SIZE: i32 = 40;

/// Space kept free on the iPod for the music and cover art databases, which
/// grow with every song.
const RESERVED_SPACE: u64 = 20 * 1024 * 1024;

/// While adding songs, the database is saved at least this often, so that
/// unplugging the iPod halfway loses at most a few songs.
const SAVE_INTERVAL: Duration = Duration::from_secs(10);

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
    /// Switches the bottom bar between the progress and the selection bar.
    bottom_bar: gtk::Stack,
    select_button: gtk::ToggleButton,
    search_button: gtk::ToggleButton,
    search_entry: gtk::SearchEntry,
    sort_button: gtk::MenuButton,
    selection_label: gtk::Label,
    select_all_button: gtk::Button,
    selection_add_button: gtk::MenuButton,
    selection_delete_button: gtk::Button,
    /// Offers to delete files the database doesn't know about.
    leftovers_banner: adw::Banner,
    /// "35 Songs · 2h 5m" and the iPod's details, above the song list.
    songs_title: gtk::Label,
    songs_details: gtk::Label,
    /// Switches between the song list and the "nothing here" pages.
    songs_stack: gtk::Stack,
    no_results: adw::StatusPage,
    /// The songs, their order, the search and the ticks (see `songs.rs`).
    songs: SongList,
    playlists: gtk::ListBox,
    /// The playlist page that is open, if any, so it can be refreshed.
    open_playlist: RefCell<Option<OpenPlaylist>>,
    covers: RefCell<HashMap<TrackId, Option<gdk::Texture>>>,
    /// True while songs are being added; other changes wait until it's done.
    busy: Cell<bool>,
}

/// Why adding a song didn't work.
enum ImportError {
    /// Only this song failed; carry on with the next one.
    Song(String),
    /// Adding more songs is pointless (the iPod is full or gone).
    Stop(String),
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

        let select_button = gtk::ToggleButton::builder()
            .icon_name("selection-mode-symbolic")
            .tooltip_text("Select Songs")
            .build();
        let search_button = gtk::ToggleButton::builder()
            .icon_name("edit-find-symbolic")
            .tooltip_text("Search (Ctrl+F)")
            .build();

        // The search bar slides in below the header when the search button is pressed.
        let search_entry = gtk::SearchEntry::builder().placeholder_text("Search songs").build();
        let search_bar = gtk::SearchBar::new();
        search_bar.set_child(Some(&adw::Clamp::builder().maximum_size(600).child(&search_entry).build()));
        search_bar.connect_entry(&search_entry);
        // The button and the bar follow each other: pressing the button shows
        // the bar, and closing the bar (Esc) un-presses the button.
        search_button
            .bind_property("active", &search_bar, "search-mode-enabled")
            .bidirectional()
            .sync_create()
            .build();

        // Above the list: how many songs, the iPod's details, and how to sort.
        let songs_title = gtk::Label::builder().xalign(0.0).css_classes(["heading"]).build();
        let songs_details = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["dim-label"]).build();
        let heading_text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        heading_text.append(&songs_title);
        heading_text.append(&songs_details);
        let sort_button = gtk::MenuButton::builder()
            .menu_model(&sort_menu())
            .label(sort_label(SortMode::default()))
            .always_show_arrow(true)
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let heading = gtk::Box::builder().spacing(12).build();
        heading.append(&heading_text);
        heading.append(&sort_button);

        // The list itself is added to this stack once it exists (see below).
        let songs_stack = gtk::Stack::builder().vexpand(true).build();
        let empty_page = adw::StatusPage::builder()
            .icon_name("folder-music-symbolic")
            .title("No Songs Yet")
            .description("Drop music files here, or use the + button.")
            .build();
        let no_results = adw::StatusPage::builder().icon_name("edit-find-symbolic").title("No Results").build();
        songs_stack.add_named(&empty_page, Some("empty"));
        songs_stack.add_named(&no_results, Some("no-results"));

        let songs_column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .vexpand(true)
            .build();
        songs_column.append(&heading);
        songs_column.append(&songs_stack);
        let songs_page = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        songs_page.append(&search_bar);
        songs_page.append(
            &adw::Clamp::builder()
                .maximum_size(600)
                .tightening_threshold(400)
                .vexpand(true)
                .child(&songs_column)
                .build(),
        );

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
        // pack_end fills from the right: eject on the far right, then select, then search.
        header.pack_end(&eject_button);
        header.pack_end(&select_button);
        header.pack_end(&search_button);

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

        // Shown in selection mode.
        let selection_label = gtk::Label::new(None);
        let select_all_button = gtk::Button::with_label("Select All");
        let selection_add_button = gtk::MenuButton::builder().label("Add to Playlist").build();
        let selection_delete_button = gtk::Button::builder()
            .label("Delete")
            .css_classes(["destructive-action"])
            .build();
        let selection_bar = gtk::ActionBar::new();
        selection_bar.pack_start(&select_all_button);
        selection_bar.set_center_widget(Some(&selection_label));
        selection_bar.pack_end(&selection_delete_button);
        selection_bar.pack_end(&selection_add_button);

        let bottom_bar = gtk::Stack::new();
        bottom_bar.add_named(&progress_box, Some("progress"));
        bottom_bar.add_named(&selection_bar, Some("selection"));

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&tabs));

        let leftovers_banner = adw::Banner::builder().button_label("Clean Up…").build();

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.add_top_bar(&leftovers_banner);
        toolbar.set_content(Some(&toasts));
        toolbar.add_bottom_bar(&bottom_bar);
        toolbar.set_reveal_bottom_bars(false);

        let page = adw::NavigationPage::new(&toolbar, &name);
        let actions = gio::SimpleActionGroup::new();
        page.insert_action_group("library", Some(&actions));

        // new_cyclic hands us a Weak reference to the Rc before it exists, so
        // things built along with the library (the song list) can keep a
        // reference back to it. A Weak one, because a strong one would keep
        // the library alive forever.
        let library = Rc::new_cyclic(|this: &Weak<Library>| {
            // The song list asks the library for covers and menus whenever a
            // row needs one, so it gets (weak) references back to it.
            let load_cover = {
                let this = this.clone();
                move |id| this.upgrade().and_then(|library| library.cover(id))
            };
            let song_menu = {
                let this = this.clone();
                move |id| match this.upgrade() {
                    Some(library) => song_menu(id, &library.db.borrow().playlists(), None),
                    None => gio::Menu::new(),
                }
            };
            let songs = SongList::new(&select_button, load_cover, song_menu);
            songs_stack.add_named(songs.widget(), Some("list"));

            Library {
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
                bottom_bar,
                select_button,
                search_button,
                search_entry,
                sort_button,
                selection_label,
                select_all_button,
                selection_add_button,
                selection_delete_button,
                leftovers_banner,
                songs_title,
                songs_details,
                songs_stack,
                no_results,
                songs,
                playlists,
                open_playlist: RefCell::new(None),
                covers: RefCell::new(HashMap::new()),
                busy: Cell::new(false),
            }
        });

        library.connect_signals(&new_playlist_button, &tabs);
        library.add_actions();
        library.refresh();
        library.check_leftovers();
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
    fn connect_signals(self: &Rc<Self>, new_playlist_button: &gtk::Button, tabs: &adw::ViewStack) {
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
        self.leftovers_banner.connect_button_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.confirm_delete_leftovers();
            }
        });

        let this = Rc::downgrade(self);
        new_playlist_button.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.ask_playlist_name(Vec::new());
            }
        });

        let this = Rc::downgrade(self);
        self.select_button.connect_toggled(move |button| {
            if let Some(this) = this.upgrade() {
                this.set_selecting(button.is_active());
            }
        });

        let this = Rc::downgrade(self);
        self.select_all_button.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.toggle_select_all();
            }
        });

        let this = Rc::downgrade(self);
        self.selection_delete_button.connect_clicked(move |_| {
            if let Some(this) = this.upgrade() {
                this.confirm_delete(this.selected_ids());
            }
        });

        // Typing in the search box filters the list. Closing the search (Esc,
        // or the button) empties the box, which shows every song again.
        let this = Rc::downgrade(self);
        self.search_entry.connect_search_changed(move |entry| {
            if let Some(this) = this.upgrade() {
                this.songs.set_query(&entry.text());
            }
        });
        let entry = self.search_entry.clone();
        self.search_button.connect_toggled(move |button| {
            if !button.is_active() {
                entry.set_text("");
            }
        });

        // The song list tells us when its ticks or its shown songs changed.
        let this = Rc::downgrade(self);
        self.songs.connect_selection_changed(move || {
            if let Some(this) = this.upgrade() {
                this.update_selection_bar();
            }
        });
        let this = Rc::downgrade(self);
        self.songs.connect_shown_changed(move || {
            if let Some(this) = this.upgrade() {
                this.update_songs_view();
                this.update_selection_bar();
            }
        });

        // Selecting and searching are only for the song list: leave selection
        // mode on the Playlists tab, and only show the buttons on the Songs tab.
        let this = Rc::downgrade(self);
        tabs.connect_visible_child_name_notify(move |tabs| {
            if let Some(this) = this.upgrade() {
                let on_songs = tabs.visible_child_name().as_deref() == Some("songs");
                this.select_button.set_visible(on_songs);
                this.search_button.set_visible(on_songs);
                if !on_songs {
                    this.select_button.set_active(false);
                }
            }
        });

        // Esc leaves selection mode, Ctrl+A selects every shown song, Ctrl+F starts a search.
        let shortcuts = gtk::ShortcutController::new();
        let this = Rc::downgrade(self);
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("Escape"),
            Some(gtk::CallbackAction::new(move |_, _| match this.upgrade() {
                Some(this) if this.select_button.is_active() => {
                    this.select_button.set_active(false);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            })),
        ));
        let this = Rc::downgrade(self);
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control>a"),
            Some(gtk::CallbackAction::new(move |_, _| match this.upgrade() {
                Some(this) if this.select_button.is_visible() && !this.busy.get() => {
                    this.select_button.set_active(true);
                    this.songs.select_shown(true);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            })),
        ));
        let this = Rc::downgrade(self);
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control>f"),
            Some(gtk::CallbackAction::new(move |_, _| match this.upgrade() {
                Some(this) if this.search_button.is_visible() => {
                    this.search_button.set_active(true);
                    this.search_entry.grab_focus();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            })),
        ));
        self.toolbar.add_controller(shortcuts);

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
        self.add_action::<TrackId>("delete-song", |this, id| this.confirm_delete(vec![id]));
        self.add_action::<TrackId>("new-playlist-with-song", |this, id| {
            this.ask_playlist_name(vec![id]);
        });
        self.add_action::<PlaylistId>("add-selected-to-playlist", |this, playlist| {
            let ids = this.selected_ids();
            let name = this.playlist_name(playlist);
            this.select_button.set_active(false);
            if this.change(|db| ids.iter().try_for_each(|&id| db.add_to_playlist(playlist, id))) {
                this.toast(&format!("Added {} to “{name}”", songs_count(ids.len())));
            }
        });
        // `()` as the parameter type: the action takes no value.
        self.add_action::<()>("new-playlist-with-selected", |this, ()| {
            this.ask_playlist_name(this.selected_ids());
        });
        self.add_action::<(PlaylistId, TrackId)>("add-to-playlist", |this, (playlist, id)| {
            this.change(|db| db.add_to_playlist(playlist, id));
        });
        self.add_action::<(PlaylistId, TrackId)>("remove-from-playlist", |this, (playlist, id)| {
            this.change(|db| db.remove_from_playlist(playlist, id));
        });
        self.add_action::<PlaylistId>("delete-playlist", |this, id| this.confirm_delete_playlist(id));
        self.add_sort_action();
    }

    /// The "Sort by" menu: an action that *remembers* which item is picked.
    /// A stateful action keeps a value (here the sort mode's name), and the
    /// menu draws a radio button for the item whose target equals it.
    fn add_sort_action(self: &Rc<Self>) {
        let initial = SortMode::default().id().to_variant();
        let action = gio::SimpleAction::new_stateful("sort", Some(glib::VariantTy::STRING), &initial);
        let this = Rc::downgrade(self);
        // For a stateful action whose value has the same type as its target,
        // activating a menu item asks to change the state to that target.
        action.connect_change_state(move |action, state| {
            let Some(state) = state else {
                return;
            };
            let (Some(this), Some(mode)) = (this.upgrade(), state.str().and_then(SortMode::from_id)) else {
                return;
            };
            action.set_state(state);
            this.songs.set_sort(mode);
            this.sort_button.set_label(&sort_label(mode));
        });
        self.actions.add_action(&action);
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
        // `()` means no parameter at all, which GTK writes as None.
        let no_parameter = T::static_variant_type() == <()>::static_variant_type();
        let parameter_type = (!no_parameter).then(|| T::static_variant_type());
        let action = gio::SimpleAction::new(name, parameter_type.as_deref());
        let this = Rc::downgrade(self);
        action.connect_activate(move |_, parameter| {
            let value = match parameter {
                Some(parameter) => T::from_variant(parameter),
                None => T::from_variant(&().to_variant()),
            };
            if let (Some(this), Some(value)) = (this.upgrade(), value) {
                run(&this, value);
            }
        });
        self.actions.add_action(&action);
    }

    /// Brings every list up to date with the database.
    fn refresh(&self) {
        let db = self.db.borrow();
        let tracks = with_test_songs(db.tracks());
        let playlists = db.playlists();
        let info = db.device_info();
        drop(db); // done reading; the song list asks for covers and menus, which borrow the database again

        let free = self.free_space().map(|bytes| format!("{} free", glib::format_size(bytes)));
        let details = [info.generation, info.model_number, free].into_iter().flatten().collect::<Vec<_>>();
        self.songs_details.set_text(&details.join(" · "));

        // The song list keeps the songs it already has (and their ticks and
        // covers), removes the ones that are gone and adds the new ones. Its
        // own signals come a moment later, so update what depends on it now.
        self.songs.set_tracks(&tracks);
        self.update_songs_view();
        self.update_selection_bar();
        self.selection_add_button.set_menu_model(Some(&selection_menu(&playlists)));

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

    /// A song on a playlist page, with its cover and a menu. `in_playlist` is
    /// set there, to offer "Remove from Playlist". (The main song list builds
    /// its rows in `songs.rs`.)
    fn song_row(
        &self,
        track: &Track,
        playlists: &[gpod::Playlist],
        in_playlist: Option<PlaylistId>,
    ) -> adw::ActionRow {
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

    /// Turns selection mode on or off (from the select button).
    fn set_selecting(&self, on: bool) {
        if on && self.busy.get() {
            self.select_button.set_active(false);
            return;
        }
        // Start (and end) with nothing ticked. The rows follow the select
        // button by themselves: check boxes instead of ⋮ menus (see `songs.rs`).
        self.songs.clear_selection();
        self.show_selection_mode(on);
    }

    /// Shows or hides the selection bar.
    fn show_selection_mode(&self, on: bool) {
        self.add_button.set_sensitive(!on && !self.busy.get());
        if !self.busy.get() {
            self.bottom_bar.set_visible_child_name("selection");
            self.toolbar.set_reveal_bottom_bars(on);
        }
        self.update_selection_bar();
    }

    fn update_selection_bar(&self) {
        let stats = self.songs.stats();
        // Songs ticked earlier can be hidden by a newer search; say so, because
        // deleting would include them.
        let hidden = stats.selected - stats.shown_selected;
        self.selection_label.set_text(&match (stats.selected, hidden) {
            (0, _) => "Select songs".to_string(),
            (count, 0) => format!("{} selected", songs_count(count)),
            (count, hidden) => format!("{} selected, {hidden} hidden by the search", songs_count(count)),
        });
        self.selection_add_button.set_sensitive(stats.selected > 0);
        self.selection_delete_button.set_sensitive(stats.selected > 0);
        let all_ticked = stats.shown > 0 && stats.shown_selected == stats.shown;
        self.select_all_button.set_label(if all_ticked { "Select None" } else { "Select All" });
        self.select_all_button.set_sensitive(stats.shown > 0);
    }

    /// Ticks every shown song, or unticks them when they're all ticked already.
    fn toggle_select_all(&self) {
        let stats = self.songs.stats();
        let all_ticked = stats.shown > 0 && stats.shown_selected == stats.shown;
        self.songs.select_shown(!all_ticked);
    }

    /// The "35 Songs · 2h 5m" line above the list, and whether the list or one
    /// of the "nothing here" pages is showing.
    fn update_songs_view(&self) {
        let stats = self.songs.stats();
        let songs = |count: usize| if count == 1 { "1 Song".to_string() } else { format!("{count} Songs") };
        let length = format_duration(stats.shown_length);
        self.songs_title.set_text(&if stats.shown == stats.total {
            format!("{} · {length}", songs(stats.total))
        } else {
            format!("{} of {} · {length}", stats.shown, songs(stats.total))
        });

        let page = if stats.total == 0 {
            "empty"
        } else if stats.shown == 0 {
            "no-results"
        } else {
            "list"
        };
        if page == "no-results" {
            // The description of a status page is markup, so escape the text.
            let query = glib::markup_escape_text(self.search_entry.text().trim());
            self.no_results.set_description(Some(&format!("No songs match “{query}”")));
        }
        self.songs_stack.set_visible_child_name(page);
    }

    /// The ticked songs, in the order the list shows them.
    fn selected_ids(&self) -> Vec<TrackId> {
        self.songs.selected_ids()
    }

    fn playlist_name(&self, id: PlaylistId) -> String {
        self.db
            .borrow()
            .playlists()
            .into_iter()
            .find(|playlist| playlist.id == id)
            .map(|playlist| playlist.name)
            .unwrap_or_default()
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
    /// page. Errors are shown as a toast. Returns whether it worked.
    fn change(&self, edit: impl FnOnce(&mut Database) -> Result<(), gpod::Error>) -> bool {
        if self.busy.get() {
            self.toast("Wait until the songs are added");
            return false;
        }
        // The block limits how long the database is borrowed: refresh below
        // borrows it again, and RefCell panics on overlapping mutable borrows.
        let result = {
            let mut db = self.db.borrow_mut();
            edit(&mut db).and_then(|()| db.save())
        };
        self.refresh();
        if let Err(error) = &result {
            self.toast(&format!("Couldn't save the changes: {error}"));
        }
        result.is_ok()
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
        self.select_button.set_active(false);
        self.select_button.set_sensitive(false);
        self.add_button.set_sensitive(false);
        self.eject_button.set_sensitive(false);
        self.leftovers_banner.set_revealed(false);
        self.progress.set_fraction(0.0);
        self.bottom_bar.set_visible_child_name("progress");
        self.toolbar.set_reveal_bottom_bars(true);

        let mut added = 0;
        let mut failed = Vec::new();
        let mut stopped = None;
        let mut save_error = None;
        let mut last_save = Instant::now();
        for (i, file) in files.iter().enumerate() {
            let name = file.file_name().unwrap_or_default().to_string_lossy().into_owned();
            self.progress_label.set_text(&format!("Adding {} of {}: {name}", i + 1, files.len()));
            match self.import_file(file.clone()).await {
                Ok(()) => added += 1,
                Err(ImportError::Song(error)) => failed.push(format!("{name}: {error}")),
                Err(ImportError::Stop(reason)) => {
                    stopped = Some(reason);
                    break;
                }
            }
            self.progress.set_fraction((i + 1) as f64 / files.len() as f64);

            if last_save.elapsed() >= SAVE_INTERVAL {
                self.progress_label.set_text("Saving…");
                if let Err(error) = self.db.borrow_mut().save() {
                    save_error = Some(error);
                    break;
                }
                last_save = Instant::now();
            }
        }

        // Save at the end, unless a save already failed (that error is the
        // one worth showing) or the iPod is gone (saving would fail anyway).
        if save_error.is_none() && self.is_connected() {
            self.progress_label.set_text("Saving…");
            save_error = self.db.borrow_mut().save().err();
        }

        self.busy.set(false);
        self.select_button.set_sensitive(true);
        self.add_button.set_sensitive(true);
        self.eject_button.set_sensitive(true);
        self.toolbar.set_reveal_bottom_bars(false);
        self.refresh();
        self.check_leftovers();

        for failure in &failed {
            eprintln!("Couldn't add {failure}");
        }
        let message = if let Some(reason) = stopped {
            format!("Added {}. {reason}", songs_count(added))
        } else if let Some(error) = save_error {
            format!("Couldn't save the music database: {error}")
        } else if let Some(first) = failed.first() {
            let more = if failed.len() > 1 { format!(" (and {} more)", failed.len() - 1) } else { String::new() };
            format!("Added {}, couldn't add {first}{more}", songs_count(added))
        } else {
            format!("Added {}", songs_count(added))
        };
        self.toast(&message);
    }

    async fn import_file(&self, file: PathBuf) -> Result<(), ImportError> {
        if !self.is_connected() {
            return Err(ImportError::Stop("The iPod was disconnected.".into()));
        }

        // spawn_blocking runs the closure on a background thread. Everything
        // it uses is moved into it, so `file` can't be used here afterwards.
        // The outer Result is Err if the thread panicked.
        let prepared = gio::spawn_blocking(move || convert::prepare(&file))
            .await
            .map_err(|_| ImportError::Song("the conversion crashed".into()))?
            .map_err(|error| ImportError::Song(error.to_string()))?;

        let size = std::fs::metadata(&prepared.file).map(|m| m.len()).unwrap_or(0);
        if let Some(free) = self.free_space()
            && free < size + RESERVED_SPACE
        {
            return Err(ImportError::Stop("The iPod is full.".into()));
        }

        let pending = self
            .db
            .borrow_mut()
            .add_track(&prepared.track, &prepared.file)
            .map_err(|error| ImportError::Song(error.to_string()))?;

        let (from, to) = (prepared.file.clone(), pending.destination.clone());
        let copied = gio::spawn_blocking(move || std::fs::copy(from, to)).await;

        let mut db = self.db.borrow_mut();
        let result = match copied {
            Ok(Ok(_)) => db.finish_copy(&pending).map_err(|error| ImportError::Song(error.to_string())),
            Ok(Err(error)) => Err(ImportError::Song(format!("copying failed: {error}"))),
            Err(_) => Err(ImportError::Song("copying crashed".into())),
        };
        if result.is_err() {
            db.cancel_copy(&pending);
            // A failed copy is often the iPod being unplugged.
            if !self.is_connected() {
                return Err(ImportError::Stop("The iPod was disconnected.".into()));
            }
        }
        result
        // `prepared` is dropped here, which deletes the converted temp file.
    }

    /// Whether the iPod is still there. When it's unplugged, the mount
    /// point disappears or becomes an empty folder.
    fn is_connected(&self) -> bool {
        self.mount_path.join("iPod_Control").is_dir()
    }

    /// Free space on the iPod in bytes, if it can be read.
    fn free_space(&self) -> Option<u64> {
        let info = gio::File::for_path(&self.mount_path)
            .query_filesystem_info(gio::FILE_ATTRIBUTE_FILESYSTEM_FREE, gio::Cancellable::NONE)
            .ok()?;
        Some(info.attribute_uint64(gio::FILE_ATTRIBUTE_FILESYSTEM_FREE))
    }

    /// Shows the banner if the iPod has song files the database doesn't know.
    fn check_leftovers(&self) {
        let leftovers = self.db.borrow().orphaned_files();
        if leftovers.is_empty() {
            self.leftovers_banner.set_revealed(false);
            return;
        }
        let size: u64 = leftovers.iter().map(|(_, size)| size).sum();
        self.leftovers_banner.set_title(&format!(
            "{} on the iPod aren't in its music database ({})",
            files_count(leftovers.len()),
            glib::format_size(size)
        ));
        self.leftovers_banner.set_revealed(true);
    }

    fn confirm_delete_leftovers(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        let leftovers = self.db.borrow().orphaned_files();
        let size: u64 = leftovers.iter().map(|(_, size)| size).sum();
        let dialog = adw::AlertDialog::new(
            Some("Delete Leftover Files?"),
            Some(&format!(
                "{} ({}) are on the iPod but not in its music database, so the iPod can't play \
                 them. They're usually left over from a sync that was interrupted.",
                files_count(leftovers.len()),
                glib::format_size(size)
            )),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        let this = Rc::downgrade(self);
        dialog.connect_response(Some("delete"), move |_, _| {
            let Some(this) = this.upgrade() else {
                return;
            };
            // Look again: only delete what is still unknown right now.
            let leftovers = this.db.borrow().orphaned_files();
            let mut freed = 0;
            let mut failed = 0;
            for (path, size) in &leftovers {
                match std::fs::remove_file(path) {
                    Ok(()) => freed += size,
                    Err(error) => {
                        eprintln!("Couldn't delete {}: {error}", path.display());
                        failed += 1;
                    }
                }
            }
            this.refresh();
            this.check_leftovers();
            if failed == 0 {
                this.toast(&format!("Freed {}", glib::format_size(freed)));
            } else {
                this.toast(&format!("Freed {}, couldn't delete {}", glib::format_size(freed), files_count(failed)));
            }
        });
        dialog.present(Some(&self.page));
    }

    fn confirm_delete(self: &Rc<Self>, ids: Vec<TrackId>) {
        let (heading, body) = match ids.as_slice() {
            [] => return,
            [id] => {
                let title = self
                    .db
                    .borrow()
                    .tracks()
                    .into_iter()
                    .find(|track| track.id == *id)
                    .and_then(|track| track.title)
                    .unwrap_or_else(|| "This song".into());
                (
                    "Delete Song?".to_string(),
                    format!("“{title}” will be removed from the iPod and from all its playlists."),
                )
            }
            _ => (
                format!("Delete {}?", songs_count(ids.len())),
                "These songs will be removed from the iPod and from all its playlists.".to_string(),
            ),
        };
        let dialog = adw::AlertDialog::new(Some(&heading), Some(&body));
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        let this = Rc::downgrade(self);
        dialog.connect_response(Some("delete"), move |_, _| {
            if let Some(this) = this.upgrade() {
                this.select_button.set_active(false);
                // try_for_each stops at the first error and returns it.
                this.change(|db| ids.iter().try_for_each(|&id| db.remove_track(id)));
                let mut covers = this.covers.borrow_mut();
                for id in &ids {
                    covers.remove(id);
                }
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

    /// Asks for a name and creates a playlist with `songs` in it (if any).
    fn ask_playlist_name(self: &Rc<Self>, songs: Vec<TrackId>) {
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
            this.select_button.set_active(false);
            this.change(|db| {
                let playlist = db.create_playlist(&name)?;
                songs.iter().try_for_each(|&song| db.add_to_playlist(playlist, song))
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
        self.eject_button.set_sensitive(false);
        let this = self.clone();
        glib::spawn_future_local(async move {
            // On success the iPod disappears and the window goes back to the
            // device list by itself.
            if let Err(error) = eject::eject(&this.mount_path).await {
                this.eject_button.set_sensitive(true);
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

/// The "Add to Playlist" menu of the selection bar.
fn selection_menu(playlists: &[gpod::Playlist]) -> gio::Menu {
    let menu = gio::Menu::new();
    let existing = gio::Menu::new();
    for playlist in playlists {
        let item = gio::MenuItem::new(Some(&playlist.name), None);
        item.set_action_and_target_value(
            Some("library.add-selected-to-playlist"),
            Some(&playlist.id.to_variant()),
        );
        existing.append_item(&item);
    }
    menu.append_section(None, &existing);
    let new = gio::Menu::new();
    new.append(Some("New Playlist…"), Some("library.new-playlist-with-selected"));
    menu.append_section(None, &new);
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

/// The "Sort by" menu: one radio item per sort mode, all for the `library.sort` action.
fn sort_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    for mode in SortMode::ALL {
        let item = gio::MenuItem::new(Some(mode.label()), None);
        item.set_action_and_target_value(Some("library.sort"), Some(&mode.id().to_variant()));
        menu.append_item(&item);
    }
    menu
}

fn sort_label(mode: SortMode) -> String {
    format!("Sort by {}", mode.label())
}

/// For trying the song list with a big library. In a debug build,
/// `OXIPOD_FAKE_SONGS=5000 cargo run` adds that many made-up songs to the
/// list. They aren't in the database, so deleting one or adding it to a
/// playlist just shows an error. Release builds ignore the variable.
fn with_test_songs(mut tracks: Vec<Track>) -> Vec<Track> {
    if !cfg!(debug_assertions) {
        return tracks;
    }
    let count: u64 = std::env::var("OXIPOD_FAKE_SONGS").ok().and_then(|n| n.parse().ok()).unwrap_or(0);
    tracks.extend((0..count).map(|n| Track {
        id: u64::MAX - n,
        title: Some(format!("Fake Song {:05}", (n * 7919) % count.max(1))),
        artist: Some(format!("Fake Artist {}", n % 97)),
        album: Some(format!("Fake Album {}", n % 389)),
        length: Duration::from_secs(120 + n % 240),
        track_number: (n % 12) as i32 + 1,
        disc_number: 1,
        added: 1_700_000_000 + n * 60,
    }));
    tracks
}

fn files_count(count: usize) -> String {
    if count == 1 { "1 file".into() } else { format!("{count} files") }
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
