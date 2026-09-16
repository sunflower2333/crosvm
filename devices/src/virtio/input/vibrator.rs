// Copyright 2026 The DroidVM Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Kernel vibrator backends for the DVH1 haptic sink.
//!
//! crosvm runs as root on the phone, so it can drive the motor through the
//! kernel's own interfaces rather than round-tripping through the Android
//! framework. Two shapes exist in practice:
//!
//! * **LED-class vibrator** (`/sys/class/leds/vibrator`), the modern upstream
//!   and Qualcomm layout: write `duration` in milliseconds, then `activate` 1;
//!   write `activate` 0 to stop early. Amplitude, where the driver exposes it,
//!   is `brightness` scaled against `max_brightness`.
//! * **timed_output** (`/sys/class/timed_output/vibrator/enable`), the legacy
//!   layout: write a millisecond count to start, 0 to stop. No amplitude.
//!
//! Probing is deliberately explicit about what it found: a haptics path that
//! silently binds nothing is indistinguishable from a broken one.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::anyhow;
use anyhow::Context;
use base::info;
use base::warn;

use super::haptics::HapticEffect;
use super::haptics::HapticSink;

/// Upper bound on a single kernel activation, in milliseconds.
///
/// The session's lease and effect deadlines are the real stop; this only keeps
/// a nonsense duration from latching the motor on if those ever fail to fire.
const MAX_ACTIVATION_MS: u64 = 5_000;

/// A left/right motor pair, tried first. The OnePlus SM8750 (AWINIC
/// aw-haptic-hv) exposes exactly this, and XInput's left/right split maps
/// onto it directly instead of being collapsed to one pulse.
const SPLIT_CANDIDATES: &[(&str, &str)] =
    &[("/sys/class/leds/vibrator_l", "/sys/class/leds/vibrator_r")];

/// Single-motor search order. An explicit path always wins over these.
const LED_CANDIDATES: &[&str] = &[
    "/sys/class/leds/vibrator",
    "/sys/class/leds/vibrator_l",
    "/sys/class/leds/vibrator_1",
    "/sys/class/leds/qti-haptics",
];
const TIMED_OUTPUT_CANDIDATES: &[&str] = &["/sys/class/timed_output/vibrator"];

#[derive(Debug, PartialEq, Eq)]
enum Backend {
    /// `duration` + `activate`, with optional `brightness` amplitude.
    Led { max_brightness: u32, amplitude: bool },
    /// A single `enable` node taking a millisecond count.
    TimedOutput,
}

/// A vibrator reached through sysfs.
#[derive(Debug)]
pub struct SysfsVibrator {
    root: PathBuf,
    backend: Backend,
    /// Tracks whether the motor was left running, so `stop` stays idempotent
    /// without reading sysfs back.
    running: bool,
}

