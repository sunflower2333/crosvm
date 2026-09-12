// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Descriptor retirement with independent renderer and display completion.
//!
//! Renderer sequence numbers retain the existing backend's cumulative contract.
//! Display tickets are private to the frontend: a display completion must never
//! advance a renderer watermark, even when the guest used the same fence id.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// Virgl's global callback carries only a u32. These host-issued tokens never
/// contain a guest cookie and are never reused during the renderer lifetime.
/// Register before calling the renderer: it may call back synchronously.
#[derive(Default)]
pub(super) struct GlobalFenceTokens {
    last_issued: u32,
    pending: BTreeSet<u32>,
}

impl GlobalFenceTokens {
    pub(super) fn reserve(&mut self) -> Option<u32> {
        // Bound host bookkeeping even if a renderer stops making progress.
        if self.pending.len() >= 4096 {
            return None;
        }
        let token = self.last_issued.checked_add(1)?;
        self.last_issued = token;
        self.pending.insert(token);
        Some(token)
    }

    pub(super) fn cancel(&mut self, token: u32) {
        self.pending.remove(&token);
    }

    /// Virgl may merge callbacks. Only a known submitted token can advance
    /// progress; unknown, stale, truncated or failed submissions cannot.
    pub(super) fn complete(&mut self, token: u64) -> bool {
        let Ok(token) = u32::try_from(token) else {
            return false;
        };
        if !self.pending.remove(&token) {
            return false;
        }
        self.pending.retain(|id| *id > token);
        true
    }

    pub(super) fn last_issued(&self) -> u32 {
        self.last_issued
    }

    pub(super) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Restoring an older snapshot into a live renderer must not recycle any
    /// token it has observed, including tokens reserved for failed commands.
    pub(super) fn restore_last_issued(&mut self, last_issued: u32) -> bool {
        if !self.pending.is_empty() {
            return false;
        }
        self.last_issued = self.last_issued.max(last_issued);
        true
    }
}

enum Dependency {
    Renderer(u64),
    Display(u64),
    Complete,
}

/// Context callbacks carry all 64 bits. Allocate across all context lifetimes,
/// so recreating a guest context ID can never inherit an old token. The exact
/// context/ring tuple must match before a callback can advance that timeline.
#[derive(Default)]
pub(super) struct ContextFenceTokens {
    last_issued: u64,
    pending: BTreeMap<u64, (u32, u8)>,
}

impl ContextFenceTokens {
    pub(super) fn reserve(&mut self, ctx_id: u32, ring_idx: u8) -> Option<u64> {
        if self.pending.len() >= 4096 {
            return None;
        }
        let token = self.last_issued.checked_add(1)?;
        self.last_issued = token;
        self.pending.insert(token, (ctx_id, ring_idx));
        Some(token)
    }

    pub(super) fn cancel(&mut self, token: u64) {
        self.pending.remove(&token);
    }

    pub(super) fn complete(&mut self, ctx_id: u32, ring_idx: u8, token: u64) -> bool {
        let ring = (ctx_id, ring_idx);
        if self.pending.get(&token) != Some(&ring) {
            return false;
        }
        // Merging is valid within one backend timeline only.
        self.pending
            .retain(|id, pending_ring| *id > token || *pending_ring != ring);
        true
    }

    pub(super) fn contains(&self, ctx_id: u32, ring_idx: u8, token: u64) -> bool {
        self.pending.get(&token) == Some(&(ctx_id, ring_idx))
    }

    pub(super) fn has_context(&self, ctx_id: u32) -> bool {
        self.pending.values().any(|ring| ring.0 == ctx_id)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub(super) fn last_issued(&self) -> u64 {
        self.last_issued
    }

    pub(super) fn restore_last_issued(&mut self, last_issued: u64) -> bool {
        if !self.pending.is_empty() {
            return false;
        }
        self.last_issued = self.last_issued.max(last_issued);
        true
    }
}

struct Pending<R, T> {
    ring: R,
    dependency: Dependency,
    value: T,
}

pub(super) struct FenceQueue<R, T> {
    pending: Vec<Pending<R, T>>,
    pub(super) completed_renderer: BTreeMap<R, u64>,
    stopped: bool,
}

impl<R, T> Default for FenceQueue<R, T> {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            completed_renderer: BTreeMap::new(),
            stopped: false,
        }
    }
}

