//! The song list on the Songs tab, with search and sorting.
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
//!   FilterListModel   hides the songs that don't match the search
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

/// A song, plus the lowercase text that searching and sorting compare.
///
/// That text is made once, here, instead of on every comparison: sorting a
/// few thousand songs compares them tens of thousands of times.
pub struct Song {
    pub track: Track,
    /// Title, artist and album in one lowercase string, for the search.
    haystack: String,
    title_key: String,
    artist_key: String,
    album_key: String,
}

impl Song {
    fn new(track: Track) -> Self {
        // A closure that lowercases an optional text; "no text" becomes "".
        let lower = |text: &Option<String>| text.as_deref().unwrap_or_default().to_lowercase();
        let title_key = lower(&track.title);
        let artist_key = lower(&track.artist);
        let album_key = lower(&track.album);
        let haystack = format!("{title_key} {artist_key} {album_key}");
        Song { track, haystack, title_key, artist_key, album_key }
    }

    /// Whether every word of the search is somewhere in the title, artist or
    /// album. "lorna pain" finds "Pain Remains" by "Lorna Shore".
    fn matches(&self, words: &[String]) -> bool {
        words.iter().all(|word| self.haystack.contains(word.as_str()))
    }

    /// Where the song sits on its album: by disc, then by track number. Songs
    /// that don't say come last.
    fn position(&self) -> (i32, i32) {
        let track = if self.track.track_number > 0 { self.track.track_number } else { i32::MAX };
        (self.track.disc_number.max(1), track)
    }

    /// Which of two songs comes first when sorting by `mode`.
    ///
    /// `Ordering` is `Less`, `Equal` or `Greater`. `then_with` only looks at the
    /// next rule when the earlier ones said `Equal`, which is how "by artist,
    /// then by album, then by track number" is written. The last rule (the id)
    /// is different for every song, so two songs are never really equal and
    /// the order doesn't jump around.
    fn compare(&self, other: &Song, mode: SortMode) -> Ordering {
        let by_title = || self.title_key.cmp(&other.title_key);
        let by_artist = || unknown_last(&self.artist_key, &other.artist_key);
        let by_album = || unknown_last(&self.album_key, &other.album_key);
        let by_position = || self.position().cmp(&other.position());
        match mode {
            SortMode::Title => by_title().then_with(by_artist),
            SortMode::Artist => by_artist().then_with(by_album).then_with(by_position).then_with(by_title),
            SortMode::Album => by_album().then_with(by_artist).then_with(by_position).then_with(by_title),
            // Newest first: note `other` on the left.
            SortMode::Added => other.track.added.cmp(&self.track.added).then_with(by_title),
        }
        .then_with(|| self.track.id.cmp(&other.track.id))
    }
}

/// Compares two names, with the empty one (unknown artist or album) last.
fn unknown_last(a: &str, b: &str) -> Ordering {
    // `false` sorts before `true`, so a name that isn't empty comes first.
    a.is_empty().cmp(&b.is_empty()).then_with(|| a.cmp(b))
}

/// What the song list is sorted by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortMode {
    #[default]
    Title,
    Artist,
    Album,
    Added,
}

impl SortMode {
    pub const ALL: [SortMode; 4] = [SortMode::Title, SortMode::Artist, SortMode::Album, SortMode::Added];

    /// The name used for the menu action's value.
    pub fn id(self) -> &'static str {
        match self {
            SortMode::Title => "title",
            SortMode::Artist => "artist",
            SortMode::Album => "album",
            SortMode::Added => "added",
        }
    }

    pub fn from_id(id: &str) -> Option<SortMode> {
        SortMode::ALL.into_iter().find(|mode| mode.id() == id)
    }

    pub fn label(self) -> &'static str {
        match self {
            SortMode::Title => "Title",
            SortMode::Artist => "Artist",
            SortMode::Album => "Album",
            SortMode::Added => "Recently Added",
        }
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

/// How many songs there are, which are shown, and which are ticked.
pub struct Stats {
    pub total: usize,
    pub shown: usize,
    /// Playing time of the songs that are shown.
    pub shown_length: Duration,
    /// Ticked songs, including those the search hides.
    pub selected: usize,
    pub shown_selected: usize,
}

pub struct SongList {
    store: gio::ListStore,
    sorted: gtk::SortListModel,
    filtered: gtk::FilterListModel,
    sorter: gtk::CustomSorter,
    filter: gtk::CustomFilter,
    scroller: gtk::ScrolledWindow,
    /// Shared with the sorter, which reads it for every comparison.
    sort_mode: Rc<Cell<SortMode>>,
    /// The search, split into lowercase words. Shared with the filter.
    words: Rc<RefCell<Vec<String>>>,
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
        let sort_mode = Rc::new(Cell::new(SortMode::default()));
        let words: Rc<RefCell<Vec<String>>> = Rc::default();

