//! Guest mode: receive UDP/Opus packets, absorb network jitter, and play back audio.
//!
//! Two threads cooperate through a lock-free SPSC ring buffer of decoded interleaved
//! f32 PCM:
//!
//! * The **receiver thread** owns the UDP socket and the Opus decoder. It decodes each
//!   packet to PCM, conceals losses it detects from sequence-number gaps using Opus PLC,
//!   and pushes the decoded samples into the ring.
//! * The **cpal output callback** (real-time) only pops PCM out of the ring. It never
//!   decodes, allocates, or locks, so it cannot stall the audio device.
//!
//! The ring doubles as the jitter buffer. Playback is withheld until the ring is
//! pre-filled to the configured target, and drift between the host and guest clocks is
//! absorbed by trimming the ring when it grows past twice the target and by emitting
//! silence when it underruns.

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleRate, StreamConfig};
use opus::{Channels, Decoder};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::HeapRb;

use crate::protocol::{
    Header, CHANNELS, FRAME_SAMPLES_INTERLEAVED, FRAME_SAMPLES_PER_CHANNEL, MAX_DATAGRAM,
    SAMPLE_RATE,
};

/// Receive-socket read timeout, so the receiver loop can observe the shutdown flag even
/// when no packets are arriving.
const RECV_TIMEOUT: Duration = Duration::from_millis(200);

/// Run the guest (receiver) until Ctrl-C.
pub fn run(bind_addr: &str, buffer_ms: u32) -> Result<()> {
    let target_samples = target_samples(buffer_ms);
    // The ring holds several multiples of the target so bursts and drift fit without
    // overflow. Minimum of a few frames guards against tiny buffer_ms values.
    let ring_capacity = (target_samples * 4).max(FRAME_SAMPLES_INTERLEAVED * 8);

    tracing::info!(
        bind = %bind_addr,
        buffer_ms,
        target_samples,
        ring_capacity,
        "starting guest"
    );

    let socket = UdpSocket::bind(bind_addr)
        .with_context(|| format!("binding guest UDP socket to {bind_addr}"))?;
    socket
        .set_read_timeout(Some(RECV_TIMEOUT))
        .context("setting socket read timeout")?;

    let ring = HeapRb::<f32>::new(ring_capacity);
    let (producer, consumer) = ring.split();

    let running = Arc::new(AtomicBool::new(true));
    {
        let running = running.clone();
        ctrlc::set_handler(move || running.store(false, Ordering::SeqCst))
            .context("installing Ctrl-C handler")?;
    }

    // Playback starts suppressed and flips on once the jitter buffer is pre-filled.
    let primed = Arc::new(AtomicBool::new(false));

    let device = default_output_device()?;
    let stream = build_output_stream(&device, consumer, primed.clone(), target_samples)?;
    stream.play().context("starting playback stream")?;

    // The receiver loop runs on this (main) thread until shutdown.
    receive_loop(
        &socket,
        producer,
        primed,
        target_samples,
        running,
    )?;

    tracing::info!("guest shutting down");
    drop(stream);
    Ok(())
}

/// Target jitter-buffer depth in interleaved samples for a given buffer size in ms.
fn target_samples(buffer_ms: u32) -> usize {
    let per_ms = (SAMPLE_RATE as usize * CHANNELS) / 1000;
    (buffer_ms as usize * per_ms).max(FRAME_SAMPLES_INTERLEAVED)
}

/// Receiver + decoder loop. Owns the Opus decoder and pushes PCM into `producer`.
fn receive_loop(
    socket: &UdpSocket,
    mut producer: impl Producer<Item = f32>,
    primed: Arc<AtomicBool>,
    target_samples: usize,
    running: Arc<AtomicBool>,
) -> Result<()> {
    let mut decoder = Decoder::new(SAMPLE_RATE, Channels::Stereo)
        .map_err(|e| anyhow!("creating Opus decoder: {e}"))?;

    let mut datagram = [0u8; MAX_DATAGRAM];
    let mut pcm = vec![0.0f32; FRAME_SAMPLES_INTERLEAVED];
    // `None` until the first packet arrives, so we do not spuriously conceal a gap at
    // startup. Afterwards it tracks the last sequence number we successfully decoded.
    let mut last_seq: Option<u32> = None;

    while running.load(Ordering::Relaxed) {
        let n = match socket.recv(&mut datagram) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => {
                tracing::warn!(error = %e, "recv error");
                continue;
            }
        };

        let (header, opus_payload) = match Header::parse(&datagram[..n]) {
            Ok(parsed) => parsed,
            Err(e) => {
                tracing::debug!(error = %e, "dropping malformed datagram");
                continue;
            }
        };

        // Conceal any packets missing between the last decoded sequence and this one.
        // Only forward gaps are concealed; a packet that arrives late (lower sequence
        // than expected) is discarded, since its slot has already been played.
        if let Some(prev) = last_seq {
            let expected = prev.wrapping_add(1);
            let gap = header.seq_num.wrapping_sub(expected);
            if gap > 0 && gap < MAX_CONCEAL_GAP {
                for _ in 0..gap {
                    conceal_one(&mut decoder, &mut pcm, &mut producer);
                }
            } else if header.seq_num == prev || is_reordered(header.seq_num, expected) {
                tracing::debug!(seq = header.seq_num, "dropping late/reordered packet");
                continue;
            }
        }

        // Decode the real packet.
        match decoder.decode_float(opus_payload, &mut pcm, false) {
            Ok(samples_per_channel) => {
                let total = samples_per_channel * CHANNELS;
                push_pcm(&mut producer, &pcm[..total]);
            }
            Err(e) => {
                tracing::warn!(error = %e, seq = header.seq_num, "decode failed; concealing");
                conceal_one(&mut decoder, &mut pcm, &mut producer);
            }
        }
        last_seq = Some(header.seq_num);

        // Flip to playing once we have buffered the target depth.
        if !primed.load(Ordering::Relaxed) && producer.occupied_len() >= target_samples {
            primed.store(true, Ordering::Release);
            tracing::info!("jitter buffer primed; playback engaged");
        }
    }

    Ok(())
}

