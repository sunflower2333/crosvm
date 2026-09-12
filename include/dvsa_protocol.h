/* Copyright 2026 The ChromiumOS Authors. BSD-style license, see LICENSE. */
#ifndef CROSVM_DVSA_PROTOCOL_H
#define CROSVM_DVSA_PROTOCOL_H
#include <stdint.h>
#include <stddef.h>

/* All fields little endian. Wire v1, never native handles or fd numbers. */
#define DVSA_MAGIC UINT32_C(0x41535644)
#define DVSA_VERSION UINT32_C(1)
#define DVSA_CMD_DISCOVER UINT32_C(0xd110)
#define DVSA_CMD_ALLOCATE UINT32_C(0xd111)
#define DVSA_CMD_DESTROY UINT32_C(0xd114)
#define DVSA_CMD_ACK_MAPPING UINT32_C(0xd115)
#define DVSA_CMD_QUERY_OWNER UINT32_C(0xd116)
#define DVSA_CMD_ALLOCATE_RECOVERABLE UINT32_C(0xd117)
#define DVSA_CMD_CLEANUP_OWNER UINT32_C(0xd118)
#define DVSA_RESP_OWNER UINT32_C(0xd216)
#define DVSA_OWNER_SESSION UINT32_C(1)
#define DVSA_OWNER_RETAINED UINT32_C(2)
#define DVSA_OWNER_RELEASED UINT32_C(3)
#define DVSA_OWNER_UNKNOWN UINT32_C(4)
#define DVSA_RESP_DISCOVER UINT32_C(0xd210)
#define DVSA_RESP_ALLOCATE UINT32_C(0xd211)
#define DVSA_FEATURE_SDR_ZERO_COPY (UINT64_C(1) << 0)
/* Reserved primitive admission only; never a render/present/write lease. */
#define DVSA_FEATURE_ALLOCATION_MAPPING (UINT64_C(1) << 1)
#define DVSA_FORMAT_ABGR8888 UINT32_C(0x34324241)
#define DVSA_LAYOUT_VERIFIED_LINEAR UINT32_C(1)
#define DVSA_UNAVAILABLE_MAPPER (UINT64_C(1) << 0)
#define DVSA_UNAVAILABLE_NO_SUFFIX (UINT64_C(1) << 1)
#define DVSA_UNAVAILABLE_TRIPLE_CAPACITY (UINT64_C(1) << 2)
#define DVSA_UNAVAILABLE_NATIVE_SURFACE (UINT64_C(1) << 3)
#define DVSA_UNAVAILABLE_ALLOCATOR (UINT64_C(1) << 4)
#define DVSA_UNAVAILABLE_RENDERER_IMPORT (UINT64_C(1) << 5)
#define DVSA_UNAVAILABLE_PRODUCER_BRIDGE (UINT64_C(1) << 6)
#define DVSA_UNAVAILABLE_CONSUMER_BRIDGE (UINT64_C(1) << 7)
#define DVSA_UNAVAILABLE_GUEST_IDENTITY (UINT64_C(1) << 8)

#pragma pack(push, 8)
struct dvsa_ctrl_header {
    uint32_t type, flags;
    uint64_t fence_id;
    uint32_t context_id;
    uint8_t ring_idx, padding[3];
};
struct dvsa_header {
    struct dvsa_ctrl_header hdr;
    uint32_t magic, version, size, scanout_id;
    uint64_t surface_generation, token;
    uint32_t resource_id, reserved;
};
struct dvsa_discovery {
    struct dvsa_header query;
    uint64_t feature_bits, unavailable_reasons;
    uint64_t bar_size, reserved_prefix, dynamic_capacity, mapping_alignment;
    uint32_t min_allocations, max_allocations;
    uint64_t triple_bytes_lower_bound;
};
struct dvsa_allocate {
    struct dvsa_header query;
    uint32_t width, height, fourcc, flags;
};
struct dvsa_allocation_description {
    uint64_t buffer_id, allocation_size, modifier, plane_offset;
    uint32_t plane_stride, width, height, fourcc, plane_count, layout_flags;
    uint64_t reserved;
};
struct dvsa_allocated {
    struct dvsa_header query;
    struct dvsa_allocation_description description;
};
struct dvsa_ack_mapping {
    struct dvsa_header query;
    uint64_t bar_offset, mapped_size;
};
struct dvsa_owner_request {
    struct dvsa_header query;
    uint64_t host_epoch[2];
};
struct dvsa_allocate_recoverable {
    struct dvsa_allocate allocation;
    uint64_t host_epoch[2];
};
struct dvsa_owner_response {
    struct dvsa_header query;
    uint64_t host_epoch[2];
    uint32_t state, owner_context;
    uint64_t allocation_size, bar_offset, mapped_size, reserved[2];
};
#pragma pack(pop)