impl SysfsVibrator {
    /// Bind to a specific sysfs directory, deciding its shape from the nodes
    /// actually present.
    pub fn open(root: &Path) -> anyhow::Result<SysfsVibrator> {
        if root.join("activate").is_file() {
            // `duration` is what makes the LED class usable as a vibrator; a
            // plain LED has `brightness` but no `activate`/`duration` pair.
            if !root.join("duration").is_file() {
                return Err(anyhow!(
                    "{} has activate but no duration; not a vibrator",
                    root.display()
                ));
            }
            let amplitude = root.join("brightness").is_file();
            let max_brightness = read_u32(&root.join("max_brightness")).unwrap_or(255).max(1);
            return Ok(SysfsVibrator {
                root: root.to_path_buf(),
                backend: Backend::Led {
                    max_brightness,
                    amplitude,
                },
                running: false,
            });
        }
        if root.join("enable").is_file() {
            return Ok(SysfsVibrator {
                root: root.to_path_buf(),
                backend: Backend::TimedOutput,
                running: false,
            });
        }
        Err(anyhow!(
            "{} has neither activate nor enable",
            root.display()
        ))
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    fn write_node(&self, node: &str, value: impl ToString) -> anyhow::Result<()> {
        let path = self.root.join(node);
        // sysfs attributes take a whole value per write; open/truncate each time.
        fs::write(&path, value.to_string())
            .with_context(|| format!("writing {}", path.display()))
    }

    fn start(&mut self, duration: Duration, magnitude: u8) -> anyhow::Result<()> {
        let ms = (duration.as_millis() as u64).clamp(1, MAX_ACTIVATION_MS);
        match self.backend {
            Backend::Led {
                max_brightness,
                amplitude,
            } => {
                if amplitude {
                    // magnitude is a percentage; map onto the driver's scale.
                    let level = (magnitude.min(100) as u32 * max_brightness).div_ceil(100);
                    self.write_node("brightness", level.max(1))?;
                }
                self.write_node("duration", ms)?;
                self.write_node("activate", 1)?;
            }
            Backend::TimedOutput => {
                self.write_node("enable", ms)?;
            }
        }
        self.running = true;
        Ok(())
    }
}

/// Which motor node(s) to open. Kept as paths, not handles, so the device can
/// reopen a clean handle on every activation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VibratorSpec {
    Single(PathBuf),
    Split { left: PathBuf, right: PathBuf },
}

impl VibratorSpec {
    /// Try the well-known locations, preferring a left/right pair. Returns
    /// `None` when the host has no vibrator, which is a normal outcome.
    pub fn probe() -> Option<VibratorSpec> {
        for (l, r) in SPLIT_CANDIDATES {
            let (l, r) = (Path::new(l), Path::new(r));
            if !l.is_dir() || !r.is_dir() {
                continue;
            }
            match (SysfsVibrator::open(l), SysfsVibrator::open(r)) {
                (Ok(_), Ok(_)) => {
                    info!(
                        "haptics: bound split vibrator left={} right={}",
                        l.display(),
                        r.display()
                    );
                    return Some(VibratorSpec::Split {
                        left: l.to_path_buf(),
                        right: r.to_path_buf(),
                    });
                }
                (Err(e), _) | (_, Err(e)) => {
                    warn!("haptics: pair {}/{} not usable: {:#}", l.display(), r.display(), e)
                }
            }
        }
        for candidate in LED_CANDIDATES.iter().chain(TIMED_OUTPUT_CANDIDATES) {
            let path = Path::new(candidate);
            if !path.is_dir() {
                continue;
            }
            match SysfsVibrator::open(path) {
                Ok(v) => {
                    info!("haptics: bound vibrator at {} ({:?})", path.display(), v.backend);
                    return Some(VibratorSpec::Single(path.to_path_buf()));
                }
                Err(e) => warn!("haptics: {} is not usable: {:#}", path.display(), e),
            }
        }
        None
    }

    /// Open fresh handles. Fails if any named node is unusable, so an explicit
    /// configuration error is reported instead of silently degrading.
    pub fn open(&self) -> anyhow::Result<Box<dyn HapticSink>> {
        Ok(match self {
            VibratorSpec::Single(p) => Box::new(SysfsVibrator::open(p)?),
            VibratorSpec::Split { left, right } => Box::new(SplitVibrator {
                left: SysfsVibrator::open(left)?,
                right: SysfsVibrator::open(right)?,
            }),
        })
    }
}

/// Two independent motors, one per side.
///
/// XInput channels are [left trigger, right trigger, left body, right body];
/// each side plays the stronger of its two. A side asked for nothing is
/// stopped explicitly, so a change of pattern cannot leave the other motor
/// running from the previous effect.
pub struct SplitVibrator {
    left: SysfsVibrator,
    right: SysfsVibrator,
}

