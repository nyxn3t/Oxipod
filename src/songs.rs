//! The song list on the Songs tab.
//!
//! Showing thousands of songs with one widget per song would be slow, so this
//! uses a `gtk::ListView`. It only builds widgets for the rows you can see and
//! reuses them while you scroll. The songs themselves live in a *model*, and
//! several models are stacked on top of each other, each doing one job (the
//! data flows upwards):
//!
//! ```text
//!   ListView          draws the rows
//!   NoSelection       (GTK's own selection is off, the songs know if they're ticked)
//!   SortListModel     puts the songs in order
//!   ListStore         every song on the iPod
//! ```

use std::cell::{Cell, OnceCell, RefCell};
use std::cmp::Ordering;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, gio, glib};

use crate::gpod::{Track, TrackId};
use crate::library::{COVER_SIZE, format_duration};

/// A song, plus its lowercase title, which sorting compares.
///
/// The lowercase text is made once, here, instead of on every comparison:
/// sorting a few thousand songs compares them tens of thousands of times.
pub struct Song {
    pub track: Track,
    title_key: String,
}

impl Song {
    fn new(track: Track) -> Self {
        // "No title" becomes "", which sorts first.
        let title_key = track.title.as_deref().unwrap_or_default().to_lowercase();
        Song { track, title_key }
    }

    /// Which of two songs comes first: by title, ignoring case, like the iPod
    /// does.
    ///
    /// `Ordering` is `Less`, `Equal` or `Greater`. `then_with` only looks at the
    /// next rule when the earlier one said `Equal`. The last rule (the id) is
    /// different for every song, so two songs are never really equal and the
    /// order doesn't jump around.
    fn compare(&self, other: &Song) -> Ordering {
        self.title_key.cmp(&other.title_key).then_with(|| self.track.id.cmp(&other.track.id))
    }
}

mod imp {
    use super::*;

    /// The part of `SongObject` that holds the data.
    ///
    /// GTK's list models can only hold GObjects, so each song is wrapped in
    /// one. A GObject subclass is written in two halves: this struct with the
    /// fields (`imp` is short for "implementation"), and the wrapper type
    /// further down, which is what the rest of the program uses.
    #[derive(Default, glib::Properties)]
    #[properties(wrapper_type = super::SongObject)]
    pub struct SongObject {
        /// Filled in once, right after the object is made. An `OnceCell` can be
        /// written one time and then only read, without needing `&mut`.
        pub(super) song: OnceCell<Song>,
        /// Whether the song is ticked in selection mode. A *property*: other
        /// code can watch it, so a row's check box follows the song.
        #[property(get, set = Self::set_selected, explicit_notify)]
        selected: Cell<bool>,
        /// The cover, once it has been loaded (it's loaded when the row is first shown).
        #[property(get, set, nullable)]
        cover: RefCell<Option<gdk::Texture>>,
        /// True once somebody asked for the cover, so it's only asked for once.
        pub(super) cover_requested: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SongObject {
        const NAME: &'static str = "OxipodSongObject";
        type Type = super::SongObject;
    }

    #[glib::derived_properties]
    impl ObjectImpl for SongObject {}

    impl SongObject {
        /// `explicit_notify` means we say when the property changed. Doing it
        /// only when the value really is different saves a lot of pointless
        /// work when "Select All" sets thousands of songs that were already ticked.
        fn set_selected(&self, selected: bool) {
            if self.selected.replace(selected) != selected {
                self.obj().notify_selected();
            }
        }
    }
}

glib::wrapper! {
    /// One song in the list model.
    pub struct SongObject(ObjectSubclass<imp::SongObject>);
}

impl SongObject {
    fn new(track: Track) -> Self {
        let object: Self = glib::Object::new();
        assert!(object.imp().song.set(Song::new(track)).is_ok(), "a new object has no song yet");
        object
    }

    pub fn song(&self) -> &Song {
        self.imp().song.get().expect("new() always sets the song")
    }

    pub fn id(&self) -> TrackId {
        self.song().track.id
    }
}

/// Calls a function once, a moment later, however many times it is poked in
/// between. Ticking 5000 songs changes 5000 songs, but the selection bar only
/// needs to be redrawn once afterwards.
#[derive(Default)]
struct Notifier {
    callback: RefCell<Option<Rc<dyn Fn()>>>,
    pending: Cell<bool>,
}

impl Notifier {
    fn set(&self, callback: impl Fn() + 'static) {
        self.callback.replace(Some(Rc::new(callback)));
    }

