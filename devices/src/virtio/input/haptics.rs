// Copyright 2026 The DroidVM Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Host side of the DroidVM XInputHID private haptic transport ("DVH1").
//!
//! This mirrors the guest contract in the Windows `vioinput` driver
//! (`vioinput/sys/HapticsCore.h`, `vioinput/docs/HAPTICS-ABI.md`). It is a
//! branch-private profile, not standard `EV_FF` and not an allocated virtio
//! feature, so nothing here may be forwarded to an ordinary evdev device.
//!
//! Frames are twelve 8-byte records. Each record is wire-identical to a
//! `virtio_input_event` (`type` u16, `code` u16, `value` u32): `type` is the
//! negotiated private event type, `code` is the record index 0..11, and
//! `value` carries the payload word. Record 11 carries CRC32C over the first
//! 88 bytes. The guest receives frames as twelve successive eventq events and
//! sends them as one 96-byte statusq descriptor.

use std::collections::VecDeque;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use std::time::Duration;
use std::time::Instant;

use data_model::Le16;
use data_model::SLe32;
use linux_input_sys::virtio_input_event;

/// `DVH1` in little-endian byte order.
pub const DVH_MAGIC: u32 = 0x3148_5644;
pub const DVH_VERSION: u16 = 1;
/// The capability record is exposed at config select/subsel and is 32 bytes.
pub const DVH_CAP_SELECT: u8 = 0x80;
pub const DVH_CAP_SUBSEL: u8 = 1;
pub const DVH_CAP_BYTES: u16 = 32;
/// Private event type. Must be > 0x1f so it can never alias a real `EV_*`.
pub const DVH_EVENT_TYPE: u16 = 0xff80;
pub const DVH_PROFILE: u16 = 1;
/// bit0 full XInputHID output, bit1 host-enforced lease + stop on reset,
/// bit2 READY/STATUS/REVOKE control. The guest rejects any other combination.
pub const DVH_FLAGS: u32 = 7;

pub const DVH_RECORD_BYTES: usize = 8;
pub const DVH_RECORDS: usize = 12;
pub const DVH_FRAME_BYTES: usize = DVH_RECORD_BYTES * DVH_RECORDS;

pub const DVH_MIN_LEASE_MS: u32 = 100;
/// Default output lease. Long enough that an ordinary keepalive cadence never
/// races it, short enough that a guest which stops talking goes quiet quickly.
pub const DVH_DEFAULT_LEASE_MS: u32 = 1000;
pub const DVH_MAX_LEASE_MS: u32 = 5000;
/// The guest gives up on the handshake if `HOST_READY` takes longer than this.
pub const DVH_HOST_READY_DEADLINE_MS: u64 = 2000;

/// Opcodes. 4/5 are the superseded raw-rumble draft: the current profile never
/// emits them, but they stay reserved so a renumber can never silently alias.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum DvhOpcode {
    HostHello = 1,
    GuestReady = 2,
    HostReady = 3,
    SetRumble = 4,
    Keepalive = 5,
    Stop = 6,
    HostStatus = 7,
    HostRevoke = 8,
    Close = 9,
    SetXinputReport = 10,
    KeepaliveXinputReport = 11,
}

impl DvhOpcode {
    pub fn from_raw(value: u16) -> Option<DvhOpcode> {
        Some(match value {
            1 => DvhOpcode::HostHello,
            2 => DvhOpcode::GuestReady,
            3 => DvhOpcode::HostReady,
            4 => DvhOpcode::SetRumble,
            5 => DvhOpcode::Keepalive,
            6 => DvhOpcode::Stop,
            7 => DvhOpcode::HostStatus,
            8 => DvhOpcode::HostRevoke,
            9 => DvhOpcode::Close,
            10 => DvhOpcode::SetXinputReport,
            11 => DvhOpcode::KeepaliveXinputReport,
            _ => return None,
        })
    }
}

/// CRC32C (Castagnoli, reflected, init/xorout 0xffffffff).
///
/// Deliberately bit-identical to the guest's `DvhCrc32c`. This detects
/// corruption only; it is not authentication, so requests must still be bound
/// to a trusted VM/controller connection.
pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc: u32 = u32::MAX;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0x82f6_3b78 & mask);
        }
    }
    crc ^ u32::MAX
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct DvhMessage {
    pub opcode: u16,
    pub epoch: u64,
    pub sequence: u64,
    pub revision: u64,
    pub motors: u32,
    pub lease_ms: u32,
    pub detail: u32,
}

/// Mirrors the guest's `DvhXinputReportActive`.
///
/// `motors` is four 8-bit percentages; bytes 0..3 are left trigger, right
/// trigger, left body, right body. `detail` byte 0 is the actuator mask whose
/// bits 3..0 map onto that same order, byte 1 is duration in 10 ms units,
/// byte 2 is delay, byte 3 is repeat count.
pub fn xinput_report_active(motors: u32, detail: u32) -> bool {
    // The mask lives in the low nibble; the high nibble is reserved.
    if (detail & 0xf0) != 0 || (detail & 0x0f) == 0 {
        return false;
    }
    let mut active = false;
    for i in 0..4 {
        let magnitude = (motors >> (i * 8)) & 0xff;
        if magnitude > 100 {
            return false;
        }
        if (detail & (8 >> i)) != 0 && magnitude != 0 {
            active = true;
        }
    }
    // A zero duration is not a valid finite effect.
    active && ((detail >> 8) & 0xff) != 0
}

impl DvhMessage {
    /// Mirrors the guest's `DvhMessageValid`. Anything this rejects must not
    /// change host state.
    pub fn is_valid(&self) -> bool {
        if self.epoch == 0 || self.sequence == 0 {
            return false;
        }
        match DvhOpcode::from_raw(self.opcode) {
            Some(DvhOpcode::HostHello) | Some(DvhOpcode::GuestReady)
            | Some(DvhOpcode::HostReady) => {
                self.revision == 0 && self.motors == 0 && self.lease_ms == 0 && self.detail == 0
            }
            Some(DvhOpcode::SetRumble) | Some(DvhOpcode::Keepalive) => {
                self.revision != 0
                    && self.motors != 0
                    && self.lease_ms != 0
                    && self.lease_ms <= DVH_MAX_LEASE_MS
                    && self.detail == 0
            }
            Some(DvhOpcode::SetXinputReport) | Some(DvhOpcode::KeepaliveXinputReport) => {
                self.revision != 0
                    && self.lease_ms != 0
                    && self.lease_ms <= DVH_MAX_LEASE_MS
                    && xinput_report_active(self.motors, self.detail)
            }
            Some(DvhOpcode::Stop) => {
                self.revision != 0 && self.motors == 0 && self.lease_ms == 0 && self.detail == 0
            }
            Some(DvhOpcode::Close) => {
                self.motors == 0 && self.lease_ms == 0 && self.detail == 0
            }
            Some(DvhOpcode::HostStatus) | Some(DvhOpcode::HostRevoke) => {
                self.motors == 0 && self.lease_ms == 0 && self.detail <= 8
            }
            None => false,
        }
    }

