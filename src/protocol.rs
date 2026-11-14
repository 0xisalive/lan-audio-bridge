//! Wire protocol and shared audio constants.
//!
//! Every datagram carries a fixed 16-byte big-endian header followed by a single
//! Opus packet. Keeping one Opus frame per datagram bounds the blast radius of a lost
//! packet to a single 10 ms frame, which the guest conceals with Opus PLC.

use anyhow::{bail, Result};

/// Sample rate of the capture and playback streams, in Hz.
///
/// 48 kHz is the native rate of Opus; using it avoids an intermediate resampling stage
/// on either end.
pub const SAMPLE_RATE: u32 = 48_000;

/// Channel count. The bridge always runs in stereo.
pub const CHANNELS: usize = 2;

/// Duration of a single Opus frame, in milliseconds.
pub const FRAME_MS: u32 = 10;

/// Samples per channel in one frame: 48_000 Hz * 10 ms = 480.
pub const FRAME_SAMPLES_PER_CHANNEL: usize = (SAMPLE_RATE as usize * FRAME_MS as usize) / 1000;

/// Interleaved f32 samples in one frame across all channels (480 * 2 = 960).
pub const FRAME_SAMPLES_INTERLEAVED: usize = FRAME_SAMPLES_PER_CHANNEL * CHANNELS;

/// Target Opus bitrate, in bits per second.
pub const OPUS_BITRATE: i32 = 160_000;

/// Magic identifier: the ASCII bytes "LANA" read as a big-endian u32.
pub const MAGIC: u32 = 0x4C41_4E41;

/// Size of the wire header in bytes: magic(4) + seq(4) + timestamp(8).
pub const HEADER_LEN: usize = 16;

/// Largest datagram we are willing to emit. Kept well under a typical 1500-byte MTU so
/// a packet never fragments at the IP layer. A 10 ms stereo frame at 160 kbps encodes
/// to roughly 200 bytes, so this is comfortable headroom.
pub const MAX_DATAGRAM: usize = 1024;

/// The 16-byte frame header prepended to every Opus packet.
///
/// Layout (all big-endian):
/// ```text
/// offset 0  : magic         u32   0x4C414E41 ("LANA")
/// offset 4  : seq_num       u32   monotonic, wraps at u32::MAX
/// offset 8  : timestamp_us  u64   host monotonic capture time, microseconds
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub seq_num: u32,
    pub timestamp_us: u64,
}

impl Header {
    /// Serialise the header into the first [`HEADER_LEN`] bytes of `out`.
    ///
    /// # Panics
    /// Panics if `out` is shorter than [`HEADER_LEN`]; callers own a buffer sized for
    /// the full datagram, so this is a programming error rather than a runtime input.
    pub fn write_to(&self, out: &mut [u8]) {
        assert!(out.len() >= HEADER_LEN, "header buffer too small");
        out[0..4].copy_from_slice(&MAGIC.to_be_bytes());
        out[4..8].copy_from_slice(&self.seq_num.to_be_bytes());
        out[8..16].copy_from_slice(&self.timestamp_us.to_be_bytes());
    }

    /// Parse a header from the front of `buf`, returning it alongside the remaining
    /// payload slice (the Opus packet).
    pub fn parse(buf: &[u8]) -> Result<(Header, &[u8])> {
        if buf.len() < HEADER_LEN {
            bail!("datagram shorter than header ({} bytes)", buf.len());
        }
        let magic = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != MAGIC {
            bail!("bad magic 0x{magic:08X}");
        }
        let seq_num = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let timestamp_us = u64::from_be_bytes([
            buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
        ]);
        Ok((
            Header {
                seq_num,
                timestamp_us,
            },
            &buf[HEADER_LEN..],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = Header {
            seq_num: 0xDEAD_BEEF,
            timestamp_us: 0x0123_4567_89AB_CDEF,
        };
        let mut buf = vec![0u8; HEADER_LEN + 3];
        buf[HEADER_LEN..].copy_from_slice(&[1, 2, 3]);
        h.write_to(&mut buf);
        let (parsed, payload) = Header::parse(&buf).unwrap();
        assert_eq!(parsed, h);
        assert_eq!(payload, &[1, 2, 3]);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = vec![0u8; HEADER_LEN];
        buf[0] = 0xFF;
        assert!(Header::parse(&buf).is_err());
    }

    #[test]
    fn rejects_short_datagram() {
        let buf = vec![0u8; HEADER_LEN - 1];
        assert!(Header::parse(&buf).is_err());
    }

    #[test]
    fn frame_arithmetic() {
        assert_eq!(FRAME_SAMPLES_PER_CHANNEL, 480);
        assert_eq!(FRAME_SAMPLES_INTERLEAVED, 960);
    }
}