        let store = gio::ListStore::new::<SongObject>();

        // `move` hands the closure its own copy of `sort_mode` (the Rc is
        // cloned first, so both the list and the sorter keep one).
        let sorter = gtk::CustomSorter::new({
            let sort_mode = sort_mode.clone();
            move |a, b| {
                // The model only ever holds SongObjects, so these can't fail.
                let a = a.downcast_ref::<SongObject>().expect("a SongObject");
                let b = b.downcast_ref::<SongObject>().expect("a SongObject");
                a.song().compare(b.song(), sort_mode.get()).into()
            }
        });
        let sorted = gtk::SortListModel::new(Some(store.clone()), Some(sorter.clone()));

        let filter = gtk::CustomFilter::new({
            let words = words.clone();
            move |item| {
                let song = item.downcast_ref::<SongObject>().expect("a SongObject");
                song.song().matches(&words.borrow())
            }
        });
        let filtered = gtk::FilterListModel::new(Some(sorted.clone()), Some(filter.clone()));

        let selection = gtk::NoSelection::new(Some(filtered.clone()));
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
        filtered.connect_items_changed(move |_, _, _, _| notifier.poke());

        SongList {
            store,
            sorted,
            filtered,
            sorter,
            filter,
            scroller,
            sort_mode,
            words,
            selection_changed: Rc::default(),
            shown_changed,
        }
    }

    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.scroller
    }

    /// `f` runs (a moment after the last change) when songs are ticked or unticked.
    pub fn connect_selection_changed(&self, f: impl Fn() + 'static) {
        self.selection_changed.set(f);
    }

    /// `f` runs (a moment after the last change) when the shown songs change:
    /// songs added or deleted, a new search.
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

    pub fn set_sort(&self, mode: SortMode) {
        self.sort_mode.set(mode);
        // Tell the sorter its rules changed, so the list sorts itself again.
        self.sorter.changed(gtk::SorterChange::Different);
    }

    /// Shows only the songs that match `query` (all of them when it's empty).
    pub fn set_query(&self, query: &str) {
        let new: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let old = self.words.replace(new.clone());
        if old == new {
            return;
        }
        // Telling the filter how it changed lets it skip work: if the new
        // search can only match a subset of what the old one matched, only the
        // songs shown now need checking again (and the other way around).
        let change = if narrower(&old, &new) {
            gtk::FilterChange::MoreStrict
        } else if narrower(&new, &old) {
            gtk::FilterChange::LessStrict
        } else {
            gtk::FilterChange::Different
        };
        self.filter.changed(change);
    }

    pub fn stats(&self) -> Stats {
        let mut stats = Stats {
            total: self.store.n_items() as usize,
            shown: self.filtered.n_items() as usize,
            shown_length: Duration::ZERO,
            selected: 0,
            shown_selected: 0,
        };
        for song in songs_in(&self.store) {
            stats.selected += usize::from(song.selected());
        }
        for song in songs_in(&self.filtered) {
            stats.shown_length += song.song().track.length;
            stats.shown_selected += usize::from(song.selected());
        }
        stats
    }

    /// The ticked songs (also the ones a search hides), in the order the list shows them.
    pub fn selected_ids(&self) -> Vec<TrackId> {
        songs_in(&self.sorted).filter(|song| song.selected()).map(|song| song.id()).collect()
    }

    /// Ticks or unticks the songs that are shown. Hidden ones stay as they are.
    pub fn select_shown(&self, on: bool) {
        for song in songs_in(&self.filtered) {
            song.set_selected(on);
        }
    }

    /// Unticks every song, hidden or not.
    pub fn clear_selection(&self) {
        for song in songs_in(&self.store) {
            song.set_selected(false);
        }
    }
}

/// Every song in a model, in the model's order.
///
/// `gio`'s own `iter::<SongObject>()` can't be used on the sort and filter
/// models: they say they hold plain `Object`s, and it refuses to cast. So this
/// asks for each item and checks its type itself. `impl Iterator` means "some
/// iterator that gives `SongObject`s": the caller can use it in a `for` loop
/// without knowing the exact type, and nothing is collected into a Vec first.
fn songs_in(model: &impl IsA<gio::ListModel>) -> impl Iterator<Item = SongObject> + '_ {
    (0..model.n_items()).filter_map(|position| model.item(position).and_downcast::<SongObject>())
}