/* DISCOVER: exactly64bytes, scanout/context/ring/padding/reserved0, identity0;
 * only FENCEbit0 allowed; fence_id0 unless FENCE. Response128bytes.
 * ALLOCATE: exactly80bytes; flags0; ABGR8888; current generation; token0;
 * unused resource_id and existing native DRM context_id both nonzero.
 * ACK_MAPPING: exactly80bytes; exact returned identity and actual mapped range.
 * DESTROY: exactly64bytes; exact retained identity, old generation allowed
 * for cleanup only. Requires successful UNMAP if mapping was attempted.
 * All mutations: hdr flags/fence/ring/padding0, scanout0; no trailing bytes.
 * Ordinary MAP_BLOB/UNMAP_BLOB identify a retained host owner by resource_id.
 * UNMAP is terminal for a token: DESTROY then ALLOCATE before another MAP.
 * New ALLOCATE requires primitive admission. Known-owner cleanup remains
 * available after Surface/capacity changes. A dedicated native diagnostic
 * context is required: holding an allocation blocks every ordinary SUBMIT in
 * that context until checked destruction. No CPU map/ACK grants a GPU lease.
 * ACQUIRE0xd112/PRESENT0xd113 are unfrozen proposals and remain unsupported.
 *
 * Optional recovery commands do not change any capability/lease bit.
 * QUERY_OWNER80: zero ownership fields request SESSION; epoch[2]0 discovers a
 * fresh nonzero host incarnation. Nonzero expected epoch must match exactly.
 * Owner QUERY/CLEANUP80: expected epoch, saved context/resource/Surface identity.
 * Token0 reconciles a lost ALLOCATE reply; nonzero token must match the record.
 * ALLOCATE_RECOVERABLE96: original allocation fields plus expected host epoch;
 * only this command establishes a recoverable allocation journal.
 * OWNER response128: ordinary unfenced envelope has context0; owner_context is
 * explicit. UNKNOWN never proves release. CLEANUP of an unknown request seals
 * its resource identity before RELEASED, rejecting a delayed allocation.
 * RELEASED is a terminal receipt after actual unmap/native release, or a
 * recorded no-allocation seal. Duplicates return the same identity. Journal
 * tombstones cannot be evicted/reused in the same epoch. Different host epochs
 * never authorize releasing an old guest record. Old Surface cleanup is valid.
 * Recovery does not make legacy context destruction idempotent.
 */
#if defined(__cplusplus)
#define DVSA_STATIC_ASSERT(c) static_assert(c, #c)
#else
#define DVSA_STATIC_ASSERT(c) _Static_assert(c, #c)
#endif
DVSA_STATIC_ASSERT(sizeof(struct dvsa_ctrl_header) == 24);
DVSA_STATIC_ASSERT(sizeof(struct dvsa_header) == 64);
DVSA_STATIC_ASSERT(sizeof(struct dvsa_discovery) == 128);
DVSA_STATIC_ASSERT(sizeof(struct dvsa_allocate) == 80);
DVSA_STATIC_ASSERT(sizeof(struct dvsa_allocation_description) == 64);
DVSA_STATIC_ASSERT(sizeof(struct dvsa_allocated) == 128);
DVSA_STATIC_ASSERT(sizeof(struct dvsa_ack_mapping) == 80);
DVSA_STATIC_ASSERT(sizeof(struct dvsa_owner_request) == 80);
DVSA_STATIC_ASSERT(sizeof(struct dvsa_allocate_recoverable) == 96);
DVSA_STATIC_ASSERT(sizeof(struct dvsa_owner_response) == 128);
DVSA_STATIC_ASSERT(offsetof(struct dvsa_owner_response, host_epoch) == 64);
DVSA_STATIC_ASSERT(offsetof(struct dvsa_owner_response, owner_context) == 84);
DVSA_STATIC_ASSERT(offsetof(struct dvsa_header, surface_generation) == 40);
DVSA_STATIC_ASSERT(offsetof(struct dvsa_discovery, feature_bits) == 64);
DVSA_STATIC_ASSERT(offsetof(struct dvsa_discovery, triple_bytes_lower_bound) == 120);
#undef DVSA_STATIC_ASSERT
#endif
