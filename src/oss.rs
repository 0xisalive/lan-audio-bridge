//! Direct OSS capture for FreeBSD.
//!
//! Opens an OSS device (e.g. `/dev/dsp0`) and reads interleaved PCM straight from it,
//! bypassing cpal, ALSA, and the ALSA OSS plugin. This is both simpler and more robust
//! on FreeBSD than the cpal -> ALSA -> alsa-plugins-oss path, which negotiates OSS
//! fragment sizes (SNDCTL_DSP_SETFRAGMENT) in ways some OSS devices reject.
//!
//! The device's native format is negotiated (S32 preferred, then S16) and each sample
//! is converted to `f32` so it drops straight into the shared encode-and-send loop.

#![cfg(target_os = "freebsd")]

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::io::AsRawFd;

use anyhow::{anyhow, bail, Context, Result};

use crate::host::PcmSource;
use crate::protocol::{CHANNELS, FRAME_SAMPLES_INTERLEAVED, SAMPLE_RATE};

// C shim entry points (see src/oss_shim.c), linked by build.rs on FreeBSD.
extern "C" {
    fn lanb_afmt_s16_le() -> std::os::raw::c_int;
    fn lanb_afmt_s32_le() -> std::os::raw::c_int;
    fn lanb_set_format(fd: std::os::raw::c_int, fmt: *mut std::os::raw::c_int) -> std::os::raw::c_int;
    fn lanb_get_formats(fd: std::os::raw::c_int, mask: *mut std::os::raw::c_int) -> std::os::raw::c_int;
    fn lanb_set_channels(fd: std::os::raw::c_int, ch: *mut std::os::raw::c_int) -> std::os::raw::c_int;
    fn lanb_set_speed(fd: std::os::raw::c_int, rate: *mut std::os::raw::c_int) -> std::os::raw::c_int;
}

/// The negotiated native sample format of the OSS device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OssFormat {
    S16Le,
    S32Le,
}

/// A blocking OSS capture device.
pub struct OssCapture {
    file: File,
    format: OssFormat,
    /// Raw read buffer sized for exactly one 10 ms frame of the native format.
    raw: Vec<u8>,
}

impl OssCapture {
    /// Open `path`, configure it for 48 kHz stereo capture, and negotiate a native
    /// integer sample format.
    pub fn open(path: &str) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .with_context(|| format!("open {path}"))?;
        let fd = file.as_raw_fd();

        // Negotiate format first: the OSS API requires SETFMT before CHANNELS/SPEED.
        // Prefer S32 (matches the widest common DAC path), fall back to S16.
        let format = negotiate_format(fd)?;

        // Channels: request stereo and require the device to grant it.
        let mut ch: std::os::raw::c_int = CHANNELS as _;
        if unsafe { lanb_set_channels(fd, &mut ch) } != 0 {
            return Err(last_err("SNDCTL_DSP_CHANNELS"));
        }
        if ch != CHANNELS as _ {
            bail!("device granted {ch} channels, need {CHANNELS}");
        }

        // Sample rate: request 48 kHz. OSS may grant a nearby rate; insist on exact so
        // the Opus encoder (fixed at 48 kHz) stays correct.
        let mut rate: std::os::raw::c_int = SAMPLE_RATE as _;
        if unsafe { lanb_set_speed(fd, &mut rate) } != 0 {
            return Err(last_err("SNDCTL_DSP_SPEED"));
        }
        if rate != SAMPLE_RATE as _ {
            bail!("device granted {rate} Hz, need {SAMPLE_RATE}");
        }

        let bytes_per_sample = match format {
            OssFormat::S16Le => 2,
            OssFormat::S32Le => 4,
        };
        let raw = vec![0u8; FRAME_SAMPLES_INTERLEAVED * bytes_per_sample];

        tracing::info!(device = %path, format = ?format, "capturing via direct OSS");
        Ok(Self { file, format, raw })
    }
}

/// Try to set S32_LE, then S16_LE, returning whichever the device accepts.
fn negotiate_format(fd: std::os::raw::c_int) -> Result<OssFormat> {
    let s32 = unsafe { lanb_afmt_s32_le() };
    let s16 = unsafe { lanb_afmt_s16_le() };

    // Query the supported mask for a clearer error if neither works.
    let mut mask: std::os::raw::c_int = 0;
    let have_mask = unsafe { lanb_get_formats(fd, &mut mask) } == 0;

    for (want, fmt) in [(s32, OssFormat::S32Le), (s16, OssFormat::S16Le)] {
        if have_mask && (mask & want) == 0 {
            continue; // Device does not advertise this format; skip the set attempt.
        }
        let mut req = want;
        if unsafe { lanb_set_format(fd, &mut req) } == 0 && req == want {
            return Ok(fmt);
        }
    }
    Err(anyhow!(
        "device supports neither S32_LE nor S16_LE (format mask 0x{:08X})",
        mask
    ))
}

impl PcmSource for OssCapture {
    fn fill(&mut self, out: &mut [f32]) -> usize {
        debug_assert_eq!(out.len(), FRAME_SAMPLES_INTERLEAVED);

        // Blocking read of exactly one frame. read_exact handles short reads by looping;
        // on error (e.g. device closed at shutdown) report a short frame so the sender
        // loop can observe the running flag and exit.
        if let Err(e) = self.file.read_exact(&mut self.raw) {
            tracing::debug!(error = %e, "OSS read ended");
            return 0;
        }

        match self.format {
            OssFormat::S16Le => {
                for (dst, chunk) in out.iter_mut().zip(self.raw.chunks_exact(2)) {
                    let s = i16::from_le_bytes([chunk[0], chunk[1]]);
                    *dst = s as f32 / 32_768.0;
                }
            }
            OssFormat::S32Le => {
                for (dst, chunk) in out.iter_mut().zip(self.raw.chunks_exact(4)) {
                    let s = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                    *dst = s as f32 / 2_147_483_648.0;
                }
            }
        }
        FRAME_SAMPLES_INTERLEAVED
    }
}

/// Build an error from the current errno for a failed ioctl.
fn last_err(what: &str) -> anyhow::Error {
    anyhow!("{what}: {}", std::io::Error::last_os_error())
}
