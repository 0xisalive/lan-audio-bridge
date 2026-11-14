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

## OS audio loopback setup (host)

The host has physical speakers (`/dev/dsp0`); they are simply too quiet. To keep local
playback working *and* capture a copy for the bridge, insert a `virtual_oss` device that
plays out to the real hardware and exposes a loopback monitor the bridge reads from.

### FreeBSD (primary host)

```sh
pkg install virtual_oss

# Create a 48 kHz stereo virtual DSP backed by the real speakers (/dev/dsp0), with a
# loopback monitor the bridge captures from. Audio the browser sends to the virtual
# device is played on the physical speakers and mirrored to vdsp.monitor.
virtual_oss -C 2 -c 2 -r 48000 -b 16 -s 10ms \
  -d dsp0 \
  -c 2 -r 48000 -b 16 -s 10ms -w vdsp.monitor \
  -t vdsp.ctl
```

`-d dsp0` makes the real speakers the playback backend, so you still hear audio locally
(turn the virtual device's volume up with `mixer` or `virtual_oss_cmd` if the hardware
is quiet). `-w vdsp.monitor` creates the capture device. Point applications (Chrome,
Firefox) at the virtual device as their output, then point the bridge at the monitor:

```sh
./target/release/lan-audio-bridge host --target <GUEST_LAN_IP>:5555 --device vdsp.monitor
```

To persist the `virtual_oss` device across reboots, add its flags to `/etc/rc.conf`
(see `virtual_oss(8)` for the `virtual_oss_dsp` rc knobs).

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