    /// Serialize one complete frame. Returns `None` rather than emitting a
    /// frame the guest would reject.
    pub fn encode(&self, event_type: u16) -> Option<[u8; DVH_FRAME_BYTES]> {
        if event_type <= 0x1f || !self.is_valid() {
            return None;
        }
        let words: [u32; DVH_RECORDS - 1] = [
            DVH_VERSION as u32 | ((self.opcode as u32) << 16),
            self.epoch as u32,
            (self.epoch >> 32) as u32,
            self.sequence as u32,
            (self.sequence >> 32) as u32,
            self.revision as u32,
            (self.revision >> 32) as u32,
            self.motors,
            self.lease_ms,
            self.detail,
            0,
        ];
        let mut out = [0u8; DVH_FRAME_BYTES];
        for (i, record) in out.chunks_exact_mut(DVH_RECORD_BYTES).enumerate() {
            record[0..2].copy_from_slice(&event_type.to_le_bytes());
            record[2..4].copy_from_slice(&(i as u16).to_le_bytes());
            if i < DVH_RECORDS - 1 {
                record[4..8].copy_from_slice(&words[i].to_le_bytes());
            }
        }
        // Record 11 covers the 88 bytes already written above.
        let crc = crc32c(&out[..(DVH_RECORDS - 1) * DVH_RECORD_BYTES]);
        out[DVH_FRAME_BYTES - 4..].copy_from_slice(&crc.to_le_bytes());
        Some(out)
    }

    /// Decode atomically: a malformed frame yields `None` and must leave the
    /// caller's state untouched.
    pub fn decode(bytes: &[u8], event_type: u16) -> Option<DvhMessage> {
        if bytes.len() != DVH_FRAME_BYTES || event_type <= 0x1f {
            return None;
        }
        let mut words = [0u32; DVH_RECORDS];
        for (i, record) in bytes.chunks_exact(DVH_RECORD_BYTES).enumerate() {
            let ty = u16::from_le_bytes([record[0], record[1]]);
            let code = u16::from_le_bytes([record[2], record[3]]);
            if ty != event_type || code != i as u16 {
                return None;
            }
            words[i] = u32::from_le_bytes([record[4], record[5], record[6], record[7]]);
        }
        if (words[0] & 0xffff) != DVH_VERSION as u32
            || words[10] != 0
            || words[11] != crc32c(&bytes[..(DVH_RECORDS - 1) * DVH_RECORD_BYTES])
        {
            return None;
        }
        let msg = DvhMessage {
            opcode: (words[0] >> 16) as u16,
            epoch: words[1] as u64 | ((words[2] as u64) << 32),
            sequence: words[3] as u64 | ((words[4] as u64) << 32),
            revision: words[5] as u64 | ((words[6] as u64) << 32),
            motors: words[7],
            lease_ms: words[8],
            detail: words[9],
        };
        if !msg.is_valid() {
            return None;
        }
        Some(msg)
    }
}

/// The 32-byte capability record the guest reads twice and compares before it
/// will negotiate at all.
pub fn build_capability(epoch: u64, lease_ms: u32) -> [u8; DVH_CAP_BYTES as usize] {
    let mut cap = [0u8; DVH_CAP_BYTES as usize];
    cap[0..4].copy_from_slice(&DVH_MAGIC.to_le_bytes());
    cap[4..6].copy_from_slice(&DVH_VERSION.to_le_bytes());
    cap[6..8].copy_from_slice(&DVH_CAP_BYTES.to_le_bytes());
    cap[8..12].copy_from_slice(&DVH_FLAGS.to_le_bytes());
    cap[12..14].copy_from_slice(&DVH_EVENT_TYPE.to_le_bytes());
    cap[14..16].copy_from_slice(&DVH_PROFILE.to_le_bytes());
    cap[16..24].copy_from_slice(&epoch.to_le_bytes());
    cap[24..28].copy_from_slice(&lease_ms.to_le_bytes());
    // cap[28..32] stays reserved/zero.
    cap
}

/// A decoded vibration command, in the units the Android side actually wants.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HapticEffect {
    /// Percentages 0..=100: left trigger, right trigger, left body, right body.
    /// A channel masked off by the actuator mask is reported as 0.
    pub magnitudes: [u8; 4],
    pub duration: Duration,
    pub delay: Duration,
    pub repeat: u8,
}

impl HapticEffect {
    /// `motors`/`detail` must already have passed `xinput_report_active`.
    fn from_report(motors: u32, detail: u32) -> HapticEffect {
        let mask = detail & 0x0f;
        let mut magnitudes = [0u8; 4];
        for (i, out) in magnitudes.iter_mut().enumerate() {
            if (mask & (8 >> i)) != 0 {
                *out = ((motors >> (i * 8)) & 0xff) as u8;
            }
        }
        HapticEffect {
            magnitudes,
            // Duration and delay are carried in 10 ms units.
            duration: Duration::from_millis(((detail >> 8) & 0xff) as u64 * 10),
            delay: Duration::from_millis(((detail >> 16) & 0xff) as u64 * 10),
            repeat: ((detail >> 24) & 0xff) as u8,
        }
    }

    /// Wall-clock span the host must stop by on its own, independent of any
    /// guest keepalive: delay, then `repeat + 1` passes of `duration`.
    pub fn total_span(&self) -> Duration {
        self.delay + self.duration * (self.repeat as u32 + 1)
    }
}

