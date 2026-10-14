//! Getting music files ready for the iPod: reading their tags and cover art
//! with ffprobe, and converting formats the iPod can't play with ffmpeg.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

use crate::gpod::{FileType, NewTrack};

/// Bitrate for files we convert to MP3.
const MP3_BITRATE_KBPS: i32 = 320;

/// The iPod plays MP3 and AAC/ALAC up to 48 kHz.
const MAX_SAMPLE_RATE: u32 = 48_000;

/// Cover art is scaled down to at most this size; the iPod shows it smaller.
const COVER_SIZE: u32 = 320;

/// File extensions we try to import. Anything ffmpeg can read would work, but
/// this keeps images, playlists etc. out when a whole folder is dropped.
const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "m4a", "m4b", "aac", "alac", "flac", "ogg", "oga", "opus", "wav", "aif", "aiff",
    "wma", "ape", "wv", "mka", "webm", "mkv", "mp4",
];

/// Names of image files used as cover art for a whole folder (album).
const FOLDER_COVERS: &[&str] = &["cover", "folder", "front", "album", "albumart"];

/// A file ready to be copied to the iPod.
pub struct Prepared {
    /// The file to copy: the original, or a converted copy in a temp folder.
    pub file: PathBuf,
    pub track: NewTrack,
    /// Deletes the temp folder (and the converted copy) when `Prepared` is
    /// dropped. The leading underscore says it's only kept for that.
    _temp_dir: Option<tempfile::TempDir>,
}

#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

// `From` lets `?` convert io errors into our `Error` automatically.
impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error(error.to_string())
    }
}

/// Turns dropped or picked paths into the list of music files to import:
/// folders are searched recursively, other files are kept if they look like
/// audio. The result is sorted, so albums are added in track order.
pub fn music_files(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut todo = paths;
    // A loop with a to-do list instead of recursion: `pop` takes the last
    // path, and folders push their contents back onto the list.
    while let Some(path) = todo.pop() {
        if path.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&path) {
                todo.extend(entries.filter_map(|entry| entry.ok()).map(|entry| entry.path()));
            }
        } else if has_extension(&path, AUDIO_EXTENSIONS) {
            files.push(path);
        }
    }
    files.sort();
    files
}

/// Reads the file's tags and makes sure the iPod can play it, converting it
/// to MP3 if needed. This runs ffmpeg, so it can take a few seconds: call it
/// off the main thread.
pub fn prepare(source: &Path) -> Result<Prepared, Error> {
    let probe = probe(source)?;
    let audio = probe
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("audio"))
        .ok_or_else(|| Error("this file has no audio".into()))?;

    let codec = audio.codec_name.as_deref().unwrap_or_default();
    let container = probe.format.format_name.as_deref().unwrap_or_default();
    let sample_rate = parse_number::<u32>(&audio.sample_rate).unwrap_or(44_100);

    // m4a files report the container as "mov,mp4,m4a,3gp,3g2,mj2".
    let playable_type = match codec {
        "mp3" if container == "mp3" => Some(FileType::Mp3),
        "aac" if container.contains("mp4") => Some(FileType::Aac),
        "alac" if container.contains("mp4") => Some(FileType::Alac),
        _ => None,
    };

    let mut track = Tags::new(&probe.format.tags, &audio.tags).into_new_track(source);
    track.length = parse_number::<f64>(&probe.format.duration)
        .map(Duration::from_secs_f64)
        .unwrap_or_default();
    track.cover = embedded_cover(source, &probe.streams).or_else(|| folder_cover(source));

    if let Some(file_type) = playable_type.filter(|_| sample_rate <= MAX_SAMPLE_RATE) {
        let bitrate = parse_number::<i32>(&audio.bit_rate)
            .or_else(|| parse_number::<i32>(&probe.format.bit_rate))
            .unwrap_or(0);
        track.file_type = file_type;
        track.sample_rate = sample_rate as u16;
        track.bitrate_kbps = bitrate / 1000;
        return Ok(Prepared { file: source.to_path_buf(), track, _temp_dir: None });
    }

    // Everything else is converted. Keep common sample rates, resample the rest.
    let output_rate = if matches!(sample_rate, 32_000 | 44_100 | 48_000) { sample_rate } else { 44_100 };
    let temp_dir = tempfile::Builder::new().prefix("oxipod-").tempdir()?;
    let output = temp_dir.path().join("track.mp3");
    run_ffmpeg(&[
        "-i".as_ref(),
        source.as_os_str(),
        "-map".as_ref(),
        "0:a:0".as_ref(),
        "-c:a".as_ref(),
        "libmp3lame".as_ref(),
        "-b:a".as_ref(),
        format!("{MP3_BITRATE_KBPS}k").as_ref(),
        "-ar".as_ref(),
        output_rate.to_string().as_ref(),
        "-id3v2_version".as_ref(),
        "3".as_ref(),
        output.as_os_str(),
    ])?;

    track.file_type = FileType::Mp3;
    track.sample_rate = output_rate as u16;
    track.bitrate_kbps = MP3_BITRATE_KBPS;
    Ok(Prepared { file: output, track, _temp_dir: Some(temp_dir) })
}

