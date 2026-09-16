# DVH1 haptics: the host half

Host side of the private XInputHID rumble transport the DroidVM Windows
`vioinput` driver negotiates. The wire contract is owned by the guest repo
(`gunyah-guest-drivers-windows`, branch `work/vioinput-xinput-haptics-20260915`)
in `vioinput/docs/HAPTICS-ABI.md` and `vioinput/sys/HapticsCore.h`. This file
describes only what crosvm does about it.

This is a branch-private profile: not standard `EV_FF`, not an allocated virtio
feature, not an assigned event type. Renumbering or standardizing it is a
version bump, never a silent reuse.

## Where it lives

| Piece | File |
| --- | --- |
| Frame codec, session state machine, virtqueue bridge | `devices/src/virtio/input/haptics.rs` |
| Kernel vibrator backends | `devices/src/virtio/input/vibrator.rs` |
| Gamepad device config (axes, buttons, absinfo) | `devices/src/virtio/input/defaults.rs` |
| Capability in config space, statusq intercept, eventq frames, expiry timer | `devices/src/virtio/input/mod.rs` |
| `--input gamepad[...]` option | `src/crosvm/config.rs`, `src/crosvm/sys/linux.rs` |

## Using it

```
--input gamepad[path=/path/to/socket,name=DroidVM Pad,haptics=true]
--input gamepad[path=/path/to/socket,haptics=true,vibrator=/sys/class/leds/vibrator]
```

`haptics` defaults to **off**. A host with nothing to vibrate must not advertise
that it can: with the capability absent the guest never opens the profile and
falls back to a plain gamepad, which is the correct outcome rather than a
failure.

## What the host guarantees

The guest trusts the host for the things it cannot enforce itself, so these are
enforced here rather than assumed:

- **Capability.** 32 bytes at config `select=0x80`, `subsel=1`: magic `DVH1`,
  version 1, flags 7, event type `0xff80`, profile 1, a fresh non-zero epoch and
  the lease. The guest reads it twice and compares, so it is a fixed snapshot
  per epoch.
- **Framing.** Guest->host frames arrive as one 96-byte statusq descriptor and
  are validated whole — twelve records, correct type and index order, reserved
  word zero, CRC32C over the first 88 bytes — before anything changes. Host->guest
  frames go out as twelve successive eventq events.
- **Never forwarded.** A descriptor carrying event type `0xff80` never reaches an
  evdev device, whether it decodes or not. A malformed one is dropped, not
  forwarded as ordinary events.
- **Handshake.** Output is refused until `GUEST_READY` has been answered with
  `HOST_READY` on the same epoch. The guest gives up after 2 s, so the reply is
  sent on the spot.
- **Sequencing.** One strictly increasing guest domain shared by handshake and
  vibration; replays are inert. The host keeps its own monotonic domain starting
  at 1. A stale epoch, a replayed sequence, an unknown opcode or a
  host-direction opcode arriving from the guest changes nothing — and does not
  even consume the sequence.
- **Leases and finite effects are separate clocks.** A keepalive renews the lease
  only. It can neither restart playback nor push out the end of a finite effect.
  Both deadlines are the host's own: a paused, crashed or disconnected guest may
  never send `STOP`, so the host stops on its own timer regardless.
- **Status carries the effect's revision.** The guest drops a `HOST_STATUS` whose
  revision does not match the effect it is playing, so completion (detail 1),
  lease expiry and sink faults all name their effect. `HOST_READY` and
  `HOST_REVOKE` are session-scoped and carry zero.
- **Revocation is sticky.** After `HOST_REVOKE` the session refuses output and a
  re-handshake on the same epoch does not recover it; only a reset with a fresh
  epoch does. Re-arming inside one D0 is deliberately not implemented, matching
  the guest.
- **Teardown stops the motor** outright rather than sending a frame nobody will
  collect.

## Report format

`motors` is four 8-bit percentages (0..=100), bytes 0..3 = left trigger, right
trigger, left body, right body. `detail` byte 0 is the actuator mask, bits 3..0
in that same order, high nibble reserved; byte 1 duration, byte 2 delay, both in
10 ms units; byte 3 repeat count. A zero duration or a mask selecting only
zero-magnitude channels is not a valid effect. The host stops on its own after
`delay + (repeat + 1) * duration`.

