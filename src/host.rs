//! Host mode: capture system audio, encode it with Opus, and send it over UDP.
//!
//! cpal delivers captured samples on a real-time input callback that must never block
//! or allocate. The callback converts each sample to `f32` (the device's native format
//! may be integer PCM, e.g. the ALSA-over-OSS stack on FreeBSD only offers integer
//! formats) and copies the interleaved result into a lock-free SPSC ring buffer. A
//! dedicated encoder thread drains the ring in exact 10 ms frames, encodes each frame
//! with Opus, prepends the 16-byte wire header, and transmits it as a single UDP
//! datagram.

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, Sample, SampleFormat, SampleRate, SizedSample, StreamConfig};
use opus::{Application, Bitrate, Channels, Encoder};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapRb};

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

    // Opus always encodes f32, but the capture device need not natively produce it.
    // Pick a sample format the device actually supports and convert to f32 in the
    // callback. This is what makes capture work on FreeBSD, where the ALSA-over-OSS
    // stack (virtual_oss) only offers integer PCM formats, never float.
    let sample_format = choose_capture_format(&device);
    tracing::info!(device = %dev_name, format = ?sample_format, "capturing");

    let running = Arc::new(AtomicBool::new(true));
    {
        let running = running.clone();
        ctrlc::set_handler(move || running.store(false, Ordering::SeqCst))
            .context("installing Ctrl-C handler")?;
    }

    // Prefer an exact 10 ms buffer for lowest latency, but some backends (notably the
    // ALSA plug/oss path on FreeBSD) reject a fixed period size and fail hw_params.
    // Fall back to the backend's default buffer so capture still works.
    let (stream, mut consumer) = match build_capture_stream(
        &device,
        sample_format,
        cpal::BufferSize::Fixed(FRAME_SAMPLES_PER_CHANNEL as u32),
    ) {
        Ok(ok) => ok,
        Err(e) => {
            tracing::warn!(error = %e, "fixed 10 ms buffer rejected; retrying with default buffer");
            build_capture_stream(&device, sample_format, cpal::BufferSize::Default)
                .context("building input stream with default buffer")?
        }
    };
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

/// Choose the capture sample format. Prefer `f32` (no conversion), then fall back to
/// the common integer formats, all of which convert losslessly-enough to `f32` for
/// Opus. If the device enumerates no usable config (some ALSA plugin PCMs report an
/// empty set), default to `i16`, the format FreeBSD's OSS stack almost always provides.
fn choose_capture_format(device: &Device) -> SampleFormat {
    // Preference order: native float first, then widest-to-narrowest integer.
    const PREFERRED: [SampleFormat; 4] = [
        SampleFormat::F32,
        SampleFormat::I32,
        SampleFormat::I16,
        SampleFormat::U16,
    ];

    let advertised: Vec<SampleFormat> = match device.supported_input_configs() {
        Ok(configs) => configs
            .filter(|c| {
                c.channels() == CHANNELS as u16
                    && c.min_sample_rate() <= SampleRate(SAMPLE_RATE)
                    && c.max_sample_rate() >= SampleRate(SAMPLE_RATE)
            })
            .map(|c| c.sample_format())
            .collect(),
        Err(e) => {
            tracing::debug!(error = %e, "could not query supported input configs");
            Vec::new()
        }
    };

    for fmt in PREFERRED {
        if advertised.contains(&fmt) {
            return fmt;
        }
    }

    // Nothing usable enumerated. i16 is the safest bet for OSS-backed capture.
    tracing::debug!("no preferred capture format advertised; defaulting to i16");
    SampleFormat::I16
}

/// Build the capture stream for the chosen sample format, converting every sample to
/// `f32` before it enters the ring so the encoder loop only ever sees `f32`.
fn build_capture_stream(
    device: &Device,
    sample_format: SampleFormat,
    buffer_size: cpal::BufferSize,
) -> Result<(cpal::Stream, HeapCons<f32>)> {
    let config = StreamConfig {
        channels: CHANNELS as u16,
        sample_rate: SampleRate(SAMPLE_RATE),
        buffer_size,
    };
    match sample_format {
        SampleFormat::F32 => build_capture_stream_typed::<f32>(device, &config),
        SampleFormat::I32 => build_capture_stream_typed::<i32>(device, &config),
        SampleFormat::I16 => build_capture_stream_typed::<i16>(device, &config),
        SampleFormat::U16 => build_capture_stream_typed::<u16>(device, &config),
        other => Err(anyhow!("unsupported capture sample format {other:?}")),
    }
}

/// Monomorphised capture-stream builder. `T` is the device's native sample type; each
/// sample is converted to `f32` via cpal's `Sample` conversion before being pushed.
/// Owns the SPSC ring and returns the consumer alongside the stream so a failed build
/// drops its own ring cleanly and the caller can retry with different parameters.
fn build_capture_stream_typed<T>(
    device: &Device,
    config: &StreamConfig,
) -> Result<(cpal::Stream, HeapCons<f32>)>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    // Lock-free hand-off: capture callback (producer) -> encoder loop (consumer).
    let ring = HeapRb::<f32>::new(FRAME_SAMPLES_INTERLEAVED * CAPTURE_RING_FRAMES);
    let (mut producer, consumer) = ring.split();

    let err_fn = |err| tracing::error!(error = %err, "capture stream error");
    let stream = device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                // Convert on the real-time thread (cheap: a multiply/clamp per sample)
                // and push into the ring. A fixed stack buffer avoids allocation.
                let mut scratch = [0.0f32; FRAME_SAMPLES_INTERLEAVED];
                let mut written = 0usize;
                let mut dropped = 0usize;
                for &s in data {
                    scratch[written] = f32::from_sample(s);
                    written += 1;
                    if written == scratch.len() {
                        dropped += scratch.len() - producer.push_slice(&scratch);
                        written = 0;
                    }
                }
                if written > 0 {
                    dropped += written - producer.push_slice(&scratch[..written]);
                }
                if dropped > 0 {
                    tracing::warn!(dropped, "capture ring overflow");
                }
            },
            err_fn,
            None,
        )
        .context("building input stream")?;
    Ok((stream, consumer))
}
