# <img src="data/icons/hicolor/scalable/apps/io.github.nyxn3t.Oxipod.svg" width="48" height="48" align="absmiddle" alt=""> Oxipod

Put music on your old iPod from Linux.

I wanted to use my old iPod nano again, but there's no good way to get music
onto it from Linux anymore, so I made this. You plug in your iPod, drop some
music on the window, and that's it.

<p align="center">
  <img src="data/screenshots/songs.png" width="340" alt="The songs on an iPod nano">
  <img src="data/screenshots/search.png" width="340" alt="Searching the songs">
</p>

## What it does

- Add songs or whole album folders by dragging them onto the window (or with
  the + button)
- Converts stuff the iPod can't play (FLAC, Opus, Ogg, WAV, WebM, ...) to MP3
  on the fly. MP3 and AAC/ALAC files are copied as they are
- Copies cover art, either from the file or from a `cover.jpg` in the album
  folder
- Search the songs on the iPod (Ctrl+F) and sort them by title, artist, album
  or when you added them. The list stays quick even with thousands of songs
- Playlists: create them, add songs, remove songs
- Delete songs from the iPod, or select a bunch and delete them or add them
  to a playlist in one go
- Copy songs back to your computer with readable names like
  `Artist - Title.mp3`, one at a time or a whole selection. Handy as a backup
  too
- Cleans up leftover files from syncs that got interrupted
- Ejects the iPod when you're done

## Installing

Grab the `.flatpak` file from the [latest release](https://github.com/nyxn3t/Oxipod/releases/latest)
and install it:

```
flatpak install --user oxipod-*.flatpak
```

It pulls in the GNOME runtime from Flathub if you don't have it yet. Oxipod
isn't on Flathub itself (yet), so updates won't show up automatically: check
the releases page now and then.

## Which iPods work

Oxipod uses [libgpod](https://sourceforge.net/projects/gtkpod/) to talk to the
iPod, so it should work with the models libgpod supports: the iPod classic,
the mini and the nano up to the 4th generation.

So far I've only tested it with an iPod nano 3rd gen. If you try it with
something else, let me know how it goes in an issue.

The touchscreen nanos (6th and 7th gen), the iPod touch and iPhones don't
work.

### First time setup on newer models

The nano 3rd gen and newer need the iPod's unique ID to write a database the
iPod accepts. libgpod normally reads it automatically when you plug the iPod
in, but that doesn't work on most current distros. If your songs don't show up
on the iPod after adding them, run this once with the iPod mounted:

```
sudo ipod-read-sysinfo-extended /dev/sdX /run/media/$USER/YOUR_IPOD
```

(replace `/dev/sdX` with your iPod's drive, `lsblk` shows it). This only adds
a small file to the iPod. I want to make Oxipod do this by itself at some
point.

## Building

You need Rust, GTK 4, libadwaita, libgpod, clang (for the libgpod bindings)
and ffmpeg. On Fedora:

```
sudo dnf install rustup gtk4-devel libadwaita-devel libgpod-devel clang-devel ffmpeg-free
rustup-init
```

Then:

```
cargo run --release
```

There's also a Flatpak manifest in `flatpak/` that builds everything,
including libgpod and a small ffmpeg:

```
flatpak run org.flatpak.Builder --user --install --force-clean flatpak/build flatpak/io.github.nyxn3t.Oxipod.yml
```

## Before you use it

Back up the `iPod_Control` folder on your iPod first. Oxipod has worked fine on
mine, but it's a young app writing to a database format Apple never
documented, so better safe than sorry.

## License

GPL-3.0-or-later, see [LICENSE](LICENSE).