impl<R: Ord + Clone, T> FenceQueue<R, T> {
    pub(super) fn any_pending(&self, predicate: impl Fn(&T) -> bool) -> bool {
        self.pending.iter().any(|p| predicate(&p.value))
    }
    pub(super) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Retain all descriptors during recovery, even if callbacks arrive later.
    pub(super) fn stop(&mut self) {
        self.stopped = true;
    }

    pub(super) fn push_renderer(&mut self, ring: R, fence_id: u64, value: T) {
        self.pending.push(Pending {
            ring,
            dependency: Dependency::Renderer(fence_id),
            value,
        });
    }

    pub(super) fn push_display(&mut self, ring: R, ticket: u64, value: T) {
        self.pending.push(Pending {
            ring,
            dependency: Dependency::Display(ticket),
            value,
        });
    }

    pub(super) fn push_complete(&mut self, ring: R, value: T) {
        self.pending.push(Pending {
            ring,
            dependency: Dependency::Complete,
            value,
        });
    }

    pub(super) fn complete_renderer(&mut self, ring: R, fence_id: u64) {
        self.completed_renderer
            .entry(ring)
            .and_modify(|last| *last = (*last).max(fence_id))
            .or_insert(fence_id);
    }

    pub(super) fn complete_display(&mut self, ticket: u64) {
        for pending in &mut self.pending {
            if matches!(pending.dependency, Dependency::Display(id) if id == ticket) {
                pending.dependency = Dependency::Complete;
                break;
            }
        }
    }

    /// A later completion cannot publish a higher timeline fence while an
    /// earlier descriptor on that ring is still owned by the display/renderer.
    /// Other rings remain independent. Values leave this queue exactly once.
    pub(super) fn drain_ready(&mut self) -> Vec<T> {
        self.drain_ready_if(|_| true)
    }

    pub(super) fn drain_ready_if(&mut self, mut accepts: impl FnMut(&T) -> bool) -> Vec<T> {
        if self.stopped {
            return Vec::new();
        }
        let mut blocked = BTreeSet::new();
        let mut ready = Vec::new();
        let mut index = 0;
        while index < self.pending.len() {
            let pending = &self.pending[index];
            let completed = match pending.dependency {
                Dependency::Renderer(id) => self
                    .completed_renderer
                    .get(&pending.ring)
                    .is_some_and(|last| id <= *last),
                Dependency::Display(_) => false,
                Dependency::Complete => true,
            };
            if completed && !blocked.contains(&pending.ring) && accepts(&pending.value) {
                ready.push(self.pending.remove(index).value);
            } else {
                blocked.insert(pending.ring.clone());
                index += 1;
            }
        }
        ready
    }
}

#[cfg(test)]
mod tests {
    use super::ContextFenceTokens;
    use super::FenceQueue;
    use super::GlobalFenceTokens;

    #[test]
    fn context_completion_matches_exact_tuple_and_never_reuses_tokens() {
        let mut tokens = ContextFenceTokens::default();
        let old = tokens.reserve(3, 1).unwrap();
        assert!(!tokens.complete(4, 1, old));
        assert!(!tokens.complete(3, 2, old));
        assert!(!tokens.complete(3, 1, old + 1));
        assert!(tokens.complete(3, 1, old));
        assert!(!tokens.has_context(3));
        let new = tokens.reserve(3, 1).unwrap();
        assert!(new > old);
        assert!(!tokens.complete(3, 1, old));
        assert!(tokens.has_context(3));
        assert!(tokens.complete(3, 1, new));
    }

