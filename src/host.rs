//! Host mode: capture system audio, encode it with Opus, and send it over UDP.
//!
//! cpal delivers captured samples on a real-time input callback that must never block
//! or allocate. The callback therefore only copies interleaved samples into a lock-free
//! SPSC ring buffer. A dedicated encoder thread drains the ring in exact 10 ms frames,
//! encodes each frame with Opus, prepends the 16-byte wire header, and transmits it as
//! a single UDP datagram.

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, SampleRate, StreamConfig};
use opus::{Application, Bitrate, Channels, Encoder};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::HeapRb;

use crate::protocol::{
    Header, CHANNELS, FRAME_SAMPLES_INTERLEAVED, FRAME_SAMPLES_PER_CHANNEL, HEADER_LEN,
    MAX_DATAGRAM, OPUS_BITRATE, SAMPLE_RATE,
};

/// Opus computational-complexity setting (0..=10). Five is the specified trade-off
/// between CPU cost and quality; it is ample for music at 160 kbps.
const OPUS_COMPLEXITY: i32 = 5;

/// Ring-buffer capacity, in frames, between the capture callback and encoder thread.
///
/// Roughly 200 ms of audio, so a transient scheduling stall on the encoder thread does
/// not force the real-time callback to drop samples.
const CAPTURE_RING_FRAMES: usize = 20;

/// How long the encoder thread parks when the ring does not yet hold a full frame.
/// A fraction of a frame keeps latency low without busy-spinning a core.
const ENCODER_IDLE_PARK: Duration = Duration::from_micros(500);

/// Run the host (sender) until Ctrl-C.
pub fn run(target: &str, device_name: Option<&str>) -> Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0").context("binding host UDP socket")?;
    socket
        .connect(target)
        .with_context(|| format!("connecting UDP socket to {target}"))?;
    tracing::info!(%target, "streaming audio to guest");

    let device = select_input_device(device_name)?;
    let dev_name = device.name().unwrap_or_else(|_| "<unknown>".into());
    // Advisory only. Some backends (notably ALSA `plug`/`oss` PCMs on FreeBSD that
    // front a virtual_oss loopback) under-report or omit their supported configs even
    // though the stream opens fine with on-the-fly conversion. Treat a negative result
    // as a warning and let `build_input_stream` be the authority on what actually works.
    if !advertises_required_config(&device) {
        tracing::warn!(
            device = %dev_name,
            "device does not advertise 48 kHz stereo f32 capture; attempting anyway \
             (the backend may convert, e.g. an ALSA plug layer)"
        );
    }
    tracing::info!(device = %dev_name, "capturing");

    let config = StreamConfig {
        channels: CHANNELS as u16,
        sample_rate: SampleRate(SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Fixed(FRAME_SAMPLES_PER_CHANNEL as u32),
    };

    // Lock-free hand-off: capture callback (producer) -> encoder loop (consumer).
    let ring = HeapRb::<f32>::new(FRAME_SAMPLES_INTERLEAVED * CAPTURE_RING_FRAMES);
    let (mut producer, mut consumer) = ring.split();

    let running = Arc::new(AtomicBool::new(true));
    {
        let running = running.clone();
        ctrlc::set_handler(move || running.store(false, Ordering::SeqCst))
            .context("installing Ctrl-C handler")?;
    }

    let err_fn = |err| tracing::error!(error = %err, "capture stream error");
    let stream = device
        .build_input_stream(
            &config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                // Push what fits. If the encoder thread has fallen behind, drop the
                // overflow rather than block the real-time thread.
                let pushed = producer.push_slice(data);
                if pushed < data.len() {
                    tracing::warn!(dropped = data.len() - pushed, "capture ring overflow");
                }
            },
            err_fn,
            None,
        )
        .context("building input stream")?;
    stream.play().context("starting capture stream")?;

    let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::Audio)
        .map_err(|e| anyhow!("creating Opus encoder: {e}"))?;
    encoder
        .set_bitrate(Bitrate::Bits(OPUS_BITRATE))
        .map_err(|e| anyhow!("setting Opus bitrate: {e}"))?;
    encoder
        .set_complexity(OPUS_COMPLEXITY)
        .map_err(|e| anyhow!("setting Opus complexity: {e}"))?;

    let start = Instant::now();
    let mut frame = vec![0.0f32; FRAME_SAMPLES_INTERLEAVED];
    let mut packet = vec![0u8; MAX_DATAGRAM];
    let mut seq_num: u32 = 0;

    // Encoder loop: drain the ring one whole 10 ms frame at a time, encode, and send.
    // Popping only when a full frame is buffered keeps channel interleaving aligned;
    // a short frame would desynchronise left/right from then on.
    while running.load(Ordering::Relaxed) {
        if consumer.occupied_len() < FRAME_SAMPLES_INTERLEAVED {
            thread::sleep(ENCODER_IDLE_PARK);
            continue;
        }

        let got = consumer.pop_slice(&mut frame);
        debug_assert_eq!(got, FRAME_SAMPLES_INTERLEAVED);

        let payload_len = encoder
            .encode_float(&frame, &mut packet[HEADER_LEN..])
            .map_err(|e| anyhow!("Opus encode: {e}"))?;

        Header {
            seq_num,
            timestamp_us: start.elapsed().as_micros() as u64,
        }
        .write_to(&mut packet);

        let datagram = &packet[..HEADER_LEN + payload_len];
        if let Err(e) = socket.send(datagram) {
            tracing::warn!(error = %e, "UDP send failed");
        }

        seq_num = seq_num.wrapping_add(1);
    }

    tracing::info!("host shutting down");
    drop(stream);
    Ok(())
}

/// Resolve the capture device: an explicit name if given, else the default input.
fn select_input_device(device_name: Option<&str>) -> Result<Device> {
    let host = cpal::default_host();
    match device_name {
        Some(name) => host
            .input_devices()
            .context("enumerating input devices")?
            .find(|d| d.name().map(|n| n == name).unwrap_or(false))
            .ok_or_else(|| anyhow!("no input device named '{name}'")),
        None => host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input device available")),
    }
}

/// Report whether the device *advertises* the required 48 kHz / stereo / f32 capture
/// format. This is only a hint: a `false` result does not mean capture will fail, since
/// a backend may convert formats transparently and may not enumerate every config it
/// can satisfy. Any query error is treated as "unknown" (returns `false`).
fn advertises_required_config(device: &Device) -> bool {
    match device.supported_input_configs() {
        Ok(mut configs) => configs.any(|c| {
            c.channels() == CHANNELS as u16
                && c.sample_format() == SampleFormat::F32
                && c.min_sample_rate() <= SampleRate(SAMPLE_RATE)
                && c.max_sample_rate() >= SampleRate(SAMPLE_RATE)
        }),
        Err(e) => {
            tracing::debug!(error = %e, "could not query supported input configs");
            false
        }
    }
}
