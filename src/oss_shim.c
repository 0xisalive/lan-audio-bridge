/*
 * OSS ioctl shim for FreeBSD.
 *
 * The SNDCTL_DSP_* request numbers are computed by the _IOWR() macros in
 * <sys/soundcard.h> and depend on the size of the argument type and the platform's
 * ioctl encoding. Rather than hardcode those numbers in Rust (brittle across arches
 * and header versions), we let the C compiler expand them against the real system
 * header and expose small typed wrappers. The Rust side then calls these and never
 * needs to know the raw request values.
 *
 * Every function returns 0 on success and -1 on failure, matching ioctl(2); the Rust
 * caller consults errno via std::io::Error::last_os_error().
 */

#include <sys/soundcard.h>
#include <sys/ioctl.h>

/* Report the AFMT_* bit for signed little-endian 16- and 32-bit PCM, so the Rust side
 * does not have to reproduce these constants either. */
int lanb_afmt_s16_le(void) { return AFMT_S16_LE; }
int lanb_afmt_s32_le(void) { return AFMT_S32_LE; }

/* Select the sample format (an AFMT_* value). On return *fmt holds the format the
 * driver actually set, which the caller must honour. */
int lanb_set_format(int fd, int *fmt) {
	return ioctl(fd, SNDCTL_DSP_SETFMT, fmt);
}

/* Query the formats the device supports (an AFMT_* mask). */
int lanb_get_formats(int fd, int *mask) {
	return ioctl(fd, SNDCTL_DSP_GETFMTS, mask);
}

/* Set the channel count; *ch holds the granted count on return. */
int lanb_set_channels(int fd, int *ch) {
	return ioctl(fd, SNDCTL_DSP_CHANNELS, ch);
}

/* Set the sample rate in Hz; *rate holds the granted rate on return. */
int lanb_set_speed(int fd, int *rate) {
	return ioctl(fd, SNDCTL_DSP_SPEED, rate);
}
