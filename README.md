# lan-audio-bridge

A lightweight, low-latency (< 50 ms) client-server audio bridge written in Rust. It
captures live browser/system audio on a **Host** machine (**FreeBSD** or **Linux**) and
streams it over a local network (UDP/Opus) to a **Guest** machine (**macOS** or
**Linux**) for real-time, lip-synced playback. The host can keep playing audio on its
own speakers while a copy is streamed to the guest.

## Architecture

Rather than demuxing MP4/WebM containers or fighting browser DRM/MSE fragmentation,
`lan-audio-bridge` captures the decoded PCM stream at the OS virtual-sink level,
compresses it into 10 ms Opus frames, and transmits it over UDP.

```text
[HOST: FreeBSD / Linux]                          [GUEST: macOS / Linux]
  Chrome / Firefox                                 CoreAudio / PipeWire
    -> decoded PCM                                   ^ low-latency callback
  OS loopback / virtual sink                       Adaptive jitter buffer (SPSC ring)
    -> capture thread (48 kHz)                       ^ Opus decoder + PLC
  Opus encoder (10 ms frame)      LAN / UDP          ^ UDP receiver
    -> UDP sender             ===============>      (~1-5 ms network transit)
```

The host's cpal capture callback copies raw samples into a lock-free SPSC ring; an
encoder thread drains exact 10 ms frames, encodes them, and sends one Opus packet per
UDP datagram. The guest's receiver thread owns the decoder, conceals losses from
sequence-number gaps with Opus PLC, and pushes decoded PCM into a second SPSC ring that
the real-time playback callback drains. The ring doubles as the jitter buffer.

## Technical specifications

- **Audio format:** 48 000 Hz, 2 channels (stereo), `f32` interleaved PCM.
- **Frame size:** 10 ms (480 samples per channel, 960 interleaved).
- **Codec:** Opus (`libopus`, `Application::Audio`, 160 kbps, complexity 5).
- **Transport:** raw UDP with a 16-byte big-endian header, one Opus packet per datagram,
  kept under 1024 bytes to avoid IP fragmentation:
  - `magic` (`u32`): `0x4C414E41` ("LANA")
  - `seq_num` (`u32`): monotonic sequence number (loss and reorder detection)
  - `timestamp_us` (`u64`): host monotonic capture timestamp in microseconds
- **Jitter and drift control (guest):**
  - Lock-free SPSC ring buffer targeting the configured depth (default 30 ms).
  - Opus Packet Loss Concealment (PLC) on missing sequence numbers.
  - Playback withheld until the buffer is pre-filled to the target.
  - Trims the buffer when it grows past 2x the target (host clock faster) and emits
    smooth silence on underrun (guest clock faster), never panicking.

## Build

```sh
cargo build --release
```

### Dependencies

`libopus` is built from source and linked statically by the `opus`/`opusic-sys` crates,
so no system libopus package is required. Audio I/O uses `cpal`:

- **Linux / FreeBSD:** ALSA. Install the development package, e.g.
  `sudo apt install libasound2-dev pkg-config` (Debian/Ubuntu) or
  `pkg install alsa-lib pkgconf` (FreeBSD).
- **macOS:** CoreAudio, part of the system SDK. No extra packages needed.