/// Where a validated effect is actually played. Kept behind a trait so the
/// protocol can be tested without an Android vibrator, and so a lost sink is a
/// normal stop rather than a special case.
pub trait HapticSink: Send {
    /// Start or replace the current effect.
    fn play(&mut self, effect: &HapticEffect) -> anyhow::Result<()>;
    /// Stop immediately. Must be idempotent.
    fn stop(&mut self) -> anyhow::Result<()>;
}

/// What the worker should do after feeding a frame to the session.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SessionAction {
    /// Control frames to hand to the guest on the eventq, in order.
    pub replies: Vec<DvhMessage>,
}

impl SessionAction {
    fn reply(msg: DvhMessage) -> SessionAction {
        SessionAction {
            replies: vec![msg],
        }
    }
}

/// Detail codes for `HOST_STATUS`. 1 is the normal end of a finite effect;
/// 2..=8 are faults. The guest treats anything > 8 as malformed.
pub const DVH_STATUS_FINISHED: u32 = 1;
pub const DVH_STATUS_SINK_FAILED: u32 = 2;
pub const DVH_STATUS_LEASE_EXPIRED: u32 = 3;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SessionState {
    /// Waiting for `GUEST_READY` against the current epoch.
    Idle,
    /// Handshake complete; output permitted.
    Ready,
    /// Session withdrawn. Only a device reset with a fresh epoch recovers it.
    Revoked,
}

/// Host half of one DVH1 session.
///
/// Owns the epoch, both sequence domains, the lease clock and the finite-effect
/// deadline. Every rule the guest relies on for safety is enforced here rather
/// than trusted: a stale epoch, a replayed sequence, an unknown-direction
/// opcode or an expired lease can never start or extend a vibration.
pub struct HapticSession {
    state: SessionState,
    epoch: u64,
    /// Host's own monotonic sequence. Never reused, never wrapped.
    host_sequence: u64,
    /// Highest guest sequence accepted so far; the guest shares one domain
    /// between handshake and vibration.
    guest_sequence: u64,
    /// Revision of the effect currently playing, if any.
    active_revision: Option<u64>,
    lease_ms: u32,
    /// Lease deadline for the running effect; the host stops at this instant
    /// unless the guest renews first.
    lease_deadline: Option<Instant>,
    /// Independent end of a finite effect. A same-revision keepalive renews the
    /// lease but must never push this out.
    effect_deadline: Option<Instant>,
}

