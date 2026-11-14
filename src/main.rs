//! `lan-audio-bridge`: a low-latency LAN audio bridge.
//!
//! One binary, two modes. The host captures system audio and streams 10 ms Opus frames
//! over UDP; the guest receives them, absorbs network jitter, conceals packet loss, and
//! plays the audio back in real time. See the module docs in `host` and `guest` for the
//! threading and buffering model, and `protocol` for the wire format.

mod guest;
mod host;
#[cfg(target_os = "freebsd")]
mod oss;
mod protocol;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Low-latency LAN audio bridge (UDP + Opus).
#[derive(Debug, Parser)]
#[command(name = "lan-audio-bridge", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    mode: Mode,
}

#[derive(Debug, Subcommand)]
enum Mode {
    /// Capture system audio and stream it to a guest.
    Host {
        /// Guest address to stream to, as `IP:PORT` (e.g. 192.168.1.20:5555).
        #[arg(long, value_name = "IP:PORT")]
        target: String,

        /// Capture device name. Defaults to the system default input device.
        #[arg(long, value_name = "NAME")]
        device: Option<String>,

        /// FreeBSD only: capture directly from this OSS device path (e.g.
        /// /dev/dsp0), bypassing cpal/ALSA entirely. Takes precedence over --device.
        #[arg(long, value_name = "PATH")]
        oss_device: Option<String>,
    },

    /// Receive an audio stream and play it back.
    Guest {
        /// Local address to bind, as `IP:PORT` (e.g. 0.0.0.0:5555).
        #[arg(long, value_name = "IP:PORT")]
        bind: String,

        /// Target jitter-buffer depth in milliseconds before playback starts.
        #[arg(long, value_name = "MS", default_value_t = 30)]
        buffer_ms: u32,
    },
}

fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.mode {
        Mode::Host {
            target,
            device,
            oss_device,
        } => host::run(&target, device.as_deref(), oss_device.as_deref()),
        Mode::Guest { bind, buffer_ms } => guest::run(&bind, buffer_ms),
    }
}

/// Initialise logging. Honours `RUST_LOG`; defaults to `info`.
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).with_target(false).init();
}