Building the vendored libopus also needs a C compiler and CMake. On FreeBSD this is not
always present in the base system; see [Toolchain on FreeBSD](#toolchain-on-freebsd).

### Toolchain on FreeBSD

The build needs a C compiler and linker (for the vendored libopus and the final Rust
link step), CMake, `pkgconf`, and the Rust toolchain:

```sh
pkg install rust cmake pkgconf alsa-lib
```

**If the build fails with "no C compiler found", a linker error, or CMake cannot find a
compiler, your install has no base-system Clang.** A normal FreeBSD RELEASE ships Clang
as `/usr/bin/cc`, but minimal, jail, cloud, and CI images are often built without it.
Check and, if missing, install LLVM from packages:

```sh
# Is a compiler present?
cc --version || clang --version

# If neither is found, install the LLVM toolchain (clang, lld, ar, ranlib).
pkg install llvm
```

`pkg install llvm` provides versioned binaries such as `clang19`/`ld.lld19`. If `cc` is
still not on `PATH` afterwards, point the build at the installed compiler explicitly so
both the `cc` crate (libopus) and CMake use it:

```sh
export CC=clang
export CXX=clang++
export CMAKE_C_COMPILER=clang
cargo build --release
```

Once a compiler, `cmake`, `pkgconf`, `alsa-lib`, and `rust` are all present, build as
usual:

```sh
cargo build --release
```

## Audio capture setup (host)

### FreeBSD (primary host) — direct OSS capture (recommended)

If the audio you want is already present on an OSS device (for example `/dev/dsp0`, a
line-in, a loopback, or a card that mixes playback into its record stream), capture it
directly. This bypasses cpal, ALSA, and the ALSA OSS plugin entirely:

```sh
./target/release/lan-audio-bridge host --target <GUEST_LAN_IP>:5555 --oss-device /dev/dsp0
```

`--oss-device` opens the device, negotiates 48 kHz stereo at the device's native format
(S32 or S16), and converts to `f32` internally. No `virtual_oss`, no `cuse`, no
`asound.conf`, no `alsa-plugins`. Confirm the device carries the audio you expect first:

```sh
cat /dev/sndstat                              # list devices (play/rec)
dd if=/dev/dsp0 of=/tmp/raw.pcm bs=4096 count=140   # ~3 s; non-empty => capturable
```

### FreeBSD — capturing played-back audio with virtual_oss (fallback)

Use this only if you need to capture what is being *played* (the speaker signal) and the
hardware does not expose it as a record stream. It inserts a `virtual_oss` device that
plays to the real speakers while mirroring to a capture monitor, then wires that monitor
into ALSA. It involves `cuse`, `virtual_oss`, and an ALSA-over-OSS config, with device
naming collisions and format/fragment negotiation to get right, so a script automates it.
Run it as root:

```sh
sh scripts/freebsd-setup.sh
```

It installs the ALSA packages it can (`alsa-lib`, `alsa-plugins`, `alsa-utils`), loads
and persists `cuse`, starts a `virtual_oss` device that plays out to your speakers while
mirroring to a capture monitor, writes `/usr/local/etc/asound.conf` to point ALSA's
default capture at that monitor, and verifies capture works.

`virtual_oss` itself is **not** installed by the script, since it is not reliably a
`pkg` on every system. Install it first from ports (`audio/virtual_oss`) or your
preferred source; the script checks that the `virtual_oss` binary is on `PATH` and stops
with a clear message if it is missing.

Then:

1. Set your browser/application audio **output** to `/dev/vdsp` (you still hear it on
   the real speakers; raise the level with `mixer` if it is quiet).
2. Run the host with no `--device` (it uses the ALSA default, now the monitor):

   ```sh
   ./target/release/lan-audio-bridge host --target <GUEST_LAN_IP>:5555
   ```

The script is configurable through environment variables (see the comments at the top),
e.g. `SPEAKER_DSP=/dev/dsp1 sh scripts/freebsd-setup.sh` if your hardware is not `dsp0`.
Undo everything with `sh scripts/freebsd-teardown.sh`.

Why no `--device <name>`: cpal on FreeBSD uses the ALSA backend, and ALSA device
enumeration does not surface a raw `virtual_oss` node, so `--device` cannot find it by
name. Routing the node through ALSA's `default` PCM (what the script does) is the
reliable path, and the bridge captures in the device's native integer format and
converts to `f32` internally.

### Linux (secondary host)

To keep local playback and capture a copy, tee audio to both the real speakers and a
null sink using a `module-combine-sink` (PulseAudio/PipeWire-pulse):

```sh
sudo apt install libasound2-dev pkg-config

# A null sink whose .monitor the bridge captures from.
pactl load-module module-null-sink sink_name=lan_out \
  sink_properties=device.description="LAN_Audio_Out"

# Fan out to the real speakers and the null sink, so you still hear audio locally.
# Replace alsa_output... with your hardware sink from `pactl list short sinks`.
pactl load-module module-combine-sink sink_name=lan_tee \
  slaves=alsa_output.pci-0000_00_1f.3.analog-stereo,lan_out \
  sink_properties=device.description="LAN_Tee"
pactl set-default-sink lan_tee
```

Point the host at the null sink's monitor:

```sh
./target/release/lan-audio-bridge host --target <GUEST_LAN_IP>:5555 --device lan_out.monitor
```

If you do not need local playback, skip the combine sink and just set `lan_out` as the
default; the bridge still reads `lan_out.monitor`.

### macOS (primary guest)

No capture setup is needed on the guest; it only plays back.

## Usage

```sh
# 1. Start the guest (receiver) on macOS / Linux.
./target/release/lan-audio-bridge guest --bind 0.0.0.0:5555 --buffer-ms 30

# 2. Start the host (sender) on FreeBSD / Linux.
./target/release/lan-audio-bridge host --target <GUEST_LAN_IP>:5555

# Optionally select a specific capture device (the loopback monitor set up above).
./target/release/lan-audio-bridge host --target <GUEST_LAN_IP>:5555 --device vdsp.monitor
```

List available devices by starting the host with a bogus `--device` name; the error
names the default device, or set `RUST_LOG=debug` for verbose diagnostics.

## Logging

Logging is controlled by `RUST_LOG` (default `info`), e.g. `RUST_LOG=debug` to see
packet-loss concealment and drift-trim events.

## License

Licensed under either of Apache License, Version 2.0 or MIT license at your option.