impl HapticSink for SplitVibrator {
    fn play(&mut self, effect: &HapticEffect) -> anyhow::Result<()> {
        let l = effect.magnitudes[0].max(effect.magnitudes[2]);
        let r = effect.magnitudes[1].max(effect.magnitudes[3]);
        let span = effect.total_span();
        let rl = if l == 0 {
            self.left.stop()
        } else {
            self.left.start(span, l)
        };
        let rr = if r == 0 {
            self.right.stop()
        } else {
            self.right.start(span, r)
        };
        // Report the first failure but always attempt both sides.
        rl.and(rr)
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        let a = self.left.stop();
        let b = self.right.stop();
        a.and(b)
    }
}

impl HapticSink for SysfsVibrator {
    fn play(&mut self, effect: &HapticEffect) -> anyhow::Result<()> {
        // One pulse at the strongest requested channel. The kernel nodes take a
        // single magnitude and duration, so per-actuator routing, the inter-
        // repeat gaps and `delay` are not reproduced; the span is played solid.
        // The session's own deadline still bounds it.
        let magnitude = effect.magnitudes.iter().copied().max().unwrap_or(0);
        if magnitude == 0 {
            return self.stop();
        }
        self.start(effect.total_span(), magnitude)
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        if !self.running {
            return Ok(());
        }
        // Clear the flag first: a failed write must not leave us believing the
        // motor is still ours to stop on every later call.
        self.running = false;
        match self.backend {
            Backend::Led { .. } => self.write_node("activate", 0),
            Backend::TimedOutput => self.write_node("enable", 0),
        }
    }
}

