//! Editing the tags of songs: the dialog, and the rules behind it.
//!
//! The tags are the texts that describe a song: its title, artist, album and
//! album artist. They are changed in the iPod's database only. The song files
//! on the computer keep their own tags, and the iPod doesn't read the ones
//! inside its files, it shows what the database says.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::gpod::{Tags, Track};
use crate::library::boxed_list;

/// The texts the dialog can change, in the order it shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Title,
    Artist,
    Album,
    AlbumArtist,
}

impl Field {
    const ALL: [Field; 4] = [Field::Title, Field::Artist, Field::Album, Field::AlbumArtist];

    fn name(self) -> &'static str {
        match self {
            Field::Title => "Title",
            Field::Artist => "Artist",
            Field::Album => "Album",
            Field::AlbumArtist => "Album Artist",
        }
    }

    /// This field of a song.
    fn of(self, track: &Track) -> Option<&str> {
        match self {
            Field::Title => track.title.as_deref(),
            Field::Artist => track.artist.as_deref(),
            Field::Album => track.album.as_deref(),
            Field::AlbumArtist => track.album_artist.as_deref(),
        }
    }

    /// Sets this field of `tags`.
    fn put(self, tags: &mut Tags, text: String) {
        // `&mut` to the right field, so one `match` finds it and one line sets it.
        let field = match self {
            Field::Title => &mut tags.title,
            Field::Artist => &mut tags.artist,
            Field::Album => &mut tags.album,
            Field::AlbumArtist => &mut tags.album_artist,
        };
        *field = Some(text);
    }

    /// What a row calls itself: the name of the field, and, while the songs
    /// have different texts in it and nothing was typed yet, a hint that says so.
    fn row_title(self, start: &Start, typed: &str) -> String {
        if start.differs && typed.is_empty() {
            format!("{} (different for each song)", self.name())
        } else {
            self.name().to_string()
        }
    }
}

/// What a field starts out with when the dialog opens.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Start {
    /// The text all the songs share. Empty when they don't have one, or when they differ.
    text: String,
    /// Whether the songs have different texts. The field starts out empty then,
    /// and an empty field means "leave them as they are".
    differs: bool,
}

/// The start of one field, from what each song has in it. No text and empty
/// text are the same, and spaces around a text don't count.
fn start_of<'a>(values: impl IntoIterator<Item = Option<&'a str>>) -> Start {
    let mut values = values.into_iter().map(|value| value.unwrap_or_default().trim());
    let first = values.next().unwrap_or_default();
    if values.all(|value| value == first) {
        Start { text: first.to_string(), differs: false }
    } else {
        Start { text: String::new(), differs: true }
    }
}

/// What the user changed: every field whose text is not what it started with.
/// `fields` has each field with its start and the text in its row now.
fn changes<'a>(fields: impl IntoIterator<Item = (Field, &'a Start, String)>) -> Tags {
    let mut tags = Tags::default();
    for (field, start, text) in fields {
        let text = text.trim();
        if text != start.text {
            field.put(&mut tags, text.to_string());
        }
    }
    tags
}

/// Whether these changes can be applied: there is one, and a song keeps a title.
fn can_apply(tags: &Tags) -> bool {
    !tags.is_empty() && tags.title.as_deref().is_none_or(|title| !title.is_empty())
}