    fn poke(self: &Rc<Self>) {
        if self.pending.replace(true) {
            return;
        }
        let this = self.clone();
        glib::idle_add_local_once(move || {
            this.pending.set(false);
            // Clone the callback out first: it may call `set` again.
            let callback = this.callback.borrow().clone();
            if let Some(callback) = callback {
                callback();
            }
        });
    }
}

/// How many songs there are, how long they play, and how many are ticked.
pub struct Stats {
    pub total: usize,
    pub length: Duration,
    pub selected: usize,
}

pub struct SongList {
    store: gio::ListStore,
    sorted: gtk::SortListModel,
    scroller: gtk::ScrolledWindow,
    selection_changed: Rc<Notifier>,
    shown_changed: Rc<Notifier>,
}

impl SongList {
    /// `select_button` is the header button that turns selection mode on and
    /// off; the rows follow it. `load_cover` and `song_menu` are asked for a
    /// song's cover and its ⋮ menu when a row needs them.
    pub fn new(
        select_button: &gtk::ToggleButton,
        load_cover: impl Fn(TrackId) -> Option<gdk::Texture> + 'static,
        song_menu: impl Fn(TrackId) -> gio::Menu + 'static,
    ) -> Self {
        let store = gio::ListStore::new::<SongObject>();

        let sorter = gtk::CustomSorter::new(|a, b| {
            // The model only ever holds SongObjects, so these can't fail.
            let a = a.downcast_ref::<SongObject>().expect("a SongObject");
            let b = b.downcast_ref::<SongObject>().expect("a SongObject");
            a.song().compare(b.song()).into()
        });
        let sorted = gtk::SortListModel::new(Some(store.clone()), Some(sorter));

        let selection = gtk::NoSelection::new(Some(sorted.clone()));
        let factory = row_factory(select_button, Rc::new(load_cover), Rc::new(song_menu));
        let view = gtk::ListView::builder()
            .model(&selection)
            .factory(&factory)
            .show_separators(true)
            .single_click_activate(true)
            .build();

        // A row is "activated" by clicking it or pressing Enter. Rows are only
        // activatable in selection mode (see the factory), where it ticks the song.
        view.connect_activate(|view, position| {
            let song = view.model().and_then(|model| model.item(position)).and_downcast::<SongObject>();
            if let Some(song) = song {
                song.set_selected(!song.selected());
            }
        });

        let scroller = gtk::ScrolledWindow::builder()
            .child(&view)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .css_classes(["card"])
            .overflow(gtk::Overflow::Hidden)
            .build();

        let shown_changed = Rc::new(Notifier::default());
        let notifier = shown_changed.clone();
        sorted.connect_items_changed(move |_, _, _, _| notifier.poke());

        SongList { store, sorted, scroller, selection_changed: Rc::default(), shown_changed }
    }

    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.scroller
    }

    /// `f` runs (a moment after the last change) when songs are ticked or unticked.
    pub fn connect_selection_changed(&self, f: impl Fn() + 'static) {
        self.selection_changed.set(f);
    }

    /// `f` runs (a moment after the last change) when songs are added or deleted.
    pub fn connect_shown_changed(&self, f: impl Fn() + 'static) {
        self.shown_changed.set(f);
    }

    /// Makes the list show exactly these songs.
    ///
    /// Songs that are already in the list are left alone, so they keep their
    /// tick, their cover and the scroll position stays put. Only the songs
    /// that are gone are removed, and the new ones added.
    pub fn set_tracks(&self, tracks: &[Track]) {
        let wanted: std::collections::HashSet<TrackId> = tracks.iter().map(|track| track.id).collect();
        let mut present = std::collections::HashSet::new();
        let mut gone = Vec::new();
        for (position, song) in songs_in(&self.store).enumerate() {
            if wanted.contains(&song.id()) {
                present.insert(song.id());
            } else {
                gone.push(position as u32);
            }
        }
        // Remove from the back, so the positions still to be removed don't shift.
        for position in gone.into_iter().rev() {
            self.store.remove(position);
        }

        let new_songs: Vec<SongObject> = tracks
            .iter()
            .filter(|track| !present.contains(&track.id))
            .map(|track| {
                let song = SongObject::new(track.clone());
                let notifier = self.selection_changed.clone();
                song.connect_selected_notify(move |_| notifier.poke());
                song
            })
            .collect();
        self.store.extend_from_slice(&new_songs);
    }

    pub fn stats(&self) -> Stats {
        let mut stats = Stats { total: self.store.n_items() as usize, length: Duration::ZERO, selected: 0 };
        for song in songs_in(&self.store) {
            stats.length += song.song().track.length;
            stats.selected += usize::from(song.selected());
        }
        stats
    }

    /// The ticked songs, in the order the list shows them.
    pub fn selected_ids(&self) -> Vec<TrackId> {
        songs_in(&self.sorted).filter(|song| song.selected()).map(|song| song.id()).collect()
    }

    /// Ticks or unticks every song.
    pub fn select_all(&self, on: bool) {
        for song in songs_in(&self.sorted) {
            song.set_selected(on);
        }
    }

