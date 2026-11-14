#!/bin/sh
#
# freebsd-setup.sh - one-shot host audio setup for lan-audio-bridge on FreeBSD.
#
# Configures a virtual_oss device that plays audio out to the real speakers while
# mirroring it to a loopback capture node, then wires that node into ALSA so the bridge
# (which talks to ALSA via cpal) can capture it. Finally it verifies capture actually
# produces a signal.
#
# Run as root:
#     sh scripts/freebsd-setup.sh
#
# Override defaults via environment variables, e.g.:
#     SPEAKER_DSP=/dev/dsp1 sh scripts/freebsd-setup.sh
#
# Undo with scripts/freebsd-teardown.sh.

set -eu

# --- Tunables ---------------------------------------------------------------------

# Physical playback device the virtual device feeds (your real, if too quiet, speakers).
SPEAKER_DSP="${SPEAKER_DSP:-/dev/dsp0}"

# Name of the virtual OSS output device applications (Chrome/Firefox) play into.
VDSP_NAME="${VDSP_NAME:-vdsp}"

# Name of the monitor device the bridge captures from. Created with -w as a companion
# of the output device, it mirrors everything played to VDSP_NAME. This is the pattern
# confirmed to capture played-back audio on FreeBSD.
LOOP_NAME="${LOOP_NAME:-vdsp.monitor}"

# Control device for virtual_oss.
CTL_NAME="${CTL_NAME:-vdsp.ctl}"

# Audio format. 32-bit keeps the whole chain (OSS -> ALSA -> cpal) at one width and
# matches the sample format cpal negotiates, avoiding width-mismatch surprises.
RATE="${RATE:-48000}"
CHANNELS="${CHANNELS:-2}"
BITS="${BITS:-32}"
# 10 ms buffer at RATE: samples = RATE / 100.
BUFSAMPLES="${BUFSAMPLES:-480}"

ASOUND_CONF="${ASOUND_CONF:-/usr/local/etc/asound.conf}"

# --- Helpers ----------------------------------------------------------------------

log()  { printf '[setup] %s\n' "$*"; }
warn() { printf '[setup] WARNING: %s\n' "$*" >&2; }
die()  { printf '[setup] ERROR: %s\n' "$*" >&2; exit 1; }

require_root() {
	if [ "$(id -u)" -ne 0 ]; then
		die "must run as root (try: sudo sh $0)"
	fi
}

have() { command -v "$1" >/dev/null 2>&1; }

# --- 1. Dependencies --------------------------------------------------------------

install_deps() {
	log "checking packages (virtual_oss, alsa-lib, alsa-plugins, alsa-utils)"
	# alsa-plugins provides the OSS plugin that lets ALSA (hence cpal) read an OSS node.
	# alsa-utils provides arecord/aplay for the verification step.
	#
	# virtual_oss is intentionally NOT auto-installed: it is not reliably available as a
	# pkg on every system (it may come from ports or a local build). We require the
	# binary to be present and tell the user how to get it rather than guessing.
	missing=""
	for pkg in alsa-lib alsa-plugins alsa-utils; do
		if ! pkg info -e "$pkg" >/dev/null 2>&1; then
			missing="$missing $pkg"
		fi
	done
	if [ -n "$missing" ]; then
		log "installing:$missing"
		# shellcheck disable=SC2086
		pkg install -y $missing || die "pkg install failed for:$missing"
	else
		log "ALSA packages already present"
	fi

	if ! have virtual_oss; then
		die "virtual_oss not found on PATH. Install it from ports (audio/virtual_oss) \
or your preferred source, then re-run this script."
	fi
}

# --- 2. cuse kernel module --------------------------------------------------------

load_cuse() {
	if kldstat -q -m cuse 2>/dev/null; then
		log "cuse module already loaded"
	else
		log "loading cuse module"
		kldload cuse || die "could not load cuse; is the module available?"
	fi
	# Persist across reboots.
	if ! grep -q '^cuse_load="YES"' /boot/loader.conf 2>/dev/null; then
		log "enabling cuse at boot in /boot/loader.conf"
		printf 'cuse_load="YES"\n' >> /boot/loader.conf
	fi
	[ -e /dev/cuse ] || die "/dev/cuse missing even after loading cuse"
}

# --- 3. virtual_oss ---------------------------------------------------------------

stop_stale_virtual_oss() {
	if pgrep -q virtual_oss 2>/dev/null; then
		log "stopping existing virtual_oss instance(s)"
		pkill virtual_oss || true
		# Give cuse clients a moment to release before recreating devices.
		sleep 1
	fi
}