impl Drop for SysfsVibrator {
    fn drop(&mut self) {
        // Never leave the motor latched on because a worker went away.
        if let Err(e) = self.stop() {
            warn!("haptics: failed to stop vibrator on teardown: {:#}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;

    fn led_node(amplitude: bool, max_brightness: Option<u32>) -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("activate"), "0").unwrap();
        fs::write(dir.path().join("duration"), "0").unwrap();
        if amplitude {
            fs::write(dir.path().join("brightness"), "0").unwrap();
        }
        if let Some(max) = max_brightness {
            fs::write(dir.path().join("max_brightness"), format!("{max}\n")).unwrap();
        }
        dir
    }

    fn read(dir: &TempDir, node: &str) -> String {
        fs::read_to_string(dir.path().join(node)).unwrap()
    }

    fn effect(magnitudes: [u8; 4], duration_ms: u64, repeat: u8) -> HapticEffect {
        HapticEffect {
            magnitudes,
            duration: Duration::from_millis(duration_ms),
            delay: Duration::ZERO,
            repeat,
        }
    }

    #[test]
    fn led_backend_writes_duration_then_activates() {
        let dir = led_node(false, None);
        let mut v = SysfsVibrator::open(dir.path()).unwrap();
        v.play(&effect([0, 0, 60, 0], 50, 0)).unwrap();
        assert_eq!(read(&dir, "duration"), "50");
        assert_eq!(read(&dir, "activate"), "1");
    }

    #[test]
    fn repeat_extends_the_activation_span() {
        let dir = led_node(false, None);
        let mut v = SysfsVibrator::open(dir.path()).unwrap();
        // (repeat + 1) passes of 50 ms.
        v.play(&effect([0, 0, 60, 0], 50, 2)).unwrap();
        assert_eq!(read(&dir, "duration"), "150");
    }

    #[test]
    fn amplitude_scales_against_max_brightness() {
        let dir = led_node(true, Some(1000));
        let mut v = SysfsVibrator::open(dir.path()).unwrap();
        // Strongest channel wins; 50% of 1000.
        v.play(&effect([10, 50, 0, 20], 50, 0)).unwrap();
        assert_eq!(read(&dir, "brightness"), "500");
    }

    #[test]
    fn amplitude_defaults_to_255_scale_without_max_brightness() {
        let dir = led_node(true, None);
        let mut v = SysfsVibrator::open(dir.path()).unwrap();
        v.play(&effect([0, 0, 100, 0], 50, 0)).unwrap();
        assert_eq!(read(&dir, "brightness"), "255");
    }

    /// A non-zero request must never round down to a silent zero.
    #[test]
    fn tiny_amplitude_still_drives_the_motor() {
        let dir = led_node(true, Some(255));
        let mut v = SysfsVibrator::open(dir.path()).unwrap();
        v.play(&effect([0, 0, 1, 0], 50, 0)).unwrap();
        assert_ne!(read(&dir, "brightness"), "0");
    }

    #[test]
    fn stop_deactivates_and_is_idempotent() {
        let dir = led_node(false, None);
        let mut v = SysfsVibrator::open(dir.path()).unwrap();
        v.play(&effect([0, 0, 60, 0], 50, 0)).unwrap();
        v.stop().unwrap();
        assert_eq!(read(&dir, "activate"), "0");
        // A second stop writes nothing new and still succeeds.
        fs::write(dir.path().join("activate"), "sentinel").unwrap();
        v.stop().unwrap();
        assert_eq!(read(&dir, "activate"), "sentinel");
    }

    #[test]
    fn zero_magnitude_stops_instead_of_starting() {
        let dir = led_node(false, None);
        let mut v = SysfsVibrator::open(dir.path()).unwrap();
        v.play(&effect([0, 0, 0, 0], 50, 0)).unwrap();
        assert_eq!(read(&dir, "activate"), "0");
    }

    #[test]
    fn activation_is_capped() {
        let dir = led_node(false, None);
        let mut v = SysfsVibrator::open(dir.path()).unwrap();
        v.play(&effect([0, 0, 60, 0], 2000, 9)).unwrap();
        assert_eq!(read(&dir, "duration"), MAX_ACTIVATION_MS.to_string());
    }

    #[test]
    fn timed_output_backend_uses_enable() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("enable"), "0").unwrap();
        let mut v = SysfsVibrator::open(dir.path()).unwrap();
        assert_eq!(v.backend, Backend::TimedOutput);
        v.play(&effect([0, 0, 60, 0], 50, 0)).unwrap();
        assert_eq!(read(&dir, "enable"), "50");
        v.stop().unwrap();
        assert_eq!(read(&dir, "enable"), "0");
    }

    #[test]
    fn a_plain_led_is_not_mistaken_for_a_vibrator() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("brightness"), "0").unwrap();
        fs::write(dir.path().join("max_brightness"), "255").unwrap();
        assert!(SysfsVibrator::open(dir.path()).is_err());

        // activate without duration is equally not a vibrator.
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("activate"), "0").unwrap();
        assert!(SysfsVibrator::open(dir.path()).is_err());
    }

    fn split() -> (TempDir, TempDir, SplitVibrator) {
        let l = led_node(true, Some(255));
        let r = led_node(true, Some(255));
        let v = SplitVibrator {
            left: SysfsVibrator::open(l.path()).unwrap(),
            right: SysfsVibrator::open(r.path()).unwrap(),
        };
        (l, r, v)
    }

    #[test]
    fn split_routes_left_and_right_channels_to_their_motors() {
        let (l, r, mut v) = split();
        // left body 60%, right trigger 30%
        v.play(&effect([0, 30, 60, 0], 50, 0)).unwrap();
        assert_eq!(read(&l, "activate"), "1");
        assert_eq!(read(&l, "brightness"), "153");
        assert_eq!(read(&r, "activate"), "1");
        assert_eq!(read(&r, "brightness"), "77");
    }

    #[test]
    fn split_side_takes_the_stronger_of_its_two_channels() {
        let (l, _r, mut v) = split();
        // left trigger 90 beats left body 20
        v.play(&effect([90, 0, 20, 0], 50, 0)).unwrap();
        assert_eq!(read(&l, "brightness"), "230");
    }

    #[test]
    fn split_stops_a_side_that_is_no_longer_requested() {
        let (l, r, mut v) = split();
        v.play(&effect([0, 0, 60, 60], 50, 0)).unwrap();
        assert_eq!(read(&r, "activate"), "1");
        // New pattern: left only. Right must be released, not left running.
        v.play(&effect([0, 0, 60, 0], 50, 0)).unwrap();
        assert_eq!(read(&l, "activate"), "1");
        assert_eq!(read(&r, "activate"), "0");
    }

    #[test]
    fn split_stop_releases_both() {
        let (l, r, mut v) = split();
        v.play(&effect([0, 0, 60, 60], 50, 0)).unwrap();
        v.stop().unwrap();
        assert_eq!(read(&l, "activate"), "0");
        assert_eq!(read(&r, "activate"), "0");
    }

    #[test]
    fn spec_open_fails_closed_if_either_side_is_unusable() {
        let l = led_node(false, None);
        let bogus = TempDir::new().unwrap(); // no vibrator nodes at all
        let spec = VibratorSpec::Split {
            left: l.path().to_path_buf(),
            right: bogus.path().to_path_buf(),
        };
        assert!(spec.open().is_err());
        assert!(VibratorSpec::Single(l.path().to_path_buf()).open().is_ok());
    }

    #[test]
    fn drop_stops_a_running_motor() {
        let dir = led_node(false, None);
        {
            let mut v = SysfsVibrator::open(dir.path()).unwrap();
            v.play(&effect([0, 0, 60, 0], 5000, 0)).unwrap();
            assert_eq!(read(&dir, "activate"), "1");
        }
        assert_eq!(read(&dir, "activate"), "0");
    }
}

