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
voxfont [-R <driver>] [--no-history] [MIDI_DIR [SOUNDFONT_DIR]]
```

- `--no-history` — don't read or write the playing history this run.
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

On exit, voxfont remembers the two panel directories and the loaded SoundFont,
and restores them on the next launch, with the cursor back on the file you
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
└ Enter play · Tab view · f star · G reveal · d forget · / filter ───────────┘
```

| Key | Action |
| --- | --- |
| `Enter` | replay: load the font, play the track, move the panels onto it |
| `Tab` | switch view: combinations · one row per track · one per SoundFont |
| `G` | point both panels at the entry without playing it |
| `f` | star the row as a favourite (marked `★` in the second column) |
| `d` | forget the selected entry · `D` erase the whole history (asks twice) |
| `/` | filter by track or SoundFont name |
| `Esc` / `q` | close |

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
| `Esc` / `q` | close |

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

Force a specific audio backend if the default doesn't produce sound:

```sh
VOXFONT_AUDIO_DRIVER=pipewire voxfont ~/midi ~/soundfonts   # or alsa, pulseaudio, jack
```

Quick non-interactive check of the audio/FluidSynth path:

```sh
voxfont --selftest /path/to/font.sf2 /path/to/song.mid
```

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
| `R` | playing history (`Enter` replays the track with its SoundFont) |
| `f` | star the track + SoundFont being heard as a favourite |
| `F` | favourites (`Enter` plays the starred list from there on) |
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

When playback was started from the favourites (<kbd>F</kbd> → <kbd>Enter</kbd>),
"the directory" in that table becomes the starred list.

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