check_speaker_dsp() {
	if [ ! -e "$SPEAKER_DSP" ]; then
		warn "$SPEAKER_DSP does not exist; available playback devices:"
		cat /dev/sndstat 2>/dev/null || true
		die "set SPEAKER_DSP to a real device from /dev/sndstat and re-run"
	fi
}

start_virtual_oss() {
	log "starting virtual_oss: play -> $SPEAKER_DSP, apps -> /dev/$VDSP_NAME, capture -> /dev/$LOOP_NAME"
	# Global stream spec (channels/rate/bits/buffer) up front, applied to the backend.
	#   -f : backend device (your real, if too quiet, speakers)
	#   -w : WAV-format monitor companion; MUST precede the -d it belongs to. It mirrors
	#        everything played to the output device, which is what we capture.
	#   -d : app-facing output device (point Chrome/Firefox here)
	#   -t : control device
	# This single-device + monitor layout is the documented, known-working way to
	# capture played-back audio on FreeBSD.
	virtual_oss -B \
		-C "$CHANNELS" -c "$CHANNELS" -r "$RATE" -b "$BITS" -s "${BUFSAMPLES}" \
		-f "$SPEAKER_DSP" \
		-w "$LOOP_NAME" \
		-d "$VDSP_NAME" \
		-t "$CTL_NAME" \
		|| die "virtual_oss failed to start (check 'Could not create CUSE device' above)"

	# -B backgrounds; wait briefly for the device nodes to appear.
	i=0
	while [ ! -e "/dev/$LOOP_NAME" ] && [ "$i" -lt 20 ]; do
		sleep 0.1
		i=$((i + 1))
	done
	[ -e "/dev/$LOOP_NAME" ] || die "/dev/$LOOP_NAME did not appear after starting virtual_oss"
	log "virtual_oss running; loopback node /dev/$LOOP_NAME is present"
}

# --- 4. ALSA configuration --------------------------------------------------------

# libasound reads a per-user ~/.asoundrc that completely SHADOWS the system
# /usr/local/etc/asound.conf. A stale ~/.asoundrc (e.g. one left from manual testing
# that still points the default capture at an old device) silently overrides everything
# the system config says. Detect it and move it aside so our system config takes effect.
neutralise_user_asoundrc() {
	user_rc="${HOME:-/root}/.asoundrc"
	[ -f "$user_rc" ] || return 0

	# If it already routes to our monitor, leave it; otherwise it would shadow us.
	if grep -q "$LOOP_NAME" "$user_rc" 2>/dev/null; then
		log "found ~/.asoundrc already referencing $LOOP_NAME; leaving it in place"
		return 0
	fi

	bak="${user_rc}.bak.$(date +%Y%m%d%H%M%S)"
	warn "a ~/.asoundrc exists and would override the system config; moving it to $bak"
	mv "$user_rc" "$bak"
}