#[cfg(test)]
mod guest_frame_tests {
    //! End-to-end over the real guest wire format.
    //!
    //! These byte strings were produced by the Windows driver's own encoder
    //! (`vioinput/sys/HapticsCore.h`, compiled and run), not by this crate. They
    //! drive the full host path -- statusq classification, CRC validation,
    //! session state machine, sink -- into a vibrator node, so a divergence from
    //! the guest shows up as a motor that does not move.

    use std::time::Duration;
    use std::time::Instant;

    use tempfile::TempDir;

    use super::*;
    use crate::virtio::input::haptics::HapticsBridge;
    use crate::virtio::input::haptics::StatusVerdict;

    // Guest epoch 0x1122334455667788, one 50 ms 60% left-body rumble.
    const GUEST_READY: &str = "80ff00000100020080ff01008877665580ff02004433221180ff03000100000080ff04000000000080ff05000000000080ff06000000000080ff07000000000080ff08000000000080ff09000000000080ff0a000000000080ff0b0035c8af03";
    const SET_XINPUT: &str = "80ff000001000a0080ff01008877665580ff02004433221180ff03000200000080ff04000000000080ff05000100000080ff06000000000080ff070000003c0080ff0800e803000080ff09000205000080ff0a000000000080ff0b00503f0365";
    const KEEPALIVE: &str = "80ff000001000b0080ff01008877665580ff02004433221180ff03000300000080ff04000000000080ff05000100000080ff06000000000080ff070000003c0080ff0800e803000080ff09000205000080ff0a000000000080ff0b00efc2c775";
    const STOP: &str = "80ff00000100060080ff01008877665580ff02004433221180ff03000400000080ff04000000000080ff05000100000080ff06000000000080ff07000000000080ff08000000000080ff09000000000080ff0a000000000080ff0b00adbe5655";

    const GUEST_EPOCH: u64 = 0x1122_3344_5566_7788;