/// Whether every word of `old` is part of some word of `new`. Then a song that
/// matches `new` is sure to match `old` too: `new` is the stricter search.
fn narrower(old: &[String], new: &[String]) -> bool {
    old.iter().all(|word| new.iter().any(|other| other.contains(word.as_str())))
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

    fn song(id: u64, title: &str, artist: &str, album: &str, disc: i32, track: i32, added: u64) -> Song {
        let text = |text: &str| (!text.is_empty()).then(|| text.to_string());
        Song::new(Track {
            id,
            title: text(title),
            artist: text(artist),
            album: text(album),
            length: Duration::from_secs(180),
            track_number: track,
            disc_number: disc,
            added,
        })
    }

    /// Sorts the songs by `mode` and returns their ids.
    fn order(songs: &mut [Song], mode: SortMode) -> Vec<u64> {
        songs.sort_by(|a, b| a.compare(b, mode));
        songs.iter().map(|song| song.track.id).collect()
    }

    #[test]
    fn search_needs_every_word_but_in_any_field() {
        let song = song(1, "Pain Remains I", "Lorna Shore", "Pain Remains", 1, 1, 0);
        let words = |query: &str| query.split_whitespace().map(str::to_lowercase).collect::<Vec<_>>();
        assert!(song.matches(&words("")));
        assert!(song.matches(&words("lorna pain")));
        assert!(song.matches(&words("SHORE remains")));
        assert!(!song.matches(&words("lorna metallica")));
    }

    #[test]
    fn sorts_by_title_ignoring_case() {
        let mut songs = [
            song(1, "banana", "", "", 0, 0, 0),
            song(2, "Apple", "", "", 0, 0, 0),
            song(3, "cherry", "", "", 0, 0, 0),
        ];
        assert_eq!(order(&mut songs, SortMode::Title), [2, 1, 3]);
    }

    #[test]
    fn sorts_by_artist_then_album_then_track_with_unknown_last() {
        let mut songs = [
            song(1, "No Artist", "", "", 0, 0, 0),
            song(2, "B side", "Zed", "Second", 1, 1, 0),
            song(3, "Track two", "Abe", "Debut", 1, 2, 0),
            song(4, "Track one", "Abe", "Debut", 1, 1, 0),
            song(5, "Other disc", "Abe", "Debut", 2, 1, 0),
        ];
        assert_eq!(order(&mut songs, SortMode::Artist), [4, 3, 5, 2, 1]);
    }

    #[test]
    fn newest_songs_come_first() {
        let mut songs = [
            song(1, "old", "", "", 0, 0, 100),
            song(2, "new", "", "", 0, 0, 300),
            song(3, "middle", "", "", 0, 0, 200),
        ];
        assert_eq!(order(&mut songs, SortMode::Added), [2, 3, 1]);
    }

    #[test]
    fn songs_that_look_the_same_keep_a_steady_order() {
        let mut songs = [song(9, "same", "", "", 0, 0, 0), song(3, "same", "", "", 0, 0, 0)];
        assert_eq!(order(&mut songs, SortMode::Title), [3, 9]);
    }

    #[test]
    fn knows_when_a_search_became_stricter() {
        let words = |query: &str| query.split_whitespace().map(str::to_lowercase).collect::<Vec<_>>();
        assert!(narrower(&words("lorna"), &words("lorna shore"))); // another word
        assert!(narrower(&words("lor"), &words("lorna"))); // a longer word
        assert!(narrower(&words(""), &words("anything"))); // from nothing
        assert!(!narrower(&words("lorna shore"), &words("lorna")));
        assert!(!narrower(&words("lorna"), &words("shore")));
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

    /// The whole list with 20,000 songs: the models, and the rows in a real
    /// window. GTK needs a display and can only be started once per program,
    /// so this is one test; run it with `cargo test -- --ignored --nocapture`.
    /// A window flashes up for a second or two.
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
                track_number: (n % 12) as i32 + 1,
                disc_number: 1,
                added: n,
            })
            .collect();

        let start = Instant::now();
        list.set_tracks(&tracks);
        println!("adding 20000 songs and sorting them by title: {:?}", start.elapsed());
        assert_eq!(list.stats().shown, 20_000);

        for mode in SortMode::ALL {
            let start = Instant::now();
            list.set_sort(mode);
            let first = list.sorted.item(0).and_downcast::<SongObject>().unwrap();
            println!("sorting by {}: {:?} (first: {:?})", mode.label(), start.elapsed(), first.song().track.title);
        }
        list.set_sort(SortMode::Title);

        for query in ["artist 5", "artist 51", "album 17 song 0001", ""] {
            let start = Instant::now();
            list.set_query(query);
            println!("search {query:?}: {:?}, {} shown", start.elapsed(), list.stats().shown);
        }
        assert_eq!(list.stats().shown, 20_000);

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

        // Selection mode, tick everything, search, untick.
        button.set_active(true);
        list.select_shown(true);
        settle();
        assert_eq!(list.stats().selected, 20_000);
        list.set_query("song 0001");
        settle();
        let stats = list.stats();
        println!("searching 'song 0001' while everything is ticked: {} shown, {} ticked", stats.shown, stats.selected);
        assert_eq!(stats.shown_selected, stats.shown);
        assert_eq!(list.selected_ids().len(), 20_000);
        list.clear_selection();
        settle();
        assert_eq!(list.stats().selected, 0);
        window.close();
    }
}
