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
- **Remembers** — restores your directories and SoundFont next time.

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
voxfont [MIDI_DIR] [SOUNDFONT_DIR]
```

- `MIDI_DIR` — starting directory for the left (MIDI) panel.
- `SOUNDFONT_DIR` — starting directory for the right (SoundFont) panel.

Both are optional. The precedence is: command-line argument, then the directory
remembered from the previous session, then `$HOME`.

### Saved session

On exit, voxfont remembers the two panel directories and the loaded SoundFont,
and restores them on the next launch. The state is stored at
`$XDG_CONFIG_HOME/voxfont/state.conf` (default `~/.config/voxfont/state.conf`).

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