impl HapticSession {
    pub fn new(epoch: u64, lease_ms: u32) -> HapticSession {
        HapticSession {
            state: SessionState::Idle,
            epoch,
            host_sequence: 0,
            guest_sequence: 0,
            active_revision: None,
            lease_ms: lease_ms.clamp(DVH_MIN_LEASE_MS, DVH_MAX_LEASE_MS),
            lease_deadline: None,
            effect_deadline: None,
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn lease_ms(&self) -> u32 {
        self.lease_ms
    }

    pub fn capability(&self) -> [u8; DVH_CAP_BYTES as usize] {
        build_capability(self.epoch, self.lease_ms)
    }

    fn next_host_sequence(&mut self) -> u64 {
        self.host_sequence += 1;
        self.host_sequence
    }

    /// Build a host->guest control frame.
    ///
    /// `revision` matters: the guest drops a `HOST_STATUS` whose revision does
    /// not equal the effect it is currently playing, so a status about an
    /// effect must carry that effect's revision. `HOST_READY` and
    /// `HOST_REVOKE` are session-scoped and carry zero.
    fn control(&mut self, opcode: DvhOpcode, detail: u32, revision: u64) -> DvhMessage {
        DvhMessage {
            opcode: opcode as u16,
            epoch: self.epoch,
            sequence: self.next_host_sequence(),
            revision,
            motors: 0,
            lease_ms: 0,
            detail,
        }
    }

    /// Drop any running effect and clear both deadlines. Used by every stop
    /// path so none of them can leave the motor latched on.
    fn halt(&mut self, sink: &mut dyn HapticSink) {
        self.active_revision = None;
        self.lease_deadline = None;
        self.effect_deadline = None;
        if let Err(e) = sink.stop() {
            base::error!("haptics: sink stop failed: {:#}", e);
        }
    }

    /// Feed one validated-on-the-wire frame from the guest's statusq.
    pub fn handle_guest_frame(
        &mut self,
        msg: &DvhMessage,
        now: Instant,
        sink: &mut dyn HapticSink,
    ) -> SessionAction {
        // A frame for any other epoch is a leftover from a previous device
        // generation and must not touch state.
        if msg.epoch != self.epoch {
            return SessionAction::default();
        }
        // One shared, strictly increasing guest domain. Replays are inert.
        if msg.sequence <= self.guest_sequence {
            return SessionAction::default();
        }
        let Some(opcode) = DvhOpcode::from_raw(msg.opcode) else {
            return SessionAction::default();
        };
        // Host-direction opcodes must never arrive from the guest.
        if matches!(
            opcode,
            DvhOpcode::HostHello
                | DvhOpcode::HostReady
                | DvhOpcode::HostStatus
                | DvhOpcode::HostRevoke
        ) {
            return SessionAction::default();
        }
        self.guest_sequence = msg.sequence;

        match opcode {
            DvhOpcode::GuestReady => {
                if self.state == SessionState::Revoked {
                    // Recovery requires a new epoch, so stay silent rather than
                    // implying the session came back.
                    return SessionAction::default();
                }
                self.state = SessionState::Ready;
                self.halt(sink);
                let reply = self.control(DvhOpcode::HostReady, 0, 0);
                SessionAction::reply(reply)
            }
            DvhOpcode::Close => {
                // Session-level stop. A later stale keepalive must not revive it.
                self.halt(sink);
                if self.state == SessionState::Ready {
                    self.state = SessionState::Idle;
                }
                SessionAction::default()
            }
            DvhOpcode::Stop => {
                if self.state != SessionState::Ready {
                    return SessionAction::default();
                }
                self.halt(sink);
                SessionAction::default()
            }
            DvhOpcode::SetXinputReport => {
                if self.state != SessionState::Ready {
                    return SessionAction::default();
                }
                let effect = HapticEffect::from_report(msg.motors, msg.detail);
                if let Err(e) = sink.play(&effect) {
                    base::error!("haptics: sink play failed: {:#}", e);
                    self.halt(sink);
                    // Report against the revision the guest just sent, or it
                    // will not associate the fault with its own effect.
                    let reply =
                        self.control(DvhOpcode::HostStatus, DVH_STATUS_SINK_FAILED, msg.revision);
                    return SessionAction::reply(reply);
                }
                self.active_revision = Some(msg.revision);
                self.lease_deadline = Some(now + Duration::from_millis(self.lease_ms as u64));
                self.effect_deadline = Some(now + effect.total_span());
                SessionAction::default()
            }
            DvhOpcode::KeepaliveXinputReport => {
                // Renews the lease only. It may not restart playback or push
                // out the finite-effect deadline.
                if self.state != SessionState::Ready
                    || self.active_revision != Some(msg.revision)
                {
                    return SessionAction::default();
                }
                self.lease_deadline = Some(now + Duration::from_millis(self.lease_ms as u64));
                SessionAction::default()
            }
            // Superseded raw-rumble draft: reserved, never acted on.
            DvhOpcode::SetRumble | DvhOpcode::Keepalive => SessionAction::default(),
            DvhOpcode::HostHello
            | DvhOpcode::HostReady
            | DvhOpcode::HostStatus
            | DvhOpcode::HostRevoke => SessionAction::default(),
        }
    }

    /// Independent host-side expiry. Must be called regularly: the guest may be
    /// paused, crashed or disconnected, in which case no STOP is ever sent.
    pub fn poll_timeouts(&mut self, now: Instant, sink: &mut dyn HapticSink) -> SessionAction {
        if self.active_revision.is_none() {
            return SessionAction::default();
        }
        // `halt` clears the revision, so capture it before stopping.
        let revision = self.active_revision.unwrap_or(0);
        // Finite effect reaching its own end is a normal completion, and takes
        // priority over the lease so the guest is told to stop renewing.
        if self.effect_deadline.is_some_and(|d| now >= d) {
            self.halt(sink);
            let reply = self.control(DvhOpcode::HostStatus, DVH_STATUS_FINISHED, revision);
            return SessionAction::reply(reply);
        }
        if self.lease_deadline.is_some_and(|d| now >= d) {
            self.halt(sink);
            let reply = self.control(DvhOpcode::HostStatus, DVH_STATUS_LEASE_EXPIRED, revision);
            return SessionAction::reply(reply);
        }
        SessionAction::default()
    }

    /// When the next timer-driven stop is due, so the worker can size its wait.
    pub fn next_deadline(&self) -> Option<Instant> {
        match (self.effect_deadline, self.lease_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Withdraw the session: broker lost, user disabled haptics, VM going away.
    pub fn revoke(&mut self, sink: &mut dyn HapticSink) -> SessionAction {
        self.halt(sink);
        if self.state == SessionState::Revoked {
            return SessionAction::default();
        }
        self.state = SessionState::Revoked;
        let reply = self.control(DvhOpcode::HostRevoke, 0, 0);
        SessionAction::reply(reply)
    }

    /// Device reset: a brand-new epoch, never a reused one, and nothing
    /// carried over from the previous generation.
    pub fn reset(&mut self, epoch: u64, sink: &mut dyn HapticSink) {
        self.halt(sink);
        self.state = SessionState::Idle;
        self.epoch = epoch;
        self.host_sequence = 0;
        self.guest_sequence = 0;
    }
}

/// A fresh session epoch. Never zero and never reused within a boot, which is
/// what lets the guest tell a new device generation from a stale one.
pub fn next_epoch() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    // The counter keeps successive resets distinct even inside one clock tick.
    base.wrapping_add(COUNTER.fetch_add(1, Ordering::Relaxed)) | 1
}

/// Everything the device needs to bring up a haptic session.
///
/// The sink is built per activation so a reset gets a clean one, and so the
/// device can be re-activated after the previous worker has been torn down.
pub struct HapticsSetup {
    pub epoch: u64,
    pub lease_ms: u32,
    pub sink_factory: Box<dyn Fn() -> Box<dyn HapticSink> + Send>,
}

impl HapticsSetup {
    pub fn capability(&self) -> [u8; DVH_CAP_BYTES as usize] {
        build_capability(self.epoch, self.lease_ms)
    }
}

/// Sink that only reports what it was asked to do.
///
/// This is the honest default until an Android vibrator bridge exists: the
/// guest negotiates, handshakes and sends real reports, and the host logs them
/// instead of pretending the phone buzzed.
pub struct LoggingSink {
    name: String,
}

impl LoggingSink {
    pub fn new(name: String) -> LoggingSink {
        LoggingSink { name }
    }
}

impl HapticSink for LoggingSink {
    fn play(&mut self, effect: &HapticEffect) -> anyhow::Result<()> {
        base::info!(
            "haptics[{}]: play lt={} rt={} lb={} rb={} duration={:?} delay={:?} repeat={} \
             (no vibrator bridge; not played)",
            self.name,
            effect.magnitudes[0],
            effect.magnitudes[1],
            effect.magnitudes[2],
            effect.magnitudes[3],
            effect.duration,
            effect.delay,
            effect.repeat,
        );
        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        base::info!("haptics[{}]: stop", self.name);
        Ok(())
    }
}

/// Glue between the session state machine and the two virtqueues.
///
/// Owns the outbound control frames so the worker only has to ask whether
/// anything is pending. Host->guest frames go out as twelve successive eventq
/// events; guest->host frames arrive as one 96-byte statusq descriptor.
pub struct HapticsBridge {
    session: HapticSession,
    sink: Box<dyn HapticSink>,
    pending: VecDeque<virtio_input_event>,
}

/// What `handle_status_descriptor` decided about a statusq descriptor.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StatusVerdict {
    /// A private frame. Already handled; it must not reach the event source.
    Consumed,
    /// Carried the private event type but was malformed. Dropped, not forwarded.
    Rejected,
    /// Ordinary status events; the caller should forward them as before.
    NotOurs,
}

impl HapticsBridge {
    pub fn new(epoch: u64, lease_ms: u32, sink: Box<dyn HapticSink>) -> HapticsBridge {
        HapticsBridge {
            session: HapticSession::new(epoch, lease_ms),
            sink,
            pending: VecDeque::new(),
        }
    }

    pub fn capability(&self) -> [u8; DVH_CAP_BYTES as usize] {
        self.session.capability()
    }

    /// Split each reply frame into the twelve events the guest reassembles.
    fn queue(&mut self, action: SessionAction) {
        for msg in action.replies {
            let Some(frame) = msg.encode(DVH_EVENT_TYPE) else {
                // Refusing to emit beats emitting something the guest rejects.
                base::error!("haptics: refusing to send invalid control frame {msg:?}");
                continue;
            };
            for record in frame.chunks_exact(DVH_RECORD_BYTES) {
                self.pending.push_back(virtio_input_event {
                    type_: Le16::from(u16::from_le_bytes([record[0], record[1]])),
                    code: Le16::from(u16::from_le_bytes([record[2], record[3]])),
                    value: SLe32::from(i32::from_le_bytes([
                        record[4], record[5], record[6], record[7],
                    ])),
                });
            }
        }
    }

    /// Classify and, when it is ours, act on one whole statusq descriptor.
    pub fn handle_status_descriptor(&mut self, bytes: &[u8], now: Instant) -> StatusVerdict {
        // Only the private event type is ours. Anything else is ordinary status
        // traffic and stays on its existing path.
        if bytes.len() < 2 || u16::from_le_bytes([bytes[0], bytes[1]]) != DVH_EVENT_TYPE {
            return StatusVerdict::NotOurs;
        }
        let Some(msg) = DvhMessage::decode(bytes, DVH_EVENT_TYPE) else {
            // Private type but bad framing, CRC, length or fields. Dropped
            // silently: forwarding it to an evdev device is forbidden.
            return StatusVerdict::Rejected;
        };
        let action = self.session.handle_guest_frame(&msg, now, self.sink.as_mut());
        self.queue(action);
        StatusVerdict::Consumed
    }

    /// Host-side expiry. Must be driven even when the guest has gone quiet.
    pub fn poll_timeouts(&mut self, now: Instant) {
        let action = self.session.poll_timeouts(now, self.sink.as_mut());
        self.queue(action);
    }

    /// Withdraw the session: broker lost, haptics disabled, VM going away.
    pub fn revoke(&mut self) {
        let action = self.session.revoke(self.sink.as_mut());
        self.queue(action);
    }

    /// Device reset. The epoch must be fresh; reusing one is forbidden.
    pub fn reset(&mut self, epoch: u64) {
        self.session.reset(epoch, self.sink.as_mut());
        self.pending.clear();
    }

    /// Unconditional stop, with no frame to the guest. For teardown paths where
    /// the guest will never read another event.
    pub fn stop_now(&mut self) {
        self.session.halt(self.sink.as_mut());
        self.pending.clear();
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.session.next_deadline()
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn pop_pending(&mut self) -> Option<virtio_input_event> {
        self.pending.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what the protocol asked the motor to do.
    #[derive(Default)]
    struct FakeSink {
        played: Vec<HapticEffect>,
        stops: usize,
        fail_play: bool,
    }

    impl HapticSink for FakeSink {
        fn play(&mut self, effect: &HapticEffect) -> anyhow::Result<()> {
            if self.fail_play {
                anyhow::bail!("injected failure");
            }
            self.played.push(*effect);
            Ok(())
        }
        fn stop(&mut self) -> anyhow::Result<()> {
            self.stops += 1;
            Ok(())
        }
    }

    /// detail: mask in the low nibble, then duration/delay/repeat in 10 ms units.
    fn detail(mask: u32, duration: u32, delay: u32, repeat: u32) -> u32 {
        mask | (duration << 8) | (delay << 16) | (repeat << 24)
    }

    fn xinput(seq: u64, revision: u64, motors: u32, detail: u32) -> DvhMessage {
        DvhMessage {
            opcode: DvhOpcode::SetXinputReport as u16,
            epoch: 7,
            sequence: seq,
            revision,
            motors,
            lease_ms: 1000,
            detail,
        }
    }

    fn ready_session(sink: &mut FakeSink, now: Instant) -> HapticSession {
        let mut session = HapticSession::new(7, 1000);
        let guest_ready = DvhMessage {
            opcode: DvhOpcode::GuestReady as u16,
            epoch: 7,
            sequence: 1,
            ..Default::default()
        };
        let action = session.handle_guest_frame(&guest_ready, now, sink);
        assert_eq!(action.replies.len(), 1);
        assert_eq!(action.replies[0].opcode, DvhOpcode::HostReady as u16);
        session
    }

    #[test]
    fn crc32c_matches_known_vectors() {
        // Castagnoli check values, distinct from CRC-32/IEEE.
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
        assert_eq!(crc32c(b"a"), 0xc1d0_4330);
    }

    #[test]
    fn capability_layout_is_exact() {
        let cap = build_capability(0x1122_3344_5566_7788, 250);
        assert_eq!(cap.len(), 32);
        assert_eq!(&cap[0..4], b"DVH1");
        assert_eq!(u16::from_le_bytes([cap[4], cap[5]]), 1);
        assert_eq!(u16::from_le_bytes([cap[6], cap[7]]), 32);
        assert_eq!(u32::from_le_bytes([cap[8], cap[9], cap[10], cap[11]]), 7);
        assert_eq!(u16::from_le_bytes([cap[12], cap[13]]), 0xff80);
        assert_eq!(u16::from_le_bytes([cap[14], cap[15]]), 1);
        assert_eq!(
            u64::from_le_bytes(cap[16..24].try_into().unwrap()),
            0x1122_3344_5566_7788
        );
        assert_eq!(
            u32::from_le_bytes(cap[24..28].try_into().unwrap()),
            250
        );
        assert_eq!(&cap[28..32], &[0, 0, 0, 0]);
    }

    #[test]
    fn frame_round_trips() {
        let msg = xinput(9, 3, 0x0000_6400, detail(0x4, 5, 0, 0));
        let frame = msg.encode(DVH_EVENT_TYPE).expect("encode");
        assert_eq!(frame.len(), DVH_FRAME_BYTES);
        assert_eq!(DvhMessage::decode(&frame, DVH_EVENT_TYPE), Some(msg));
    }

    #[test]
    fn every_single_bit_flip_is_rejected() {
        let msg = xinput(9, 3, 0x0000_6400, detail(0x4, 5, 0, 0));
        let frame = msg.encode(DVH_EVENT_TYPE).expect("encode");
        for byte in 0..frame.len() {
            for bit in 0..8 {
                let mut corrupt = frame;
                corrupt[byte] ^= 1 << bit;
                assert_eq!(
                    DvhMessage::decode(&corrupt, DVH_EVENT_TYPE),
                    None,
                    "byte {byte} bit {bit} accepted"
                );
            }
        }
    }

    #[test]
    fn decode_rejects_wrong_event_type_and_length() {
        let msg = xinput(9, 3, 0x0000_6400, detail(0x4, 5, 0, 0));
        let frame = msg.encode(DVH_EVENT_TYPE).expect("encode");
        assert_eq!(DvhMessage::decode(&frame, 0xff81), None);
        assert_eq!(DvhMessage::decode(&frame[..88], DVH_EVENT_TYPE), None);
        // A private type must never collide with a real EV_* code.
        assert_eq!(DvhMessage::decode(&frame, 0x1f), None);
    }

    #[test]
    fn invalid_reports_never_encode() {
        // Magnitude over 100%.
        assert!(xinput(9, 3, 0x0000_ff00, detail(0x4, 5, 0, 0))
            .encode(DVH_EVENT_TYPE)
            .is_none());
        // Reserved high nibble of the actuator mask set.
        assert!(xinput(9, 3, 0x0000_6400, detail(0x14, 5, 0, 0))
            .encode(DVH_EVENT_TYPE)
            .is_none());
        // Zero duration.
        assert!(xinput(9, 3, 0x0000_6400, detail(0x4, 0, 0, 0))
            .encode(DVH_EVENT_TYPE)
            .is_none());
        // Mask selects a channel whose magnitude is zero.
        assert!(xinput(9, 3, 0x0000_0000, detail(0x4, 5, 0, 0))
            .encode(DVH_EVENT_TYPE)
            .is_none());
        // Lease beyond the negotiated ceiling.
        let mut over = xinput(9, 3, 0x0000_6400, detail(0x4, 5, 0, 0));
        over.lease_ms = DVH_MAX_LEASE_MS + 1;
        assert!(over.encode(DVH_EVENT_TYPE).is_none());
        // Sequence zero is never valid.
        assert!(xinput(0, 3, 0x0000_6400, detail(0x4, 5, 0, 0))
            .encode(DVH_EVENT_TYPE)
            .is_none());
    }

    #[test]
    fn report_decodes_to_masked_channels_and_units() {
        // Mask 0x6 selects right trigger (bit2) and left body (bit1).
        let effect = HapticEffect::from_report(0x0000_3264, detail(0x6, 5, 2, 1));
        assert_eq!(effect.magnitudes, [0, 0x32, 0, 0]);
        assert_eq!(effect.duration, Duration::from_millis(50));
        assert_eq!(effect.delay, Duration::from_millis(20));
        assert_eq!(effect.repeat, 1);
        // delay + (repeat + 1) * duration
        assert_eq!(effect.total_span(), Duration::from_millis(120));
    }

    #[test]
    fn output_is_refused_before_the_handshake() {
        let mut sink = FakeSink::default();
        let now = Instant::now();
        let mut session = HapticSession::new(7, 1000);
        let action =
            session.handle_guest_frame(&xinput(2, 1, 0x0000_6400, detail(0x4, 5, 0, 0)), now, &mut sink);
        assert!(action.replies.is_empty());
        assert!(sink.played.is_empty());
    }

    #[test]
    fn handshake_then_play() {
        let mut sink = FakeSink::default();
        let now = Instant::now();
        let mut session = ready_session(&mut sink, now);
        session.handle_guest_frame(&xinput(2, 1, 0x0000_6400, detail(0x4, 5, 0, 0)), now, &mut sink);
        assert_eq!(sink.played.len(), 1);
        assert_eq!(sink.played[0].magnitudes, [0, 0x64, 0, 0]);
    }

    #[test]
    fn stale_epoch_and_replayed_sequence_are_inert() {
        let mut sink = FakeSink::default();
        let now = Instant::now();
        let mut session = ready_session(&mut sink, now);

        let mut stale_epoch = xinput(2, 1, 0x0000_6400, detail(0x4, 5, 0, 0));
        stale_epoch.epoch = 6;
        session.handle_guest_frame(&stale_epoch, now, &mut sink);
        assert!(sink.played.is_empty());

        // Sequence 1 was consumed by GUEST_READY; the domain is shared.
        session.handle_guest_frame(&xinput(1, 1, 0x0000_6400, detail(0x4, 5, 0, 0)), now, &mut sink);
        assert!(sink.played.is_empty());
    }

    #[test]
    fn host_direction_opcodes_from_guest_are_ignored() {
        let mut sink = FakeSink::default();
        let now = Instant::now();
        let mut session = ready_session(&mut sink, now);
        let spoofed = DvhMessage {
            opcode: DvhOpcode::HostReady as u16,
            epoch: 7,
            sequence: 2,
            ..Default::default()
        };
        assert!(session.handle_guest_frame(&spoofed, now, &mut sink).replies.is_empty());
        // The spoofed frame must not consume the guest sequence either.
        session.handle_guest_frame(&xinput(2, 1, 0x0000_6400, detail(0x4, 5, 0, 0)), now, &mut sink);
        assert_eq!(sink.played.len(), 1);
    }

    #[test]
    fn keepalive_renews_lease_without_extending_a_finite_effect() {
        let mut sink = FakeSink::default();
        let start = Instant::now();
        let mut session = ready_session(&mut sink, start);
        // 50 ms effect under a 1000 ms lease.
        session.handle_guest_frame(
            &xinput(2, 1, 0x0000_6400, detail(0x4, 5, 0, 0)),
            start,
            &mut sink,
        );

        let mut keepalive = xinput(3, 1, 0x0000_6400, detail(0x4, 5, 0, 0));
        keepalive.opcode = DvhOpcode::KeepaliveXinputReport as u16;
        let at_40ms = start + Duration::from_millis(40);
        session.handle_guest_frame(&keepalive, at_40ms, &mut sink);
        assert!(session.poll_timeouts(at_40ms, &mut sink).replies.is_empty());

        // The effect still ends on its own schedule, not the renewed lease.
        let at_60ms = start + Duration::from_millis(60);
        let action = session.poll_timeouts(at_60ms, &mut sink);
        assert_eq!(action.replies.len(), 1);
        assert_eq!(action.replies[0].opcode, DvhOpcode::HostStatus as u16);
        assert_eq!(action.replies[0].detail, DVH_STATUS_FINISHED);
        assert_eq!(sink.stops, 2); // one from the handshake, one from the stop
    }

    #[test]
    fn lease_expiry_stops_a_silent_guest() {
        let mut sink = FakeSink::default();
        let start = Instant::now();
        let mut session = ready_session(&mut sink, start);
        // A long effect: 200 * 10 ms = 2 s, well past the 1 s lease.
        session.handle_guest_frame(
            &xinput(2, 1, 0x0000_6400, detail(0x4, 200, 0, 0)),
            start,
            &mut sink,
        );
        let at_1100ms = start + Duration::from_millis(1100);
        let action = session.poll_timeouts(at_1100ms, &mut sink);
        assert_eq!(action.replies.len(), 1);
        assert_eq!(action.replies[0].detail, DVH_STATUS_LEASE_EXPIRED);
    }

    #[test]
    fn keepalive_for_another_revision_is_ignored() {
        let mut sink = FakeSink::default();
        let start = Instant::now();
        let mut session = ready_session(&mut sink, start);
        session.handle_guest_frame(
            &xinput(2, 1, 0x0000_6400, detail(0x4, 200, 0, 0)),
            start,
            &mut sink,
        );
        let mut keepalive = xinput(3, 99, 0x0000_6400, detail(0x4, 200, 0, 0));
        keepalive.opcode = DvhOpcode::KeepaliveXinputReport as u16;
        session.handle_guest_frame(&keepalive, start + Duration::from_millis(900), &mut sink);
        // The lease was not renewed, so the original deadline still fires.
        let action = session.poll_timeouts(start + Duration::from_millis(1100), &mut sink);
        assert_eq!(action.replies.len(), 1);
        assert_eq!(action.replies[0].detail, DVH_STATUS_LEASE_EXPIRED);
    }

    #[test]
    fn close_then_stale_keepalive_does_not_resume() {
        let mut sink = FakeSink::default();
        let start = Instant::now();
        let mut session = ready_session(&mut sink, start);
        session.handle_guest_frame(
            &xinput(2, 1, 0x0000_6400, detail(0x4, 200, 0, 0)),
            start,
            &mut sink,
        );
        let close = DvhMessage {
            opcode: DvhOpcode::Close as u16,
            epoch: 7,
            sequence: 3,
            revision: 1,
            ..Default::default()
        };
        session.handle_guest_frame(&close, start, &mut sink);

        let mut keepalive = xinput(4, 1, 0x0000_6400, detail(0x4, 200, 0, 0));
        keepalive.opcode = DvhOpcode::KeepaliveXinputReport as u16;
        session.handle_guest_frame(&keepalive, start, &mut sink);
        assert!(session.next_deadline().is_none());
        assert_eq!(sink.played.len(), 1);
    }

    #[test]
    fn revoked_session_refuses_output_until_a_new_epoch() {
        let mut sink = FakeSink::default();
        let start = Instant::now();
        let mut session = ready_session(&mut sink, start);
        let action = session.revoke(&mut sink);
        assert_eq!(action.replies[0].opcode, DvhOpcode::HostRevoke as u16);

        session.handle_guest_frame(
            &xinput(2, 1, 0x0000_6400, detail(0x4, 5, 0, 0)),
            start,
            &mut sink,
        );
        assert!(sink.played.is_empty());

        // Re-handshaking on the same epoch must not recover it.
        let guest_ready = DvhMessage {
            opcode: DvhOpcode::GuestReady as u16,
            epoch: 7,
            sequence: 3,
            ..Default::default()
        };
        assert!(session
            .handle_guest_frame(&guest_ready, start, &mut sink)
            .replies
            .is_empty());

        // A reset with a fresh epoch does.
        session.reset(8, &mut sink);
        let guest_ready = DvhMessage {
            opcode: DvhOpcode::GuestReady as u16,
            epoch: 8,
            sequence: 1,
            ..Default::default()
        };
        let action = session.handle_guest_frame(&guest_ready, start, &mut sink);
        assert_eq!(action.replies[0].opcode, DvhOpcode::HostReady as u16);
    }

    #[test]
    fn sink_failure_reports_a_fault_and_stops() {
        let mut sink = FakeSink::default();
        let start = Instant::now();
        let mut session = ready_session(&mut sink, start);
        sink.fail_play = true;
        let action = session.handle_guest_frame(
            &xinput(2, 1, 0x0000_6400, detail(0x4, 5, 0, 0)),
            start,
            &mut sink,
        );
        assert_eq!(action.replies[0].detail, DVH_STATUS_SINK_FAILED);
        assert!(session.next_deadline().is_none());
    }

    #[test]
    fn host_sequence_is_monotonic_and_never_zero() {
        let mut sink = FakeSink::default();
        let start = Instant::now();
        let mut session = ready_session(&mut sink, start);
        let first = session.control(DvhOpcode::HostStatus, DVH_STATUS_FINISHED, 1).sequence;
        let second = session.control(DvhOpcode::HostStatus, DVH_STATUS_FINISHED, 1).sequence;
        assert!(first >= 1 && second > first);
    }

    fn bridge() -> HapticsBridge {
        HapticsBridge::new(7, 1000, Box::new(LoggingSink::new("test".to_string())))
    }

    /// Frames leave as exactly twelve records the guest can reassemble.
    #[test]
    fn control_frames_split_into_twelve_ordered_events() {
        let mut b = bridge();
        let guest_ready = DvhMessage {
            opcode: DvhOpcode::GuestReady as u16,
            epoch: 7,
            sequence: 1,
            ..Default::default()
        };
        let frame = guest_ready.encode(DVH_EVENT_TYPE).expect("encode");
        assert_eq!(
            b.handle_status_descriptor(&frame, Instant::now()),
            StatusVerdict::Consumed
        );

        let mut out = Vec::new();
        while let Some(evt) = b.pop_pending() {
            out.push(evt);
        }
        assert_eq!(out.len(), DVH_RECORDS);
        for (i, evt) in out.iter().enumerate() {
            assert_eq!(evt.type_.to_native(), DVH_EVENT_TYPE);
            assert_eq!(evt.code.to_native(), i as u16);
        }
        // Reassembling the records must yield the HOST_READY the guest expects.
        let mut rebuilt = Vec::new();
        for evt in &out {
            rebuilt.extend_from_slice(&evt.type_.to_native().to_le_bytes());
            rebuilt.extend_from_slice(&evt.code.to_native().to_le_bytes());
            rebuilt.extend_from_slice(&evt.value.to_native().to_le_bytes());
        }
        let decoded = DvhMessage::decode(&rebuilt, DVH_EVENT_TYPE).expect("decode");
        assert_eq!(decoded.opcode, DvhOpcode::HostReady as u16);
        assert_eq!(decoded.epoch, 7);
    }

    #[test]
    fn ordinary_status_traffic_is_left_alone() {
        let mut b = bridge();
        // An EV_LED event: not ours, must go back to the caller to forward.
        let evt = [0x11, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00];
        assert_eq!(
            b.handle_status_descriptor(&evt, Instant::now()),
            StatusVerdict::NotOurs
        );
        assert!(!b.has_pending());
    }

    #[test]
    fn corrupt_private_frame_is_rejected_not_forwarded() {
        let mut b = bridge();
        let guest_ready = DvhMessage {
            opcode: DvhOpcode::GuestReady as u16,
            epoch: 7,
            sequence: 1,
            ..Default::default()
        };
        let mut frame = guest_ready.encode(DVH_EVENT_TYPE).expect("encode");
        frame[90] ^= 0x01; // break the CRC
        assert_eq!(
            b.handle_status_descriptor(&frame, Instant::now()),
            StatusVerdict::Rejected
        );
        assert!(!b.has_pending());
    }

    /// A truncated frame carries the private type but is not a frame. It must
    /// never be treated as ordinary events and forwarded to evdev.
    #[test]
    fn truncated_private_frame_is_not_forwarded() {
        let mut b = bridge();
        let guest_ready = DvhMessage {
            opcode: DvhOpcode::GuestReady as u16,
            epoch: 7,
            sequence: 1,
            ..Default::default()
        };
        let frame = guest_ready.encode(DVH_EVENT_TYPE).expect("encode");
        assert_eq!(
            b.handle_status_descriptor(&frame[..48], Instant::now()),
            StatusVerdict::Rejected
        );
    }

    #[test]
    fn bridge_arms_a_deadline_only_while_an_effect_runs() {
        let mut b = bridge();
        let now = Instant::now();
        assert!(b.next_deadline().is_none());

        let guest_ready = DvhMessage {
            opcode: DvhOpcode::GuestReady as u16,
            epoch: 7,
            sequence: 1,
            ..Default::default()
        };
        b.handle_status_descriptor(&guest_ready.encode(DVH_EVENT_TYPE).unwrap(), now);
        assert!(b.next_deadline().is_none());

        let play = xinput(2, 1, 0x0000_6400, detail(0x4, 5, 0, 0));
        b.handle_status_descriptor(&play.encode(DVH_EVENT_TYPE).unwrap(), now);
        assert!(b.next_deadline().is_some());

        // Past the effect's own end the deadline clears and a status goes out.
        while b.pop_pending().is_some() {}
        b.poll_timeouts(now + Duration::from_millis(60));
        assert!(b.next_deadline().is_none());
        assert!(b.has_pending());
    }

    #[test]
    fn epochs_are_never_zero_and_never_repeat() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let e = next_epoch();
            assert_ne!(e, 0);
            assert!(seen.insert(e), "epoch {e} reused");
        }
    }

    /// The guest drops a HOST_STATUS whose revision does not match the effect
    /// it is playing, so every status must carry that effect's revision.
    #[test]
    fn host_status_carries_the_effect_revision() {
        let mut sink = FakeSink::default();
        let start = Instant::now();
        let mut session = ready_session(&mut sink, start);
        session.handle_guest_frame(
            &xinput(2, 0xabcd, 0x0000_6400, detail(0x4, 5, 0, 0)),
            start,
            &mut sink,
        );
        let action = session.poll_timeouts(start + Duration::from_millis(60), &mut sink);
        assert_eq!(action.replies[0].detail, DVH_STATUS_FINISHED);
        assert_eq!(action.replies[0].revision, 0xabcd);

        // A lease-expiry status must name its effect too.
        let mut session = ready_session(&mut sink, start);
        session.handle_guest_frame(
            &xinput(2, 0x1234, 0x0000_6400, detail(0x4, 200, 0, 0)),
            start,
            &mut sink,
        );
        let action = session.poll_timeouts(start + Duration::from_millis(1100), &mut sink);
        assert_eq!(action.replies[0].detail, DVH_STATUS_LEASE_EXPIRED);
        assert_eq!(action.replies[0].revision, 0x1234);

        // And so must a sink fault.
        let mut session = ready_session(&mut sink, start);
        sink.fail_play = true;
        let action = session.handle_guest_frame(
            &xinput(2, 0x5555, 0x0000_6400, detail(0x4, 5, 0, 0)),
            start,
            &mut sink,
        );
        assert_eq!(action.replies[0].revision, 0x5555);
        sink.fail_play = false;
    }

    /// Session-scoped frames carry no revision, and every emitted frame must
    /// survive the guest's own validity check.
    #[test]
    fn all_emitted_control_frames_are_encodable() {
        let mut sink = FakeSink::default();
        let start = Instant::now();
        let mut session = ready_session(&mut sink, start);
        session.handle_guest_frame(
            &xinput(2, 9, 0x0000_6400, detail(0x4, 5, 0, 0)),
            start,
            &mut sink,
        );
        let mut frames = session.poll_timeouts(start + Duration::from_millis(60), &mut sink).replies;
        frames.extend(session.revoke(&mut sink).replies);
        assert!(frames.len() >= 2);
        for msg in frames {
            assert!(
                msg.encode(DVH_EVENT_TYPE).is_some(),
                "host emitted a frame the guest would reject: {msg:?}"
            );
        }
    }

    #[test]
    fn superseded_raw_rumble_opcodes_do_nothing() {
        let mut sink = FakeSink::default();
        let start = Instant::now();
        let mut session = ready_session(&mut sink, start);
        let legacy = DvhMessage {
            opcode: DvhOpcode::SetRumble as u16,
            epoch: 7,
            sequence: 2,
            revision: 1,
            motors: 0xffff,
            lease_ms: 1000,
            detail: 0,
        };
        assert!(session.handle_guest_frame(&legacy, start, &mut sink).replies.is_empty());
        assert!(sink.played.is_empty());
    }
}