/// Largest forward sequence gap we will attempt to conceal. Beyond this we assume a
/// stream discontinuity (e.g. the host restarted) rather than isolated packet loss, and
/// let the next real packet resynchronise playback.
const MAX_CONCEAL_GAP: u32 = 32;

/// Heuristic: treat a sequence that is far *behind* the expected one as reordered rather
/// than a huge forward jump caused by wraparound.
fn is_reordered(seq: u32, expected: u32) -> bool {
    // If the backward distance is smaller than the forward distance, it is a late/
    // reordered packet.
    let forward = seq.wrapping_sub(expected);
    let backward = expected.wrapping_sub(seq);
    backward < forward
}

/// Run one Opus PLC step (decode with an empty payload) and push the concealed frame.
fn conceal_one(
    decoder: &mut Decoder,
    pcm: &mut [f32],
    producer: &mut impl Producer<Item = f32>,
) {
    match decoder.decode_float(&[], pcm, false) {
        Ok(samples_per_channel) => {
            let total = samples_per_channel * CHANNELS;
            push_pcm(producer, &pcm[..total]);
            tracing::debug!("concealed a lost frame via PLC");
        }
        Err(e) => tracing::warn!(error = %e, "PLC decode failed"),
    }
}

/// Push decoded PCM into the ring, dropping any overflow (the output side is behind and
/// drift control will catch up). Returns nothing; overflow is logged sparingly.
fn push_pcm(producer: &mut impl Producer<Item = f32>, pcm: &[f32]) {
    let pushed = producer.push_slice(pcm);
    if pushed < pcm.len() {
        tracing::debug!(dropped = pcm.len() - pushed, "jitter buffer overflow on push");
    }
}

/// Resolve the default output device.
fn default_output_device() -> Result<Device> {
    cpal::default_host()
        .default_output_device()
        .ok_or_else(|| anyhow!("no default output device available"))
}

/// Build the real-time playback stream. The callback only reads from the ring.
fn build_output_stream(
    device: &Device,
    mut consumer: impl Consumer<Item = f32> + Send + 'static,
    primed: Arc<AtomicBool>,
    target_samples: usize,
) -> Result<cpal::Stream> {
    let name = device.name().unwrap_or_else(|_| "<unknown>".into());
    tracing::info!(device = %name, "playback device");

    let config = StreamConfig {
        channels: CHANNELS as u16,
        sample_rate: SampleRate(SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Fixed(FRAME_SAMPLES_PER_CHANNEL as u32),
    };

    // Drift ceiling: if the ring grows past twice the target, the host clock is running
    // faster than ours. Trim the excess so latency stays bounded.
    let high_watermark = target_samples * 2;

    let err_fn = |err| tracing::error!(error = %err, "playback stream error");
    let stream = device
        .build_output_stream(
            &config,
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                // Hold silence until the jitter buffer has been primed.
                if !primed.load(Ordering::Acquire) {
                    out.fill(0.0);
                    return;
                }

                // Drift control: discard the oldest samples when we have drifted long.
                let occupied = consumer.occupied_len();
                if occupied > high_watermark {
                    let excess = occupied - target_samples;
                    // Keep trimming aligned to whole stereo frames so we never split a
                    // left/right pair.
                    let trim = (excess / CHANNELS) * CHANNELS;
                    let _ = consumer.skip(trim);
                    tracing::debug!(trim, "trimmed jitter buffer to curb drift");
                }

                let filled = consumer.pop_slice(out);
                if filled < out.len() {
                    // Underrun: smoothly output silence for the remainder rather than
                    // replaying stale samples or panicking.
                    out[filled..].fill(0.0);
                }
            },
            err_fn,
            None,
        )
        .context("building output stream")?;
    Ok(stream)
}
