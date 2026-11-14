# lan-audio-bridge

A lightweight, low-latency (< 50 ms) client-server audio bridge written in Rust. It
captures live browser/system audio on a headless **Host** machine (**FreeBSD** or
**Linux**) and streams it over a local network (UDP/Opus) to a **Guest** machine
(**macOS** or **Linux**) for real-time, lip-synced playback.

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

A C toolchain and CMake are required to build the vendored libopus.

## OS audio loopback setup (host)

The host usually has no physical speakers, so create a dummy/loopback sink that the
browser outputs to and that `lan-audio-bridge` captures from.

### FreeBSD (primary host)

```sh
pkg install virtual_oss opus rust

# 48 kHz stereo virtual device (dsp0) backed by /dev/null, with a loopback monitor.
virtual_oss -C 2 -c 2 -r 48000 -b 16 -s 10ms -f /dev/null -d dsp0 -w dsp0.monitor -t vdsp.ctl
```

Point the host at `dsp0.monitor` via `--device`.

### Linux (secondary host)

```sh
sudo apt install libopus-dev libasound2-dev pkg-config  # or use the vendored libopus

pactl load-module module-null-sink sink_name=lan_out \
  sink_properties=device.description="LAN_Audio_Out"
pactl set-default-sink lan_out
```

Point the host at `lan_out.monitor` via `--device`.

### macOS (primary guest)

No capture setup is needed on the guest; it only plays back.

## Usage

```sh
# 1. Start the guest (receiver) on macOS / Linux.
./target/release/lan-audio-bridge guest --bind 0.0.0.0:5555 --buffer-ms 30

# 2. Start the host (sender) on FreeBSD / Linux.
./target/release/lan-audio-bridge host --target <GUEST_LAN_IP>:5555

# Optionally select a specific capture device.
./target/release/lan-audio-bridge host --target <GUEST_LAN_IP>:5555 --device dsp0.monitor
```

List available devices by starting the host with a bogus `--device` name; the error
names the default device, or set `RUST_LOG=debug` for verbose diagnostics.

## Logging

Logging is controlled by `RUST_LOG` (default `info`), e.g. `RUST_LOG=debug` to see
packet-loss concealment and drift-trim events.

## License

Licensed under either of Apache License, Version 2.0 or MIT license at your option.
