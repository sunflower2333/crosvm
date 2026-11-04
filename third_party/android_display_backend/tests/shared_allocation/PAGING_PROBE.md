# Explicit Native AHB paging probe

The production allocator provides synchronous byte READ, WRITE and FILL on its
authenticated DMA-BUF alias. These operations are for explicit residency paging;
ordinary Present never invokes them. The caller must first establish exclusive
paging ownership after producer GPU retirement and all Android consumer releases.

Every operation is nonempty and at most 65536 bytes. The range is bounded by the
authoritative allocation size, including row/end padding. READ and WRITE permit
byte offsets; FILL requires four-byte offset/length alignment and writes a little
endian `u32` pattern. DMA-BUF sync START/END surround the operation. The API returns
zero or negative errno. An error can occur after WRITE/FILL has changed bytes;
failure must not admit another writer or publish a completed paging fence.

The private C holder and Rust slice pointers never cross the virtio wire. Rust
`HostSharedAllocation` exposes `read_bytes`, `write_bytes` and `fill_bytes`, all
returning `anyhow::Result<()>`. APEX/stub variants reject these operations.

`explicit_paging_read_bytes`, `explicit_paging_write_bytes` and
`explicit_paging_fill_bytes` count bytes at actual CPU operations. They include
copies whose subsequent sync END fails. The counters are process-scoped and are
reported separately from image-copy/blit, saved-buffer, and encoder operations
in `NATIVE-AHB-SURFACE` logs.

Build the real allocator and fixture from the workspace-local AOSP toolchain:

```sh
TMPDIR=/home/sunf/droidvm-repos/.artifacts node \
  libs/android_display_backend/tests/shared_allocation/build-probe.mjs \
  /home/sunf/droidvm-repos/.artifacts/native-shared-ahb-paging-probe-20260922-v1 paging
```

The probe runs `rgba` and `bgra` separately, with 64x64 and 513x257 allocations.
It writes and reads all allocation bytes in bounded full/final chunks, verifies
unaligned readback, checks visible bytes through the native AHB CPU lock, fills
and rereads all padding, rejects invalid ranges/alignment/null arguments, and
checks exact operation counters. No VM, guest paging, GPU submission or Present
is exercised by this probe.

Device evidence from 2026-09-22 is frozen under
`.artifacts/native-shared-ahb-paging-probe-20260922-v1/`:

| Format | Read bytes | Write bytes | Fill bytes | Result |
| --- | ---: | ---: | ---: | --- |
| RGBA | 1359878 | 679936 | 679936 | PASS |
| BGRA | 1220614 | 610304 | 610304 | PASS |

Both local and on-device executable SHA256:
`a5bd4d372b10932b0296ae41eaac55142eea1e8e85f8f6c560210edd174a982f`.
The per-byte checks are intentionally reported as assertions, not independent
test scenarios or proof of Windows residency behavior.