Opcodes 4/5 (`SET_RUMBLE`/`KEEPALIVE`) are the superseded raw-rumble draft. They
stay reserved and are never acted on; their four magnitude bytes must not be
read as two 16-bit motors.

## Verification

- `cargo test -p devices --lib virtio::input` — 56 tests. 27 protocol: codec round-trip, every
  single-bit corruption of a frame rejected, capability byte layout, handshake
  gating, stale epoch/replay, spoofed host opcodes, lease vs finite-effect
  independence, revocation, sink faults, frame splitting, and the rule that
  every frame the host emits passes the guest's own validity check. 11 vibrator:
  both backends' write protocol, amplitude scaling, the activation cap,
  idempotent stop, and stop-on-drop, all against real files.
- **End-to-end on guest bytes.** `vibrator.rs::guest_frame_tests` replays a
  handshake, rumble, keepalive and stop *encoded by the driver's own C code* and
  asserts the sysfs node is driven and released — including that a keepalive does
  not retrigger it, that a silent guest still gets stopped by the host's timer,
  and that an unhandshaken or stale-epoch guest cannot move the motor at all.
- **Real device surface.** `mod.rs::haptic_device_tests` drives config space
  through `read`/`write` (the capability is byte-identical across two reads, as
  the guest requires, and absent when haptics are off), checks the gamepad
  advertises every axis the guest's probe insists on with a usable range, and
  moves a whole guest frame in and a whole control frame out over real
  `GuestMemory` descriptor chains.
- **Differential test against the guest.** The protocol core was checked against
  the driver's real C implementation by compiling `vioinput/sys/HapticsCore.h`
  and comparing 5000 generated messages in both directions: 5000/5000 identical
  encodings and 5000/5000 identical decodes. This is the check that matters —
  self-consistent Rust that disagrees with the guest by one byte produces
  silence, not an error.

## The motor

crosvm runs as root on the phone, so effects go straight to the kernel rather
than back through the Android framework. `SysfsVibrator` handles both shapes
found in practice:

- **LED class** (`/sys/class/leds/vibrator`, and the `vibrator_1` / `qti-haptics`
  variants): write `duration` ms then `activate` 1; `activate` 0 stops early.
  Amplitude uses `brightness` scaled against `max_brightness` when the driver
  exposes it. A node with `activate` but no `duration`, or a plain LED with only
  `brightness`, is rejected rather than driven.
- **timed_output** (`/sys/class/timed_output/vibrator/enable`): a millisecond
  count starts it, 0 stops it. No amplitude.

`vibrator=` names a node explicitly and is a hard error if unusable; omitting it
probes the well-known paths. If nothing is found the device still comes up and
logs each effect instead of playing it — the guest negotiates either way, which
keeps a missing motor distinguishable from a broken transport. The handle is
reopened per activation, and `Drop` stops the motor so a worker going away
cannot leave it latched on.

Approximation worth knowing: the kernel nodes take one magnitude and one
duration, so the strongest requested channel is played solid for
`delay + (repeat + 1) * duration`. Per-actuator routing, inter-repeat gaps and
`delay` silence are not reproduced.

## What is NOT done

- **Nothing here has run against the Windows guest, or on the phone.** The phone
  was unreachable for this work (VPN up, host silent), so the guest driver and
  this host have only ever been checked against each other. Driver binding,
  `XInputGetState`/`XInputSetState`, and a motor that a person can feel are all
  still unverified. Treat everything above as a testable implementation, not a
  working feature.
- The sysfs node names are the documented upstream/QTI ones but have not been
  confirmed against this specific device. Check what `/sys/class/leds/` actually
  contains on the phone before assuming the probe binds.
- The ABI also requires the host to stop on user-disable and on going to the
  background. crosvm has no signal for either, so that policy belongs to the
  DroidVM app and is not implemented; `HapticsBridge::revoke` is the hook it
  would call.
- The host never sends `HOST_HELLO`; it is optional and cannot substitute for the
  capability record anyway.