    #[test]
    fn merged_context_callbacks_do_not_advance_another_context_or_ring() {
        let mut tokens = ContextFenceTokens::default();
        let first = tokens.reserve(3, 1).unwrap();
        let other_context = tokens.reserve(4, 1).unwrap();
        let other_ring = tokens.reserve(3, 2).unwrap();
        let merged = tokens.reserve(3, 1).unwrap();
        assert!(tokens.complete(3, 1, merged));
        assert!(!tokens.complete(3, 1, first));
        assert!(tokens.complete(4, 1, other_context));
        assert!(tokens.has_context(3));
        assert!(tokens.complete(3, 2, other_ring));
        assert!(tokens.is_empty());
    }

    #[test]
    fn context_cancel_snapshot_bound_and_overflow_keep_nonreuse() {
        let mut tokens = ContextFenceTokens::default();
        assert!(tokens.restore_last_issued(u64::from(u32::MAX)));
        let cancelled = tokens.reserve(1, 0).unwrap();
        assert!(cancelled > u64::from(u32::MAX));
        assert!(!tokens.restore_last_issued(u64::MAX));
        tokens.cancel(cancelled);
        assert!(!tokens.complete(1, 0, cancelled));
        assert!(tokens.restore_last_issued(0));
        assert!(tokens.reserve(1, 0).unwrap() > cancelled);
        for _ in 1..4096 {
            assert!(tokens.reserve(1, 0).is_some());
        }
        assert_eq!(tokens.reserve(2, 0), None);
        let last = tokens.last_issued();
        assert!(tokens.complete(1, 0, last));
        assert!(tokens.is_empty());
        assert!(tokens.restore_last_issued(u64::MAX));
        assert_eq!(tokens.reserve(1, 0), None);
    }

    #[test]
    fn opaque_global_cookies_do_not_control_renderer_progress() {
        let mut tokens = GlobalFenceTokens::default();
        let mut queue = FenceQueue::default();
        let cookies = [u64::MAX, 0, 7, 7, 2];
        for cookie in cookies {
            let token = tokens.reserve().unwrap();
            queue.push_renderer(0, u64::from(token), cookie);
        }
        assert!(!tokens.complete(u64::MAX));
        assert!(!tokens.complete(0));
        assert!(queue.drain_ready().is_empty());
        assert!(tokens.complete(2));
        queue.complete_renderer(0, 2);
        assert_eq!(queue.drain_ready(), cookies[..2]);
        assert!(tokens.complete(5));
        queue.complete_renderer(0, 5);
        assert_eq!(queue.drain_ready(), cookies[2..]);
        assert!(tokens.is_empty());
    }

    #[test]
    fn token_callback_before_response_publication_is_safe() {
        let mut tokens = GlobalFenceTokens::default();
        let mut queue = FenceQueue::default();
        let token = u64::from(tokens.reserve().unwrap());
        assert!(tokens.complete(token));
        queue.complete_renderer(0, token);
        queue.push_renderer(0, token, u64::MAX);
        assert_eq!(queue.drain_ready(), [u64::MAX]);
        assert!(!tokens.complete(token));
    }

    #[test]
    fn cancelled_token_is_never_reused_or_accepted_as_completion() {
        let mut tokens = GlobalFenceTokens::default();
        let failed = tokens.reserve().unwrap();
        tokens.cancel(failed);
        let active = tokens.reserve().unwrap();
        assert!(active > failed);
        assert!(!tokens.complete(u64::from(failed)));
        assert!(!tokens.complete(u64::from(active) + 1));
        assert!(!tokens.is_empty());
        assert!(tokens.complete(u64::from(active)));
    }