    fn bytes(hex: &str) -> Vec<u8> {
        (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
            .collect()
    }

    fn node() -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("activate"), "0").unwrap();
        fs::write(dir.path().join("duration"), "0").unwrap();
        fs::write(dir.path().join("brightness"), "0").unwrap();
        fs::write(dir.path().join("max_brightness"), "255").unwrap();
        dir
    }

    fn read(dir: &TempDir, n: &str) -> String {
        fs::read_to_string(dir.path().join(n)).unwrap()
    }

    #[test]
    fn a_real_guest_rumble_reaches_the_motor() {
        let dir = node();
        let vibrator = SysfsVibrator::open(dir.path()).unwrap();
        let mut bridge = HapticsBridge::new(GUEST_EPOCH, 1000, Box::new(vibrator));
        let t0 = Instant::now();

        // Handshake, and the host answers.
        assert_eq!(
            bridge.handle_status_descriptor(&bytes(GUEST_READY), t0),
            StatusVerdict::Consumed
        );
        assert!(bridge.has_pending(), "host did not answer GUEST_READY");
        while bridge.pop_pending().is_some() {}

        // The rumble itself.
        assert_eq!(
            bridge.handle_status_descriptor(&bytes(SET_XINPUT), t0),
            StatusVerdict::Consumed
        );
        assert_eq!(read(&dir, "activate"), "1", "motor did not start");
        assert_eq!(read(&dir, "duration"), "50");
        // 60% of the 255 scale.
        assert_eq!(read(&dir, "brightness"), "153");

        // A keepalive renews the lease and must not retrigger the motor.
        fs::write(dir.path().join("activate"), "sentinel").unwrap();
        assert_eq!(
            bridge.handle_status_descriptor(&bytes(KEEPALIVE), t0 + Duration::from_millis(20)),
            StatusVerdict::Consumed
        );
        assert_eq!(read(&dir, "activate"), "sentinel", "keepalive replayed the effect");

        // The guest's STOP stops it.
        assert_eq!(
            bridge.handle_status_descriptor(&bytes(STOP), t0 + Duration::from_millis(30)),
            StatusVerdict::Consumed
        );
        assert_eq!(read(&dir, "activate"), "0", "motor did not stop");
    }

    /// With no STOP from the guest, the host's own deadline must stop the motor.
    #[test]
    fn a_silent_guest_still_gets_the_motor_stopped() {
        let dir = node();
        let vibrator = SysfsVibrator::open(dir.path()).unwrap();
        let mut bridge = HapticsBridge::new(GUEST_EPOCH, 1000, Box::new(vibrator));
        let t0 = Instant::now();

        bridge.handle_status_descriptor(&bytes(GUEST_READY), t0);
        bridge.handle_status_descriptor(&bytes(SET_XINPUT), t0);
        assert_eq!(read(&dir, "activate"), "1");

        // Nothing further from the guest; the 50 ms effect ends on its own.
        bridge.poll_timeouts(t0 + Duration::from_millis(60));
        assert_eq!(read(&dir, "activate"), "0");
    }

    /// Output before the handshake must never reach the motor.
    #[test]
    fn an_unhandshaken_guest_cannot_drive_the_motor() {
        let dir = node();
        let vibrator = SysfsVibrator::open(dir.path()).unwrap();
        let mut bridge = HapticsBridge::new(GUEST_EPOCH, 1000, Box::new(vibrator));
        bridge.handle_status_descriptor(&bytes(SET_XINPUT), Instant::now());
        assert_eq!(read(&dir, "activate"), "0");
    }

    /// A frame from a previous device generation must be inert.
    #[test]
    fn a_stale_epoch_cannot_drive_the_motor() {
        let dir = node();
        let vibrator = SysfsVibrator::open(dir.path()).unwrap();
        // Host is on a different epoch than the frames were encoded for.
        let mut bridge = HapticsBridge::new(GUEST_EPOCH ^ 1, 1000, Box::new(vibrator));
        bridge.handle_status_descriptor(&bytes(GUEST_READY), Instant::now());
        bridge.handle_status_descriptor(&bytes(SET_XINPUT), Instant::now());
        assert_eq!(read(&dir, "activate"), "0");
    }
}

fn read_u32(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}