    /// Unticks every song.
    pub fn clear_selection(&self) {
        for song in songs_in(&self.store) {
            song.set_selected(false);
        }
    }
}

/// Every song in a model, in the model's order.
///
/// `gio`'s own `iter::<SongObject>()` can't be used on the sort model: it
/// says it holds plain `Object`s, and `iter` refuses to cast. So this asks
/// for each item and checks its type itself. `impl Iterator` means "some
/// iterator that gives `SongObject`s": the caller can use it in a `for` loop
/// without knowing the exact type, and nothing is collected into a Vec first.
fn songs_in(model: &impl IsA<gio::ListModel>) -> impl Iterator<Item = SongObject> + '_ {
    (0..model.n_items()).filter_map(|position| model.item(position).and_downcast::<SongObject>())
}

/// What a row is currently connected to. Dropping it disconnects the row from
/// the song (Rust runs `drop` automatically when a value goes away), so a
/// recycled row can't keep reacting to the song it used to show.
struct Shown {
    song: SongObject,
    handlers: Vec<glib::SignalHandlerId>,
}

impl Drop for Shown {
    fn drop(&mut self) {
        for handler in self.handlers.drain(..) {
            self.song.disconnect(handler);
        }
    }
}

type CoverLoader = Rc<dyn Fn(TrackId) -> Option<gdk::Texture>>;
type MenuBuilder = Rc<dyn Fn(TrackId) -> gio::Menu>;

/// Builds the rows. GTK calls `setup` once for each row widget it creates (a
/// screenful of them), and then reuses those widgets for different songs while
/// you scroll: the row's `item` changes, and we fill the widgets in again.
fn row_factory(
    select_button: &gtk::ToggleButton,
    load_cover: CoverLoader,
    song_menu: MenuBuilder,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let select_button = select_button.clone();

    factory.connect_setup(move |_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().expect("the factory makes ListItems");

        let check = gtk::CheckButton::builder()
            .valign(gtk::Align::Center)
            .css_classes(["selection-mode"])
            .build();
        let cover = gtk::Image::builder().pixel_size(COVER_SIZE).build();
        let title = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let subtitle = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();
        let text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .build();
        text.append(&title);
        text.append(&subtitle);
        let length = gtk::Label::builder().css_classes(["dim-label", "numeric"]).build();
        let menu = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .tooltip_text("More")
            .build();

        let row = gtk::Box::builder()
            .spacing(12)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(12)
            .margin_end(12)
            .build();
        row.append(&check);
        row.append(&cover);
        row.append(&text);
        row.append(&length);
        row.append(&menu);
        list_item.set_child(Some(&row));

        // Property bindings keep one property equal to another: when selection
        // mode is on, show the check box instead of the menu, and let the row be
        // clicked. They live as long as both widgets do.
        select_button.bind_property("active", &check, "visible").sync_create().build();
        select_button.bind_property("active", &menu, "visible").sync_create().invert_boolean().build();
        select_button.bind_property("active", list_item, "activatable").sync_create().build();

        // The widgets below are children of the list item, so they must only
        // hold a *weak* reference back to it, or neither could ever be freed.
        let weak_item = list_item.downgrade();
        let song_menu = song_menu.clone();
        menu.set_create_popup_func(move |button| {
            // Called when the menu is about to open: build it for the song the row shows now.
            let song = weak_item.upgrade().and_then(|item| item.item()).and_downcast::<SongObject>();
            if let Some(song) = song {
                button.set_menu_model(Some(&song_menu(song.id())));
            }
        });

        let weak_item = list_item.downgrade();
        check.connect_toggled(move |check| {
            let song = weak_item.upgrade().and_then(|item| item.item()).and_downcast::<SongObject>();
            if let Some(song) = song {
                song.set_selected(check.is_active());
            }
        });

        // GTK sets the list item's `item` when the row starts showing a song
        // (and clears it when the row is no longer needed). The closure owns
        // the `shown` cell, so every row has its own.
        let load_cover = load_cover.clone();
        let shown: RefCell<Option<Shown>> = RefCell::new(None);
        list_item.connect_item_notify(move |list_item| {
            // Forget the previous song first: dropping `Shown` disconnects it.
            shown.replace(None);
            let Some(song) = list_item.item().and_downcast::<SongObject>() else {
                return;
            };

            let track = &song.song().track;
            title.set_text(track.title.as_deref().unwrap_or("Unknown Title"));
            let details = [track.artist.as_deref(), track.album.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" — ");
            subtitle.set_visible(!details.is_empty());
            subtitle.set_text(&details);
            length.set_text(&format_duration(track.length));

            check.set_active(song.selected());
            show_cover(&cover, song.cover().as_ref());
            request_cover(&song, &load_cover);

            // Keep the row up to date while it shows this song.
            let handlers = vec![
                song.connect_selected_notify({
                    let check = check.clone();
                    move |song| check.set_active(song.selected())
                }),
                song.connect_cover_notify({
                    let cover = cover.clone();
                    move |song| show_cover(&cover, song.cover().as_ref())
                }),
            ];
            shown.replace(Some(Shown { song, handlers }));
        });
    });
    factory
}