/// Asks for new tags for `songs`, and calls `apply` with what was changed when
/// the user presses Apply. With several songs the title is left out (each song
/// has one of its own), and a field the songs differ in starts out empty.
pub fn show(parent: &impl IsA<gtk::Widget>, songs: &[Track], apply: impl Fn(Tags) + 'static) {
    let several = songs.len() > 1;
    let list = boxed_list("");
    // Each field with what it started with and the row that edits it. The rows
    // are held weakly: they sit inside the dialog, and the handlers that look
    // at them belong to the dialog too, so holding them tightly would be a loop.
    let mut fields: Vec<(Field, Start, glib::WeakRef<adw::EntryRow>)> = Vec::new();
    for field in Field::ALL {
        if several && field == Field::Title {
            continue;
        }
        let start = start_of(songs.iter().map(|song| field.of(song)));
        // activates_default: pressing Enter picks the default response, "Apply".
        let row = adw::EntryRow::builder().title(field.row_title(&start, "")).activates_default(true).build();
        row.set_text(&start.text);
        list.append(&row);
        fields.push((field, start, row.downgrade()));
    }
    let fields = Rc::new(fields);

    let heading = if several { format!("Edit Tags of {} Songs", songs.len()) } else { "Edit Tags".to_string() };
    let dialog = adw::AlertDialog::new(
        Some(&heading),
        Some("This changes the iPod only. The song files on your computer keep their own tags."),
    );
    dialog.set_extra_child(Some(&list));
    dialog.add_responses(&[("cancel", "Cancel"), ("apply", "Apply")]);
    dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
    dialog.set_response_enabled("apply", false);
    dialog.set_default_response(Some("apply"));
    dialog.set_close_response("cancel");

    // What the user has changed so far.
    let changes_now: Rc<dyn Fn() -> Tags> = {
        let fields = fields.clone();
        Rc::new(move || {
            changes(fields.iter().filter_map(|(field, start, row)| Some((*field, start, row.upgrade()?.text().to_string()))))
        })
    };

    for (field, start, row) in fields.iter() {
        let Some(row) = row.upgrade() else {
            continue;
        };
        row.connect_changed({
            let (field, start) = (*field, start.clone());
            let (dialog, changes_now) = (dialog.downgrade(), changes_now.clone());
            move |row| {
                row.set_title(&field.row_title(&start, row.text().trim()));
                if let Some(dialog) = dialog.upgrade() {
                    dialog.set_response_enabled("apply", can_apply(&changes_now()));
                }
            }
        });
    }

    dialog.connect_response(Some("apply"), move |_, _| {
        let tags = changes_now();
        if can_apply(&tags) {
            apply(tags);
        }
    });
    dialog.present(Some(parent));
    if let Some(row) = fields.first().and_then(|(_, _, row)| row.upgrade()) {
        row.grab_focus();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn songs_that_agree_start_with_their_text() {
        let start = start_of([Some("Artist A"), Some("Artist A")]);
        assert_eq!(start, Start { text: "Artist A".into(), differs: false });
        // One song is the same as itself.
        assert_eq!(start_of([Some("Solo")]).text, "Solo");
    }

    #[test]
    fn songs_that_differ_start_empty() {
        let start = start_of([Some("Artist A"), Some("Artist B")]);
        assert_eq!(start, Start { text: String::new(), differs: true });
        // One that has the text and one that has nothing differ as well.
        assert!(start_of([Some("Artist A"), None]).differs);
    }

    #[test]
    fn no_text_empty_text_and_spaces_are_the_same() {
        let start = start_of([None, Some(""), Some("  ")]);
        assert_eq!(start, Start { text: String::new(), differs: false });
        assert_eq!(start_of([Some(" Album "), Some("Album")]).text, "Album");
    }

    fn fields<'a>(starts: &'a [(Field, Start)], texts: &[&str]) -> Vec<(Field, &'a Start, String)> {
        starts.iter().zip(texts).map(|((field, start), text)| (*field, start, text.to_string())).collect()
    }

    #[test]
    fn only_what_was_changed_is_in_the_result() {
        let starts = [
            (Field::Title, start_of([Some("Old Title")])),
            (Field::Artist, start_of([Some("Artist")])),
            (Field::Album, start_of([Some("Album")])),
        ];
        // Nothing typed: nothing to do.
        let nothing = changes(fields(&starts, &["Old Title", "Artist", "Album"]));
        assert!(nothing.is_empty());
        assert!(!can_apply(&nothing));

        // Spaces don't count as a change, and the artist alone is changed.
        let tags = changes(fields(&starts, &[" Old Title ", "New Artist", "Album"]));
        assert_eq!(tags, Tags { artist: Some("New Artist".into()), ..Tags::default() });
        assert!(can_apply(&tags));
    }

    #[test]
    fn a_field_the_songs_differ_in_stays_until_something_is_typed() {
        let starts = [(Field::Artist, start_of([Some("A"), Some("B")])), (Field::Album, start_of([Some("X"), Some("X")]))];
        // The artist is empty (they differ): it stays. The album is what they share.
        assert!(changes(fields(&starts, &["", "X"])).is_empty());
        let tags = changes(fields(&starts, &["Various", "X"]));
        assert_eq!(tags, Tags { artist: Some("Various".into()), ..Tags::default() });
    }

    #[test]
    fn clearing_what_all_songs_share_empties_it() {
        let starts = [(Field::Album, start_of([Some("X"), Some("X")]))];
        let tags = changes(fields(&starts, &[""]));
        assert_eq!(tags, Tags { album: Some(String::new()), ..Tags::default() });
        assert!(can_apply(&tags), "a song can lose its album");
    }

    #[test]
    fn a_song_cannot_lose_its_title() {
        let starts = [(Field::Title, start_of([Some("Old Title")]))];
        let tags = changes(fields(&starts, &["   "]));
        assert_eq!(tags.title.as_deref(), Some(""));
        assert!(!can_apply(&tags));
    }

    #[test]
    fn a_row_says_when_the_songs_differ_and_nothing_was_typed() {
        let differ = Start { text: String::new(), differs: true };
        let agree = Start { text: "X".into(), differs: false };
        assert_eq!(Field::Artist.row_title(&differ, ""), "Artist (different for each song)");
        assert_eq!(Field::Artist.row_title(&differ, "Various"), "Artist");
        assert_eq!(Field::AlbumArtist.row_title(&agree, ""), "Album Artist");
    }
}
