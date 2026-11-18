# Shredini

Shredini is a Rust music workspace with a native GPUI desktop interface. Play
and monitor a guitar, bass, microphone, MIDI keyboard, or virtual instrument.
Record a performance and take it into Shredini Studio to arrange, edit, mix, and
export a song.

## Play and record

1. Pick a source in **Live**, then choose the driver, input channel, stereo
   output, and MIDI port in **Audio & MIDI**. Monitoring starts when you press
   **Start audio**.
2. Use **Sound design** for Clean, Crunch, and Bass amp/cabinet voicings,
   compression, drive, tone, delay, reverb, and bypass. Changes apply while you
   play. **Vocals** adds semitone pitch shifts, formant color, one harmony voice,
   and vocal presets. Formant color uses a spectral filter approximation.
3. Choose a sampled or synthesized instrument in **Sound library**. **Audition**
   works with a stereo output alone. The library supports SF2 banks and WAV,
   FLAC, MP3, or Ogg/Vorbis keyboard samples with root note, trim, and held looping.
   The same audio formats can be used as backing tracks or imported into Studio.
4. Press **Record**, then **Finish take**. Dry capture and rendered instrument
   audio are stored in separate mono 32-bit float WAVs at the playback rate.
   Studio opens the take with a snapshot of the built-in effects. Incoming
   keyboard notes, control changes, and bends also go into an editable MIDI
   track when notes were recorded. That track starts muted because the rendered
   instrument stem already represents the performance.

Backing playback, metronome, and Studio playback have separate paths from the dry
record tap. Takes are stored under **Takes** in the local data directory. Recording
is bounded to ten minutes or 28.8 million frames per take, whichever comes first.

### Inputs and drivers

A USB guitar cable with its own converter, instrument interface, or audio
interface with a microphone input can provide capture. Shredini selects a mono
channel on the input device and uses a stereo output. Windows uses WASAPI by
default, macOS uses CoreAudio/CoreMIDI, and Linux uses ALSA with the configured
system audio and MIDI endpoints. Independent input and output clocks use
adaptive resampling. Diagnostics show accepted buffer sizes, queue latency,
underruns, dropped input frames, and driver xruns.

For **TONEX ONE**, install the IK driver if using ASIO. Set the pedal to Stomp
mode and bypass its internal processing for dry USB input. Turn off its direct
monitoring to hear the software chain alone. Select TONEX ONE as both input and
output for ASIO, typically input channel 1 at 44.1 kHz. Use WASAPI and select the
available Windows endpoints if the output is a different interface. Start with
128 frames and increase the request if the device underruns.

### Third-party effects

In **Sound design → CLAP insert**, stop audio, choose a trusted local **.clap**
file or macOS bundle, and select an effect from it. The insert accepts one mono
or stereo input bus and one mono or stereo output bus. It runs after the built-in
rack and before the master volume. Shredini adds 64 frames of buffering on that
path plus any latency from the plugin itself.

The host runs the selected effect with its default settings during the session.
The current host scope is one live, headless CLAP audio effect. Plugin windows,
parameter editing, VST/AU, third-party instruments, and Studio plugin rendering
are outside that scope. Dry takes and the built-in effect settings transfer to
Studio.

## Shredini Studio

- **Arrange** by importing audio or MIDI, then selecting a bar or a track in the
  timeline. Audio regions can be moved, duplicated, and trimmed without changing
  their source files. Record over a playing Studio mix by starting an instrument
  input and playing the mix through that output stream.
- **Edit MIDI** in the piano roll. Click a grid cell to add or select a note, then
  adjust its length or velocity. Use the bar and octave controls to move through
  the roll. Standard MIDI File imports use project tempo and the selected track
  instrument.
- **Mix** with per-track gain, pan, mute, editable built-in effects, and
  interpolated gain automation. Set automation points at the playhead.
- **Save** a .shredini project and keep its linked source files. The project
  stores clips, trims, notes, mixer settings, and automation. Media inside the
  project directory can use relative paths. Shredini also keeps a local Studio
  recovery project.
- **Play mix** renders a preview from the playhead. **Export WAV** renders the
  complete mix as a 48 kHz stereo 32-bit float WAV. Both renders run on a worker.
  Projects support up to 64 tracks and a ten-minute render.

Studio is included by default. Use `cargo run --locked --no-default-features`
for the desktop player with recording and without the production workspace.

## Build and run

Use current stable Rust and a native toolchain for the target OS. Run these commands from the
repository root.

```sh
cargo run --locked
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
```

### Windows

Install Visual Studio C++ Build Tools and the Windows SDK for
`x86_64-pc-windows-msvc`. The default build uses WASAPI and native MIDI.
For the optional ASIO build, install desktop LLVM/Clang and use the repository
script. It uses `CPAL_ASIO_DIR` when set or downloads the official Steinberg SDK
to the build directory.

```powershell
.\scripts\build.ps1 -NoAsio -Package
.\scripts\build.ps1 -Package
```

### Linux

Install the compiler, ALSA, GPUI graphics, and input dependencies. For Ubuntu and Debian, install the development packages below.

```sh
sudo apt-get install build-essential clang libclang-dev pkg-config libasound2-dev libudev-dev libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libx11-xcb-dev libxcb-shape0-dev libxcb-xfixes0-dev libfontconfig1-dev libvulkan1
cargo run --locked
python3 scripts/package-native.py
```

GPUI uses Vulkan on Linux. File dialogs use the XDG desktop portal.

### macOS

Install Apple's Command Line Tools with `xcode-select --install`. Build on
Intel or Apple Silicon with `cargo run --locked`. Run
`python3 scripts/package-native.py` to produce a development-signed app bundle
with its microphone usage description.

### Web practice edition

The existing browser practice edition shares the Rust DSP, decoder, and
instruments through a WebAssembly AudioWorklet. Its browser UI provides
monitoring, backing tracks, the sampler, and virtual instruments.

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.129 --locked --root target/tools
python3 scripts/build-web.py
python3 scripts/serve-web.py
```

Open localhost port 8080. Audio device and Web MIDI capabilities depend on the
browser. Local files are decoded in a worker and remain on-device.

## Architecture

| Crate | Responsibility |
| --- | --- |
| `shredini-audio-core` | DSP, effect settings, virtual instruments, MIDI and media |
| `shredini-engine` | CPAL devices, realtime streams, clock correction, CLAP hosting, and bounded take capture |
| `shredini-studio` | Project storage, MIDI import, non-destructive regions, mix rendering, and export |
| `shredini` | Native GPUI workspaces, session persistence, and the separate WASM adapter |

The desktop callback uses preallocated buffers, atomics, and bounded SPSC
queues. Media decoding, file writes, program preparation, and offline rendering
run outside the callback. Driver and third-party plugin behavior depends on
their implementations.

Settings retain the existing **Shredinini** data folder so previously saved
devices, tones, and media paths still load. Set `SHREDINI_DATA_DIR` before
launching to use a different local folder. The CLI supports `--list-devices`,
`--list-midi`, `--probe-midi`, `--keyboard`, and `--setup`.

See [VALIDATION.md](VALIDATION.md) for tested paths and hardware coverage.