    #[test]
    fn token_restoration_never_recycles_renderer_identity() {
        let mut tokens = GlobalFenceTokens::default();
        let old = tokens.reserve().unwrap();
        assert!(!tokens.restore_last_issued(40));
        assert!(tokens.complete(u64::from(old)));
        assert!(tokens.restore_last_issued(40));
        assert!(tokens.restore_last_issued(0));
        assert_eq!(tokens.last_issued(), 40);
        let new = tokens.reserve().unwrap();
        assert_eq!(new, 41);
        assert!(!tokens.complete(u64::from(old)));
        assert!(!tokens.is_empty());
        assert!(tokens.complete(u64::from(new)));
    }

    #[test]
    fn global_tokens_cross_signed_boundary_but_never_wrap_u32() {
        let mut tokens = GlobalFenceTokens::default();
        assert!(tokens.restore_last_issued(i32::MAX as u32));
        let token = tokens.reserve().unwrap();
        assert_eq!(token, 0x8000_0000);
        // Mirrors only the documented C ABI conversions, not completion logic.
        assert!(tokens.complete(u64::from(token as i32 as u32)));
        assert!(tokens.restore_last_issued(u32::MAX - 1));
        assert_eq!(tokens.reserve(), Some(u32::MAX));
        assert_eq!(tokens.reserve(), None);
        assert!(tokens.complete(u64::from(u32::MAX)));
        assert_eq!(tokens.reserve(), None);
    }

    #[test]
    fn pending_token_bookkeeping_is_bounded_without_reusing_ids() {
        let mut tokens = GlobalFenceTokens::default();
        for _ in 0..4096 {
            tokens.reserve().unwrap();
        }
        assert_eq!(tokens.reserve(), None);
        assert!(tokens.complete(4096));
        assert_eq!(tokens.reserve(), Some(4097));
    }

    #[test]
    fn mapped_global_progress_still_waits_for_display_ownership() {
        let mut tokens = GlobalFenceTokens::default();
        let mut queue = FenceQueue::default();
        let first = u64::from(tokens.reserve().unwrap());
        queue.push_renderer(0, first, "old control");
        queue.push_display(0, 17, "display source");
        let last = u64::from(tokens.reserve().unwrap());
        queue.push_renderer(0, last, "new cursor");
        assert!(tokens.complete(last));
        queue.complete_renderer(0, last);
        assert_eq!(queue.drain_ready(), ["old control"]);
        queue.complete_display(17);
        assert_eq!(queue.drain_ready(), ["display source", "new cursor"]);
    }

    #[test]
    fn renderer_cannot_return_display_owned_buffer_or_later_timeline_fence() {
        let mut queue = FenceQueue::default();
        queue.push_display(1, 100, "display buffer");
        queue.push_renderer(1, 11, "later renderer");
        queue.complete_renderer(1, 11);
        assert!(queue.drain_ready().is_empty());
        queue.complete_display(100);
        assert_eq!(queue.drain_ready(), ["display buffer", "later renderer"]);
        assert!(queue.is_empty());
    }

    #[test]
    fn display_completion_does_not_complete_earlier_renderer_work() {
        let mut queue = FenceQueue::default();
        queue.push_renderer(1, 5, "renderer");
        queue.push_display(1, 101, "display");
        queue.complete_display(101);
        assert!(queue.drain_ready().is_empty());
        assert!(!queue.completed_renderer.contains_key(&1));
        queue.complete_renderer(1, 5);
        assert_eq!(queue.drain_ready(), ["renderer", "display"]);
    }

    #[test]
    fn out_of_order_display_completions_do_not_release_an_earlier_buffer() {
        let mut queue = FenceQueue::default();
        queue.push_display(1, 100, "first");
        queue.push_display(1, 101, "second");
        queue.push_display(1, 102, "third");
        queue.complete_display(102);
        queue.complete_display(101);
        assert!(queue.drain_ready().is_empty());
        queue.complete_display(100);
        assert_eq!(queue.drain_ready(), ["first", "second", "third"]);
    }

    #[test]
    fn independent_rings_can_retire_while_display_waits() {
        let mut queue = FenceQueue::default();
        queue.push_display(1, 100, "blocked");
        queue.push_renderer(2, 1, "other ring");
        queue.complete_renderer(2, 1);
        assert_eq!(queue.drain_ready(), ["other ring"]);
        assert!(!queue.is_empty());
    }

