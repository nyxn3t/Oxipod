//! Finding songs that look like the same song twice.
//!
//! Two songs are *possible duplicates* when they have the same title and the
//! same artist (ignoring capitals, spaces and punctuation) and are about
//! equally long. Only "possible": a live version or a remix can have the same
//! name, so the app shows such songs together and the person decides.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::time::Duration;

use crate::gpod::{Track, TrackId};

/// Songs that differ in length by no more than this count as equally long. The
/// same song encoded twice can differ by a fraction of a second, while a live
/// version or another mix is usually much longer or shorter.
const SAME_LENGTH: Duration = Duration::from_secs(3);

/// A title or artist reduced to its letters and digits in lowercase, so
/// "I'm Possessed" and "Im possessed!" come out the same.
///
/// `flat_map` turns every character into zero or more characters (the
/// lowercase form of a letter can be longer than the letter) and glues the
/// results together.
fn plain(text: Option<&str>) -> String {
    text.unwrap_or_default().chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// How good a copy is, as something to sort by, where the smallest value is
/// the best: a higher bit rate, then one that has a cover, then a bigger file,
/// then the one that was on the iPod first. `Reverse` flips the order, so the
/// big numbers (and `true`, which counts as bigger than `false`) sort first.
/// The id at the end keeps otherwise equal copies in a steady order.
fn quality(track: &Track) -> (Reverse<i32>, Reverse<bool>, Reverse<u32>, u64, TrackId) {
    (Reverse(track.bitrate), Reverse(track.has_cover), Reverse(track.size), track.added, track.id)
}

/// The groups of possible duplicates among `tracks`.
///
/// Every group has at least two songs, the best copy first (the one to keep).
/// The groups are in alphabetical order.
pub fn find_duplicates(tracks: &[Track]) -> Vec<Vec<TrackId>> {
    // First everything that has the same plain title and artist...
    let mut by_name: HashMap<(String, String), Vec<&Track>> = HashMap::new();
    for track in tracks {
        let name = (plain(track.title.as_deref()), plain(track.artist.as_deref()));
        // With no title or no artist there is too little to go on: plenty of
        // unrelated songs are called "Intro".
        if !name.0.is_empty() && !name.1.is_empty() {
            by_name.entry(name).or_default().push(track);
        }
    }

    // ...then split those by length: walking from the shortest to the longest,
    // a gap to the previous song starts a new group. (A chain of songs that
    // are each close to the next ends up in one group.)
    let mut groups: Vec<((String, String), Vec<&Track>)> = Vec::new();
    for (name, mut songs) in by_name {
        songs.sort_by_key(|track| (track.length, track.id));
        let mut group: Vec<&Track> = Vec::new();
        for song in songs {
            let gap = group.last().is_some_and(|last| song.length.saturating_sub(last.length) > SAME_LENGTH);
            if gap {
                // `take` moves the finished group out and leaves an empty one behind.
                groups.push((name.clone(), std::mem::take(&mut group)));
            }
            group.push(song);
        }
        groups.push((name, group));
    }

    let mut groups: Vec<_> = groups
        .into_iter()
        .filter(|(_, songs)| songs.len() > 1)
        .map(|(name, mut songs)| {
            songs.sort_by_key(|track| quality(track));
            (name, songs.into_iter().map(|track| track.id).collect::<Vec<_>>())
        })
        .collect();
    // Tuples are sorted by their first part, then the next one: by name, and
    // by the best copy's id for groups that share a name.
    groups.sort();
    groups.into_iter().map(|(_, ids)| ids).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: TrackId, title: &str, artist: &str, seconds: u64) -> Track {
        let text = |text: &str| (!text.is_empty()).then(|| text.to_string());
        Track {
            id,
            title: text(title),
            artist: text(artist),
            length: Duration::from_secs(seconds),
            ..Default::default()
        }
    }

    /// A copy of a song with the facts that tell copies apart.
    fn copy(id: TrackId, bitrate: i32, size: u32, added: u64) -> Track {
        Track { bitrate, size, added, ..song(id, "Same Song", "Same Artist", 200) }
    }

    #[test]
    fn groups_songs_with_the_same_name_and_length_ignoring_case_and_punctuation() {
        let songs = [
            song(1, "I'm Possessed (Gabberhead Remix)", "Cacophony of Sounds", 245),
            song(2, "Im possessed (gabberhead remix)!", "cacophony  of sounds", 246),
            song(3, "Something else", "Cacophony of Sounds", 245),
        ];
        assert_eq!(find_duplicates(&songs), [[1, 2]]);
    }

    #[test]
    fn songs_of_different_lengths_are_different_songs() {
        let name = |id, seconds| song(id, "Love Will Be There", "Gabberhead", seconds);
        // A live version is much longer, a re-encode is a second off.
        assert_eq!(find_duplicates(&[name(1, 245), name(2, 300)]), Vec::<Vec<TrackId>>::new());
        assert_eq!(find_duplicates(&[name(1, 245), name(2, 247)]), [[1, 2]]);
        assert_eq!(find_duplicates(&[name(1, 245), name(2, 249)]), Vec::<Vec<TrackId>>::new());
    }

    #[test]
    fn songs_by_different_artists_are_different_songs() {
        let songs = [song(1, "Intro", "Abe", 60), song(2, "Intro", "Zed", 60)];
        assert!(find_duplicates(&songs).is_empty());
    }

    #[test]
    fn a_song_needs_a_title_and_an_artist_to_count() {
        let songs = [
            song(1, "", "Abe", 60),
            song(2, "", "Abe", 60),
            song(3, "Intro", "", 60),
            song(4, "Intro", "", 60),
            song(5, "!!!", "???", 60),
            song(6, "!!!", "???", 60),
        ];
        assert!(find_duplicates(&songs).is_empty());
    }

    #[test]
    fn puts_the_best_copy_first() {
        // Ids and the order they should come out in: the 320 kbit/s copy first,
        // then the bigger file, then the one that was added earlier, then the id.
        let songs = [
            copy(1, 128, 9_000_000, 10),
            copy(2, 320, 5_000_000, 30),
            copy(3, 320, 6_000_000, 40),
            copy(4, 320, 5_000_000, 20),
            copy(6, 128, 9_000_000, 10),
        ];
        assert_eq!(find_duplicates(&songs), [[3, 4, 2, 1, 6]]);
    }

    #[test]
    fn prefers_a_copy_with_a_cover_over_a_bigger_file() {
        let bigger = copy(1, 320, 6_000_000, 10);
        let with_cover = Track { has_cover: true, ..copy(2, 320, 5_000_000, 20) };
        assert_eq!(find_duplicates(&[bigger.clone(), with_cover.clone()]), [[2, 1]]);
        // ...but a better bit rate still wins over a cover.
        let better_sound = copy(3, 320, 5_000_000, 20);
        let with_cover = Track { bitrate: 192, ..with_cover };
        assert_eq!(find_duplicates(&[better_sound, with_cover]), [[3, 2]]);
    }

    #[test]
    fn finds_three_copies_and_lists_the_groups_alphabetically() {
        let songs = [
            song(1, "Zebra", "Abe", 100),
            song(2, "Apple", "Abe", 100),
            song(3, "Zebra", "Abe", 101),
            song(4, "Apple", "Abe", 100),
            song(5, "Zebra", "Abe", 100),
            song(6, "Lonely", "Abe", 100),
        ];
        assert_eq!(find_duplicates(&songs), [vec![2, 4], vec![1, 3, 5]]);
    }

    #[test]
    fn two_groups_with_the_same_name_stay_apart() {
        // The studio version twice, and the live version twice.
        let songs = [
            song(1, "Song", "Abe", 200),
            song(2, "Song", "Abe", 400),
            song(3, "Song", "Abe", 200),
            song(4, "Song", "Abe", 401),
        ];
        assert_eq!(find_duplicates(&songs), [[1, 3], [2, 4]]);
    }

    // Prints the groups on a real iPod. Only reads from the iPod. Run with:
    // OXIPOD_TEST_IPOD=/run/media/... cargo test lists_possible_duplicates -- --ignored --nocapture
    #[test]
    #[ignore = "needs a mounted iPod, set OXIPOD_TEST_IPOD to its mount point"]
    fn lists_possible_duplicates_on_a_mounted_ipod() {
        use crate::gpod::Database;

        let mount = std::env::var("OXIPOD_TEST_IPOD").expect("OXIPOD_TEST_IPOD is not set");
        let db = Database::open(std::path::Path::new(&mount)).expect("could not open the database");
        let tracks = db.tracks();
        let groups = find_duplicates(&tracks);
        for group in &groups {
            println!();
            for id in group {
                let track = tracks.iter().find(|track| track.id == *id).unwrap();
                println!(
                    "  {id:>5}  {:?} by {:?}  {:?}  {} bytes, {} kbit/s, added {}",
                    track.title, track.artist, track.length, track.size, track.bitrate, track.added
                );
            }
        }
        println!("\n{} groups, {} songs", groups.len(), groups.iter().map(Vec::len).sum::<usize>());
        assert!(groups.iter().all(|group| group.len() >= 2));
    }
}