fn show_cover(image: &gtk::Image, cover: Option<&gdk::Texture>) {
    match cover {
        Some(texture) => image.set_paintable(Some(texture)),
        None => image.set_icon_name(Some("audio-x-generic-symbolic")),
    }
}

/// Loads the song's cover the first time its row is shown. It waits for an idle
/// moment, so scrolling stays smooth: the row appears at once with a plain
/// icon and the cover pops in a moment later (the row hears about it through
/// the song's `cover` property).
fn request_cover(song: &SongObject, load_cover: &CoverLoader) {
    if song.imp().cover_requested.replace(true) {
        return;
    }
    let (song, load_cover) = (song.clone(), load_cover.clone());
    glib::idle_add_local_once(move || song.set_cover(load_cover(song.id()).as_ref()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: u64, title: &str) -> Song {
        Song::new(Track {
            id,
            title: (!title.is_empty()).then(|| title.to_string()),
            artist: None,
            album: None,
            length: Duration::from_secs(180),
        })
    }

    /// Sorts the songs and returns their ids.
    fn order(songs: &mut [Song]) -> Vec<u64> {
        songs.sort_by(|a, b| a.compare(b));
        songs.iter().map(|song| song.track.id).collect()
    }

    #[test]
    fn sorts_by_title_ignoring_case() {
        let mut songs = [song(1, "banana"), song(2, "Apple"), song(3, "cherry"), song(4, "")];
        assert_eq!(order(&mut songs), [4, 2, 1, 3]);
    }

    #[test]
    fn songs_that_look_the_same_keep_a_steady_order() {
        let mut songs = [song(9, "same"), song(3, "same")];
        assert_eq!(order(&mut songs), [3, 9]);
    }

    /// How many widgets of one GTK type there are inside `widget`.
    fn count_widgets(widget: &gtk::Widget, type_name: &str) -> usize {
        let mut count = usize::from(widget.type_().name() == type_name);
        let mut child = widget.first_child();
        while let Some(current) = child {
            count += count_widgets(&current, type_name);
            child = current.next_sibling();
        }
        count
    }

    /// The whole list with 20,000 songs, in a real window. GTK needs a display
    /// and can only be started once per program, so this is one test; run it
    /// with `cargo test -- --ignored --nocapture`. A window flashes up for a
    /// second or two.
    #[test]
    #[ignore = "needs a display and opens a window"]
    fn handles_20000_songs() {
        use std::time::Instant;

        gtk::init().expect("GTK needs a display");
        let button = gtk::ToggleButton::new();
        let list = SongList::new(&button, |_| None, |_| gio::Menu::new());
        let tracks: Vec<Track> = (0..20_000u64)
            .map(|n| Track {
                id: n + 1,
                title: Some(format!("Song {:05}", (n * 7919) % 20_000)),
                artist: Some(format!("Artist {}", n % 97)),
                album: Some(format!("Album {}", n % 389)),
                length: Duration::from_secs(200),
            })
            .collect();

        let start = Instant::now();
        list.set_tracks(&tracks);
        println!("adding 20000 songs and sorting them by title: {:?}", start.elapsed());
        assert_eq!(list.stats().total, 20_000);
        let first = list.sorted.item(0).and_downcast::<SongObject>().unwrap();
        assert_eq!(first.song().track.title.as_deref(), Some("Song 00000"));

        // Now show it. Letting the main loop run for a moment lays the window
        // out and creates the rows, which runs the row factory for real.
        let window = gtk::Window::builder().default_width(500).default_height(500).child(list.widget()).build();
        window.present();
        let settle = || {
            let context = glib::MainContext::default();
            let start = Instant::now();
            while start.elapsed() < Duration::from_millis(700) {
                context.iteration(false);
            }
        };
        settle();
        // GTK keeps a pool of about 200 row widgets however long the list is
        // (10 or so are on screen), instead of one per song.
        let rows = count_widgets(list.widget().upcast_ref(), "GtkListItemWidget");
        println!("row widgets for 20000 songs in a 500 px window: {rows}");
        assert!(rows < 300, "a fixed pool of rows, not one per song");

        // Selection mode: tick everything, then untick it.
        button.set_active(true);
        list.select_all(true);
        settle();
        assert_eq!(list.stats().selected, 20_000);
        assert_eq!(list.selected_ids().len(), 20_000);
        list.clear_selection();
        settle();
        assert_eq!(list.stats().selected, 0);
        window.close();
    }
}