write_asound_conf() {
	mkdir -p "$(dirname "$ASOUND_CONF")"

	# ALSA rejects a config that assigns the same node (e.g. pcm.!default) twice, so we
	# cannot simply append our own pcm.!default next to the one in the stock FreeBSD
	# alsa-plugins asound.conf. We preserve the existing file, neutralise only its
	# previous lan-audio-bridge override and its one-liner !default / !sysdefault
	# assignments (by commenting them out), then append a single authoritative block.
	marker_begin="# >>> lan-audio-bridge BEGIN >>>"
	marker_end="# <<< lan-audio-bridge END <<<"

	if [ -f "$ASOUND_CONF" ]; then
		backup="${ASOUND_CONF}.bak.$(date +%Y%m%d%H%M%S)"
		cp "$ASOUND_CONF" "$backup"
		log "backed up existing config to $backup"

		# Drop any previous block we appended, so re-running is idempotent.
		if grep -qF "$marker_begin" "$ASOUND_CONF"; then
			sed -i '' "/^${marker_begin}\$/,/^${marker_end}\$/d" "$ASOUND_CONF"
		fi

		# Comment out every existing definition of the nodes we override, so our block
		# does not collide with theirs. We must handle BOTH forms ALSA accepts:
		#   terse one-liner:  pcm.!default pcm.sysdefault
		#   block-brace form: pcm.!default { type oss device "/dev/dsp9" }
		# The brace form can span multiple lines, so we track brace depth and comment
		# out the whole definition, not just its opening line.
		tmpf="$(mktemp -t lanbridge-conf)" || die "mktemp failed"
		in_block=0
		depth=0
		while IFS= read -r line; do
			if [ "$in_block" -eq 1 ]; then
				# Inside a multi-line block we are neutralising: comment every line and
				# track braces until the block closes.
				printf '# (lan-audio-bridge: overridden below) %s\n' "$line" >> "$tmpf"
				opens=$(printf '%s' "$line" | tr -cd '{' | wc -c | tr -d ' ')
				closes=$(printf '%s' "$line" | tr -cd '}' | wc -c | tr -d ' ')
				depth=$((depth + opens - closes))
				[ "$depth" -le 0 ] && in_block=0
				continue
			fi
			case "$line" in
				pcm.!default[[:space:]]*|ctl.!default[[:space:]]*|\
				pcm.!sysdefault[[:space:]]*|ctl.!sysdefault[[:space:]]*)
					printf '# (lan-audio-bridge: overridden below) %s\n' "$line" >> "$tmpf"
					# If this line opens a brace block that is not closed on the same
					# line, keep commenting subsequent lines until braces balance.
					opens=$(printf '%s' "$line" | tr -cd '{' | wc -c | tr -d ' ')
					closes=$(printf '%s' "$line" | tr -cd '}' | wc -c | tr -d ' ')
					depth=$((opens - closes))
					[ "$depth" -gt 0 ] && in_block=1
					;;
				*)
					printf '%s\n' "$line" >> "$tmpf"
					;;
			esac
		done < "$ASOUND_CONF"
		mv "$tmpf" "$ASOUND_CONF"
	else
		log "no existing $ASOUND_CONF; creating a new one"
		: > "$ASOUND_CONF"
	fi

	log "appending lan-audio-bridge capture override to $ASOUND_CONF (-> /dev/$LOOP_NAME)"

	# A plug layer in front of the OSS node makes ALSA convert rate/channels/width to
	# whatever cpal negotiates. The slave is left UNCONSTRAINED on purpose: pinning the
	# slave's format/rate/channels makes ALSA refine the OSS node against exactly those
	# params, and if the node does not report that precise combination the whole slave
	# is rejected ("Slave PCM not usable"). An unconstrained plug slave lets the OSS
	# node negotiate freely while plug adapts the client side. The OSS node only speaks
	# integer PCM; float is handled inside the bridge, not here.
	cat >> "$ASOUND_CONF" <<EOF

$marker_begin
# Generated by scripts/freebsd-setup.sh. Routes ALSA's default capture to the
# virtual_oss monitor (/dev/$LOOP_NAME). Remove this block (and uncomment the lines
# above) to restore the previous behaviour, or run scripts/freebsd-teardown.sh.

pcm.lan_capture_oss {
    type oss
    device "/dev/$LOOP_NAME"
}

pcm.lan_capture {
    type plug
    slave.pcm "lan_capture_oss"
}

pcm.!default pcm.lan_capture

ctl.lan_capture {
    type oss
    device "/dev/$LOOP_NAME"
}

ctl.!default ctl.lan_capture
$marker_end
EOF
}

# --- 5. Verify capture ------------------------------------------------------------

verify_capture() {
	log "verifying capture from ALSA default (3 s)..."
	tmp="$(mktemp -t lanbridge)" || die "mktemp failed"
	case "$BITS" in
		16) rec_fmt="S16_LE" ;;
		24) rec_fmt="S24_LE" ;;
		32) rec_fmt="S32_LE" ;;
		*)  rec_fmt="S16_LE" ;;
	esac

	if ! arecord -D default -f "$rec_fmt" -c "$CHANNELS" -r "$RATE" -d 3 "$tmp" >/dev/null 2>&1; then
		warn "arecord could not open the default capture device"
		warn "check that alsa-plugins is installed and $ASOUND_CONF is valid"
		rm -f "$tmp"
		return 1
	fi

	size=$(stat -f %z "$tmp" 2>/dev/null || echo 0)
	rm -f "$tmp"
	if [ "$size" -gt 100000 ]; then
		log "capture OK (recorded $size bytes). Play audio into /dev/$VDSP_NAME to stream it."
	else
		warn "capture opened but produced little/no data ($size bytes)."
		warn "that is expected if nothing was playing; it is a problem if audio WAS playing."
	fi
	return 0
}

# --- Main -------------------------------------------------------------------------

main() {
	require_root
	install_deps
	load_cuse
	check_speaker_dsp
	stop_stale_virtual_oss
	start_virtual_oss
	neutralise_user_asoundrc
	write_asound_conf
	verify_capture || true

	cat <<EOF

[setup] Done.

  Point your browser / applications' audio OUTPUT at:  /dev/$VDSP_NAME
  (You will still hear it on $SPEAKER_DSP; raise its level with 'mixer' if quiet.)

  Start the guest on the playback machine, then run the host here:

      ./target/release/lan-audio-bridge host --target <GUEST_IP>:5555

  Run with no --device: the bridge uses ALSA's default, now the loopback node.

  To undo this setup:  sh scripts/freebsd-teardown.sh
EOF
}

main "$@"