    #[test]
    fn renderer_callback_before_response_publication_is_remembered() {
        let mut queue = FenceQueue::default();
        queue.complete_renderer(1, 7);
        queue.push_renderer(1, 7, "encoded response");
        assert_eq!(queue.drain_ready(), ["encoded response"]);
        assert!(queue.drain_ready().is_empty());
    }

    #[test]
    fn renderer_fence_zero_requires_a_real_callback() {
        let mut queue = FenceQueue::default();
        queue.push_renderer(1, 0, "zero");
        assert!(queue.drain_ready().is_empty());
        queue.complete_renderer(1, 0);
        assert_eq!(queue.drain_ready(), ["zero"]);
    }

    #[test]
    fn older_callback_does_not_regress_renderer_progress() {
        let mut queue = FenceQueue::default();
        queue.complete_renderer(1, 9);
        queue.complete_renderer(1, 4);
        queue.push_renderer(1, 8, "already rendered");
        assert_eq!(queue.drain_ready(), ["already rendered"]);
    }

    #[test]
    fn retired_display_ticket_cannot_complete_another_descriptor() {
        let mut queue = FenceQueue::default();
        queue.push_display(1, 100, "old");
        queue.complete_display(100);
        assert_eq!(queue.drain_ready(), ["old"]);
        queue.push_display(1, 101, "new");
        queue.complete_display(100);
        assert!(queue.drain_ready().is_empty());
        queue.complete_display(101);
        assert_eq!(queue.drain_ready(), ["new"]);
    }

    #[test]
    fn recovery_does_not_release_buffers_on_late_callbacks() {
        let mut queue = FenceQueue::default();
        queue.push_display(1, 100, "display");
        queue.push_renderer(1, 9, "renderer");
        queue.stop();
        queue.complete_display(100);
        queue.complete_renderer(1, 9);
        assert!(queue.drain_ready().is_empty());
        assert!(!queue.is_empty());
    }

    #[test]
    fn fenced_error_cannot_bypass_display_ownership() {
        let mut queue = FenceQueue::default();
        queue.push_display(1, 100, Ok("display"));
        queue.push_complete(1, Err("create_fence failed"));
        assert!(queue.drain_ready().is_empty());
        queue.complete_display(100);
        assert_eq!(
            queue.drain_ready(),
            [Ok("display"), Err("create_fence failed")]
        );
    }

    #[test]
    fn failed_native_retirement_retains_error_descriptor_and_owned_backing() {
        use std::sync::Arc;
        let backing = Arc::new([0u8; 32]);
        let observer = Arc::downgrade(&backing);
        let mut queue = FenceQueue::default();
        queue.push_display(0, 17, backing.clone());
        queue.stop();
        // Even an otherwise immediately completed error descriptor must stay
        // owned after retirement fails. Late callbacks cannot authorize reuse.
        queue.push_complete(0, backing.clone());
        queue.push_renderer(1, 9, backing.clone());
        drop(backing);
        queue.complete_display(17);
        queue.complete_renderer(1, 9);
        assert!(queue.drain_ready().is_empty());
        assert!(queue.drain_ready_if(|_| true).is_empty());
        assert_eq!(observer.strong_count(), 3);
        drop(queue);
        assert!(observer.upgrade().is_none());
    }

    #[test]
    fn cursor_batch_cannot_consume_control_responses() {
        let mut queue = FenceQueue::default();
        queue.push_renderer(1, 1, ("control", 7));
        queue.push_renderer(2, 1, ("cursor", 8));
        queue.complete_renderer(1, 1);
        queue.complete_renderer(2, 1);
        assert_eq!(queue.drain_ready_if(|v| v.0 == "cursor"), [("cursor", 8)]);
        assert_eq!(queue.drain_ready(), [("control", 7)]);
    }
}
