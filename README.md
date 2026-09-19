# voxfont

A console MIDI / SoundFont player with a two-panel interface. The left panel
browses MIDI files (with their durations), the right panel browses SoundFonts
(`.sf2` / `.sf3`, with their sizes). Pick a SoundFont, then play MIDI files
through it. Playback is handled by [FluidSynth](https://www.fluidsynth.org/).

In the MIDI panel a `.zip` archive is browsed just like a directory: step into
it with <kbd>Enter</kbd>, navigate its subfolders, and play files straight from
it (the selected file is extracted to a temporary file behind the scenes).

The two-panel, keyboard-driven interface is inspired by the
[mocp](https://moc.daper.net/) console music player.

```
┌ MIDI files — ~/midi ─────────────────┐┌ SoundFonts — ~/sf2 ──────────────────┐
│   [+] classics/                      ││   [+] banks/                         │
│ ♪ CANYON.MID                    2:08 ││ ♪ CT8MGM.SF2                    8.2M │
│   PASSPORT.MID                  1:17 ││   2MBGMGS.SF2                   2.1M │
│   popcorn.mid                   1:24 ││   AweROMGM.sf2                  1.1M │
│   axelf.mid                     3:02 ││   FluidR3_GM.sf2                148M │
│   entertainer.mid               1:53 ││   RolandSC55.sf2                 32M │
└──────────────────────────────────────┘└──────────────────────────────────────┘
┌ Player ──────────────────────────────────────────────────────────────────────┐
│ ▶ PLAY  CANYON.MID  [00:07/02:08]  ♩5:2 4/4 120BPM  SF: CT8MGM.SF2  Vol 60%  │
│ ████░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░  6% │
└──────────────────────────────────────────────────────────────────────────────┘
```

## Why voxfont

Hear how a MIDI file sounds through different SoundFonts in seconds — no DAW, no
plugins, no mouse.

- **Fast browsing** — arrows, paging, and `/` search; `i` jumps to any folder.
- **Instant A/B** — swap the SoundFont under the playing track with one keypress.
- **At a glance** — durations, sizes, live bar·beat, tempo, and a progress bar.
- **Lightweight** — launches instantly and drives FluidSynth directly.
- **Remembers** — restores your directories and SoundFont next time, and keeps a
  history of what you played through what, replayable with one keypress.
- **Favourites** — star a track-and-SoundFont pairing you like, and play the
  starred list back as a playlist, each entry through its own SoundFont.
- **Playlists** — plain `.m3u` files you can build in voxfont or write by hand,
  where any item can be pinned to a SoundFont of its own.

## Build

Requires a Rust toolchain and the FluidSynth shared library
(`libfluidsynth.so`). No dev headers are needed — the bindings are hand-written.

```sh
cargo build --release
```

If FluidSynth lives somewhere the build script doesn't probe, point it at the
directory containing `libfluidsynth.so`:

```sh
FLUIDSYNTH_LIB_DIR=/path/to/lib cargo build --release
```

## Run

```sh
voxfont [-R <driver>] [--no-history] [-p <playlist>] [MIDI_DIR [SOUNDFONT_DIR]]
```

- `--no-history` — don't read or write the playing history this run.
- `-p`, `--playlist <file>` — open this playlist in the playlist overlay. It is
  shown, not played: press <kbd>Enter</kbd> on an item to start. Without `-p`,
  the playlist that was open at the end of the last session is reopened.
- `-R`, `--driver <driver>` — audio backend, `jack` (default) or `alsa`. When
  given it is used verbatim, with no fallback. When omitted, voxfont uses the
  `VOXFONT_AUDIO_DRIVER` env override if set, otherwise tries jack, pulseaudio,
  then alsa in turn.
- `MIDI_DIR` — starting directory for the left (MIDI) panel.
- `SOUNDFONT_DIR` — starting directory for the right (SoundFont) panel.

The directories are optional. The precedence is: command-line argument, then the
directory remembered from the previous session, then `$HOME`.

Run `voxfont --help` for the full usage summary.

### Saved session

On exit, voxfont remembers the two panel directories, the loaded SoundFont and
the open playlist file, and restores them on the next launch, with the cursor back on the file you
played last. When that file lives inside a `.zip`, the panel steps into the
archive and lands on the file itself rather than stopping at the archive. The
state is stored at `$XDG_CONFIG_HOME/voxfont/state.conf` (default
`~/.config/voxfont/state.conf`).

### Playing history

Every combination you listen to for more than a few seconds is remembered: the
MIDI file, the SoundFont it was heard through, when, and how many times. The
same file played through two SoundFonts is two separate entries — that is the
comparison the history exists to preserve.

Press <kbd>R</kbd> for the history, newest at the top:

```
┌ History — recent · 4 ──────────────────────────────────────────────────────┐
│when              track                    soundfont                  plays │
│ 2026-09-17 18:11 CANYON.MID               CT8MGM.SF2                    3x │
│ 2026-09-17 18:05 CANYON.MID               FluidR3_GM.sf2                1x │
│!2026-09-17 17:23 bwv1041.mid              RolandSC55.sf2                1x │
│ 2026-09-16 17:13 —                        2MBGMGS.SF2                   2x │
│ track  /home/me/midi/CANYON.MID   (2 min ago)                              │
│ font   /srv/sf2/CT8MGM.SF2                                                 │
└ Enter play from here · Tab view · f star · G reveal · d forget · / filter ─┘
```

| Key | Action |
| --- | --- |
| `Enter` | replay: load the font, play the track, move the panels onto it; then play on down the rows |
| `Tab` | switch view: combinations · one row per track · one per SoundFont |
| `G` | point both panels at the entry without playing it |
| `f` | star the row as a favourite (marked `★` in the second column) |
| `d` | forget the selected entry · `D` erase the whole history (asks twice) |
| `/` | filter by track or SoundFont name |
| `Esc` / `q` / `R` | close |

`Enter` makes the rows the queue, exactly as they are shown — in the view and
filter you chose — so **next** mode plays on down them, each through its own
SoundFont. Playing reorders the history, but not the queue: while the history is
the queue, reopening <kbd>R</kbd> shows the queue's rows, and pressing
<kbd>Enter</kbd> again takes a fresh copy. Rows without a track are skipped.
See [Overlays](#overlays) for how the queue relates to the overlay being open.

A row marked `!` can no longer be played, because the file has moved or been
deleted. Entries inside a `.zip` archive are stored as archive plus member, so
they replay straight from the history like any other file.

The history lives beside the session file, in
`$XDG_CONFIG_HOME/voxfont/history.conf` (default `~/.config/voxfont/`), as plain
text you can read, edit or delete. It is never sent anywhere. Start voxfont with
`--no-history` to leave it untouched for a run.

### Favourites and the playlist

A favourite is a **pair**: this track through this SoundFont. That is the
judgement worth keeping — the same tune can be wonderful through one font and
flat through another — so there is no way to star a MIDI file or a SoundFont on
its own. Star the same tune with three fonts and you get three favourites.

Press <kbd>f</kbd> while a track is playing to star what you are hearing, and
again to take the star off. It works on the track that just ended too, so you
don't have to decide before the last note.

The panels mark starred items in the column beside the `♪`:

```
┌ MIDI files — ~/midi ─────────────────┐┌ SoundFonts — ~/sf2 ──────────────────┐
│♪★ CANYON.MID                    2:08 ││♪★ CT8MGM.SF2                    8.2M │
│ ☆ popcorn.mid                   1:24 ││ ☆ RolandSC55.sf2                 32M │
│    axelf.mid                    3:02 ││    2MBGMGS.SF2                  2.1M │
└──────────────────────────────────────┘└──────────────────────────────────────┘
```

`★` means this exact pair is starred; `☆` means the item is starred in some
*other* pairing. Swap the loaded SoundFont and the solid stars move, which shows
at a glance which combinations you have already tried and which are new ground.

Press <kbd>F</kbd> for the list, in playlist order:

```
┌ Favourites · 4 ────────────────────────────────────────────────────────────┐
│starred           track                    soundfont                  plays │
│♪2026-09-17 18:12 CANYON.MID               CT8MGM.SF2                    3x │
│ 2026-09-17 18:14 bwv1041.mid              RolandSC55.sf2                1x │
│ 2026-09-17 19:02 popcorn.mid              AweROMGM.sf2                     │
│!2026-09-16 11:40 axelf.mid                FluidR3_GM.sf2                2x │
│ track  /home/me/midi/CANYON.MID   (2 min ago)                              │
│ font   /srv/sf2/CT8MGM.SF2                                                 │
└ Enter play from here · ⇧↑↓ move · d remove · G reveal · / filter ──────────┘
```

| Key | Action |
| --- | --- |
| `Enter` | play the list from this entry on |
| `Shift`+`↑` `↓` | move the entry up or down the playlist (`K` / `J` too) |
| `G` | point both panels at the entry without playing it |
| `d` or `f` | take the star off |
| `/` | filter by track or SoundFont name |
| `Esc` / `q` / `F` | close |

`Enter` makes the favourites the queue: when a track ends, **next** mode moves
to the next starred pair and loads *its* SoundFont, so a list can walk one tune
through several fonts, or several tunes each through the font that suits it. The
player bar shows how far along the list you are. Entries whose files have gone
(`!`) are skipped rather than dropped — the drive may simply not be mounted.
Pressing <kbd>Enter</kbd> on a file in the MIDI panel hands the queue back to
the directory. The **next** and **repeat** modes behave exactly as they do for a
directory; only the source of "next" differs.

Play counts come from the history rather than being counted again, so a pair you
have starred but never played shows none. The favourites live in
`$XDG_CONFIG_HOME/voxfont/favourites.conf`, in the same readable format, and the
file's order *is* the playlist order. `--no-history` does not touch them:
starring is a deliberate act, not a recording of what happened to play. There is
no "erase all" key — every favourite was starred by hand.

### Playlists

A playlist is an ordinary `.m3u` file. Each item is a MIDI track, and an item
may be **pinned** to a SoundFont of its own or left **unpinned**. An unpinned
item plays through *your font*: the SoundFont you last chose — in the
SoundFont panel, or by replaying a history entry or favourite — or the one
restored from your last session. Fonts that the list loads for pinned items
never change your font, so in

```
1  CANYON.MID    RolandSC55.sf2
2  popcorn.mid   (your font)
```

item 2 plays through whatever you had chosen, not through Roland. Loading a
font by hand while the list plays swaps it under the current track, as always,
and makes it your font from then on.

`.m3u` files show in the MIDI panel with a `≡`. <kbd>Enter</kbd> on one opens
it in the playlist overlay without playing it; <kbd>P</kbd> opens the overlay
for the playlist already open.

```
┌ Playlist — Evening A/B · 4 · modified ─────────────────────────────────────┐
│     # track                          soundfont                       plays │
│ ♪   1 CANYON.MID                     CT8MGM.SF2                         3x │
│     2 CANYON.MID                     RolandSC55.sf2                        │
│     3 bwv1041.mid                    (your font)                        1x │
│!    4 popcorn.mid                    (your font)                           │
│ track  /home/me/midi/CANYON.MID                                            │
│ font   /srv/sf2/CT8MGM.SF2                                                 │
└ Enter play from here · ⇧↑↓ move · S set font · x clear font · d remove … ──┘
```

| Key | Action |
| --- | --- |
| `Enter` | play the list from this item on |
| `Shift`+`↑` `↓` | move the item up or down (`K` / `J` too) |
| `S` | pin the loaded SoundFont to the item |
| `x` | unpin the item, so it plays through your font |
| `d` | remove the item |
| `f` | star the item's track and the font it plays through |
| `G` | point both panels at the item without playing it |
| `w` | save · `W` save as a new file |
| `/` | filter by track or SoundFont name |
| `Esc` / `q` / `P` | close |

To build a list, put the cursor on a MIDI file and press <kbd>a</kbd> to add it
unpinned, or <kbd>A</kbd> to add it pinned to the loaded SoundFont. The cursor
moves on, so pressing it repeatedly adds a run of tracks. Adding with nothing
open starts a new, untitled playlist.

Saving is explicit: the overlay title says `modified` until you press
<kbd>w</kbd>. A new playlist asks for a file name first (`.m3u` is added if you
leave it off), and saving over a different existing file asks for a second
<kbd>Enter</kbd>. Quitting, or opening another playlist, with unsaved changes
asks for a second <kbd>q</kbd> or <kbd>Enter</kbd>.

As with the favourites, playing an item makes the playlist the queue: the
**next** and **repeat** modes step through it, the player bar shows `List 3/12`,
and <kbd>Enter</kbd> on a file in the MIDI panel hands the queue back to the
directory. See [Overlays](#overlays) for how the queue relates to the overlay
being open. Items that cannot play are skipped rather than dropped, whether their
files have gone (marked `!`) or they are unpinned and you have not chosen a font
yet.

Unlike the history and the favourites, whose queues move the panels onto each
entry as it plays, the playlist leaves the panels where they were — in the
folder you opened it from. <kbd>G</kbd> still points them at an item.

#### Playlist file format

The format is extended M3U, so a plain list of MIDI paths from any other tool
opens as it is, and other players read voxfont's playlists and ignore its
additions.

```m3u
#EXTM3U
#PLAYLIST:Evening A/B

# Canyon through two fonts, then two tunes through your font.
#VOXFONT:sf=/srv/sf2/CT8MGM.SF2
~/midi/CANYON.MID
#VOXFONT:sf=RolandSC55.sf2
~/midi/CANYON.MID
classics/bwv1041.mid
/home/me/midi/songs.zip/pop/popcorn.mid
```

- **Encoding.** UTF-8, with or without a byte-order mark. Lines end in LF or
  CRLF. Leading and trailing whitespace on a line is ignored. The extension is
  `.m3u` or `.m3u8`.
- **Header.** `#EXTM3U` is optional when reading; voxfont always writes it.
- **Items.** Every line that is not blank and does not start with `#` is the
  path of one MIDI track (`.mid`, `.midi`, `.kar`, `.rmi`). The file order is
  the play order. The same track may appear any number of times.
- **Pinned items.** `#VOXFONT:sf=<path>` pins a SoundFont (`.sf2`, `.sf3`) to
  the *next* item, and only that one. Blank lines and comments may come
  between the directive and its track. Every item that is pinned carries its
  own directive; there is no setting that carries on to the items after it.
- **Unpinned items.** A track with no directive before it plays through your
  font.
- **Paths.** A path may be absolute, start with `~/` (your home directory), or
  be relative, in which case it is relative to the directory the playlist file
  is in. The same rules apply to the path in `#VOXFONT:sf=`.
- **Files inside zip archives.** Write the path through the archive as if it
  were a directory: `songs.zip/pop/popcorn.mid`. The first component that is an
  existing *file* ending in `.zip` is the archive, and the rest is the member
  inside it. A real directory named `something.zip` stays a directory.
- **Title.** `#PLAYLIST:<title>` names the list in the overlay. Without it, the
  file name is shown.
- **Other lines.** Comments, `#EXTINF` and any other directive voxfont does not
  know are ignored for playback but kept: each stays with the item that follows
  it and moves with that item when you reorder, and comments at the top of the
  file that are followed by a blank line stay at the top. All of them are
  written back when you save.
- **Lines that are not acted on.** A track that is not a MIDI file, a
  `#VOXFONT:sf=` whose path is not a SoundFont, and a `#VOXFONT:sf=` with no
  track after it (because another directive or the end of the file comes
  first) do not stop the file loading. The status line reports how many there
  were, e.g. `2 lines ignored`, and they are kept in the file as written.
- **Missing files.** An item whose track or SoundFont does not exist is still
  loaded, marked `!`, and skipped when playing.

When voxfont saves a playlist, paths inside the playlist's directory tree are
written relative to it, so the directory can be moved as a whole; everything
else is written as an absolute path. A path to a file inside an archive is
written through the archive, and each pinned item gets its own directive line.
The last open playlist is remembered in `state.conf` (see
[Saved session](#saved-session)).

Force a specific audio backend if the default doesn't produce sound:

```sh
VOXFONT_AUDIO_DRIVER=pipewire voxfont ~/midi ~/soundfonts   # or alsa, pulseaudio, jack
```

Quick non-interactive check of the audio/FluidSynth path:

```sh
voxfont --selftest /path/to/font.sf2 /path/to/song.mid
```

### Overlays

The history (<kbd>R</kbd>), the favourites (<kbd>F</kbd>) and the playlist
(<kbd>P</kbd>) are views beside the file panels, not dialogs. An overlay keeps
to the panels' space, leaving the player bar and the key hints in view, and
stays open when you press <kbd>Enter</kbd> on a row. The player keys work inside
it just as they do in the panels: `Space` pauses, `s` stops, `←` `→` `[` `]`
seek, `<` `>` `,` `.` change the volume, `n` and `r` toggle the modes.

A list plays on only while its overlay is open. Close it and the current track
plays to its end, then playback stops; the badge on the player bar (`Hist`,
`Favs` or `List`) dims to say so. Reopen it before then and the list carries on
as if it had never been closed. Reopen it after, and nothing starts by itself,
but the cursor is on the entry that would have played next, so
<kbd>Enter</kbd> picks up from there. Opening a different overlay counts as
closing this one. The directory is the one queue that plays on regardless.

## Keys

| Key | Action |
| --- | --- |
| `Tab` | switch between MIDI / SoundFont panels |
| `↑ ↓` `PgUp` `PgDn` `Home` `End` | move the cursor |
| `Enter` | enter directory · **play** MIDI · **load** SoundFont |
| `U` | go up a directory |
| `Space` / `p` | pause / resume |
| `s` | stop |
| `← →` | seek 5s · `[` `]` seek 30s |
| `n` | toggle **next** mode (auto-play the next file when one ends) |
| `r` | toggle **repeat** mode |
| `<` `>` | volume −1 / +1 · `,` `.` volume −5 / +5 |
| `Alt`+`1`…`9` | set volume 10%…90% |
| `R` | playing history (`Enter` replays the track with its SoundFont, then plays on down the list) |
| `f` | star the track + SoundFont being heard as a favourite |
| `F` | favourites (`Enter` plays the starred list from there on) |
| `P` | playlist (`Enter` plays it from there on, `w` saves) |
| `a` / `A` | add the MIDI file under the cursor to the playlist (`A`: pinned to the loaded SoundFont) |
| `H` | toggle hidden files · `Ctrl`+`r` reload panel |
| `/` or `g` | incremental search in the active panel |
| `h` / `?` | help · `q` / `Q` quit |

A SoundFont must be loaded (right panel → `Enter`) before MIDI playback works.
The **next** and **repeat** modes (shown on the player bar) combine to control
what happens when a track ends:

| next | repeat | behaviour |
| --- | --- | --- |
| off | off | play the file, then stop |
| off | on | loop the current track |
| on | off | play through the directory, then stop |
| on | on | loop the whole directory |

When playback was started from the history (<kbd>R</kbd> → <kbd>Enter</kbd>),
the favourites (<kbd>F</kbd> → <kbd>Enter</kbd>) or the playlist
(<kbd>P</kbd> → <kbd>Enter</kbd>), "the directory" in that table becomes that
list — while its overlay is open (see [Overlays](#overlays)).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

voxfont dynamically links [FluidSynth](https://www.fluidsynth.org/), which is
licensed under the GNU LGPL 2.1. LGPL permits this linking from a permissively
licensed program; FluidSynth itself remains under its own license.