// What we read from `ffprobe -print_format json`. serde fills these structs
// from the JSON; fields that are missing in the JSON become None or empty.
#[derive(Deserialize)]
struct Probe {
    #[serde(default)]
    streams: Vec<Stream>,
    format: Format,
}

#[derive(Deserialize)]
struct Stream {
    index: usize,
    codec_type: Option<String>,
    codec_name: Option<String>,
    sample_rate: Option<String>,
    bit_rate: Option<String>,
    #[serde(default)]
    disposition: HashMap<String, i64>,
    #[serde(default)]
    tags: HashMap<String, String>,
}

#[derive(Deserialize)]
struct Format {
    format_name: Option<String>,
    duration: Option<String>,
    bit_rate: Option<String>,
    #[serde(default)]
    tags: HashMap<String, String>,
}

fn probe(source: &Path) -> Result<Probe, Error> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(source)
        .output()
        .map_err(|error| missing_ffmpeg(error, "ffprobe"))?;
    if !output.status.success() {
        return Err(Error(format!(
            "ffprobe can't read this file: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| Error(format!("unexpected ffprobe output: {error}")))
}

/// Runs ffmpeg with `args` and returns what it wrote to stdout.
fn run_ffmpeg(args: &[&std::ffi::OsStr]) -> Result<Vec<u8>, Error> {
    let output = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y"])
        .args(args)
        .output()
        .map_err(|error| missing_ffmpeg(error, "ffmpeg"))?;
    if !output.status.success() {
        return Err(Error(format!(
            "ffmpeg failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

fn missing_ffmpeg(error: std::io::Error, program: &str) -> Error {
    if error.kind() == std::io::ErrorKind::NotFound {
        Error(format!("{program} isn't installed (it comes with ffmpeg)"))
    } else {
        Error(format!("can't run {program}: {error}"))
    }
}

/// Cover art stored inside the file, as a JPEG of at most COVER_SIZE pixels.
fn embedded_cover(source: &Path, streams: &[Stream]) -> Option<Vec<u8>> {
    // Cover art shows up as a video stream marked "attached_pic". A real
    // video stream (like a music video in a .webm) is not used as a cover.
    let cover = streams
        .iter()
        .find(|stream| stream.disposition.get("attached_pic") == Some(&1))?;
    let scale = format!(
        "scale='min({COVER_SIZE},iw)':'min({COVER_SIZE},ih)':force_original_aspect_ratio=decrease"
    );
    let jpeg = run_ffmpeg(&[
        "-i".as_ref(),
        source.as_os_str(),
        "-map".as_ref(),
        format!("0:{}", cover.index).as_ref(),
        "-frames:v".as_ref(),
        "1".as_ref(),
        "-vf".as_ref(),
        scale.as_ref(),
        "-c:v".as_ref(),
        "mjpeg".as_ref(),
        "-f".as_ref(),
        "image2".as_ref(),
        "pipe:1".as_ref(),
    ])
    .ok()?;
    (!jpeg.is_empty()).then_some(jpeg)
}

/// A cover.jpg, folder.png etc. next to the file.
fn folder_cover(source: &Path) -> Option<Vec<u8>> {
    let folder = source.parent()?;
    std::fs::read_dir(folder)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| {
            let stem = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default();
            FOLDER_COVERS.contains(&stem.to_lowercase().as_str())
                && has_extension(path, &["jpg", "jpeg", "png"])
        })
        .and_then(|path| std::fs::read(path).ok())
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extensions.contains(&extension.to_lowercase().as_str()))
}

/// Parses an optional number that ffprobe gives as text, like "44100".
/// `T: FromStr` means: any type that can be parsed from a string.
fn parse_number<T: std::str::FromStr>(text: &Option<String>) -> Option<T> {
    text.as_deref()?.parse().ok()
}

/// The tags of a file, with lowercase keys.
struct Tags(HashMap<String, String>);

impl Tags {
    /// Tags can be on the file (MP3, M4A, FLAC) or on the audio stream (Ogg,
    /// Opus); stream tags win when both have the same key.
    fn new(format_tags: &HashMap<String, String>, stream_tags: &HashMap<String, String>) -> Self {
        let tags = format_tags
            .iter()
            .chain(stream_tags)
            .map(|(key, value)| (key.to_lowercase(), value.trim().to_string()))
            .filter(|(_, value)| !value.is_empty())
            .collect();
        Tags(tags)
    }

    fn get(&self, keys: &[&str]) -> Option<String> {
        keys.iter().find_map(|key| self.0.get(*key).cloned())
    }

    /// Reads "3/12" style tags (track 3 of 12) as (3, 12).
    fn number_pair(&self, keys: &[&str]) -> (i32, i32) {
        let Some(text) = self.get(keys) else {
            return (0, 0);
        };
        let mut parts = text.split('/').map(|part| part.trim().parse().unwrap_or(0));
        (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
    }

    fn into_new_track(self, source: &Path) -> NewTrack {
        let mut title = self.get(&["title"]);
        let mut artist = self.get(&["artist", "album_artist", "albumartist"]);

        // No title tag: use the file name, without a YouTube id like
        // " [Fj6iySe2DJo]" that yt-dlp adds.
        if title.is_none() {
            let stem = source.file_stem().unwrap_or_default().to_string_lossy();
            title = Some(strip_video_id(&stem).to_string());
        }
        // "Artist - Title" without an artist tag: split it.
        if artist.is_none()
            && let Some((name, song)) = title.as_deref().and_then(|title| title.split_once(" - "))
        {
            artist = Some(name.trim().to_string());
            title = Some(song.trim().to_string());
        }

        let (track_number, track_count) = self.number_pair(&["track", "tracknumber"]);
        let (disc_number, disc_count) = self.number_pair(&["disc", "discnumber"]);
        let year = self
            .get(&["date", "year", "originaldate"])
            .and_then(|date| date.get(..4)?.parse().ok())
            .unwrap_or(0);

        NewTrack {
            title: title.unwrap_or_default(),
            artist,
            album: self.get(&["album"]),
            album_artist: self.get(&["album_artist", "albumartist"]),
            genre: self.get(&["genre"]),
            track_number,
            track_count: if track_count == 0 { parse_or_zero(self.get(&["tracktotal", "totaltracks"])) } else { track_count },
            disc_number,
            disc_count: if disc_count == 0 { parse_or_zero(self.get(&["disctotal", "totaldiscs"])) } else { disc_count },
            year,
            ..NewTrack::default()
        }
    }
}

fn parse_or_zero(text: Option<String>) -> i32 {
    text.and_then(|text| text.parse().ok()).unwrap_or(0)
}

/// "Song [Fj6iySe2DJo]" → "Song". Only an 11-character YouTube-style id in
/// square brackets at the end is removed.
fn strip_video_id(name: &str) -> &str {
    if let Some(start) = name.rfind(" [")
        && let Some(id) = name[start + 2..].strip_suffix(']')
        && id.len() == 11
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return &name[..start];
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(pairs: &[(&str, &str)]) -> Tags {
        let map = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Tags::new(&map, &HashMap::new())
    }

    #[test]
    fn splits_artist_and_title_from_a_youtube_file_name() {
        let track = tags(&[]).into_new_track(Path::new(
            "/home/x/Dr. Peacock & Pinotello - Trip to the Arcade [Fj6iySe2DJo].webm",
        ));
        assert_eq!(track.title, "Trip to the Arcade");
        assert_eq!(track.artist.as_deref(), Some("Dr. Peacock & Pinotello"));
    }

    #[test]
    fn keeps_brackets_that_are_not_a_video_id() {
        assert_eq!(strip_video_id("Song [Live]"), "Song [Live]");
    }

    #[test]
    fn reads_track_and_disc_numbers() {
        let track = tags(&[("TITLE", "x"), ("track", "3/12"), ("disc", "1"), ("DATE", "2019-05-01")])
            .into_new_track(Path::new("x.flac"));
        assert_eq!((track.track_number, track.track_count), (3, 12));
        assert_eq!((track.disc_number, track.disc_count), (1, 0));
        assert_eq!(track.year, 2019);
    }

    #[test]
    #[ignore = "needs ffmpeg and the test file in the home folder"]
    fn converts_the_webm_from_youtube() {
        let home = std::env::var("HOME").unwrap();
        let source = Path::new(&home)
            .join("Dr. Peacock & Pinotello - Trip to the Arcade [Fj6iySe2DJo].webm");
        let prepared = prepare(&source).expect("prepare failed");
        println!("{:?} {:?}", prepared.file, prepared.track);
        assert_eq!(prepared.track.file_type, FileType::Mp3);
        assert!(prepared.file.exists());
    }
}
