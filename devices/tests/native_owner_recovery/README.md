# Production native allocation recovery tests

Run `cargo test --manifest-path devices/tests/native_owner_recovery/Cargo.toml --locked`.

This independent workspace compiles the whole production `native_shared.rs`,
the actual shared allocation wire definitions/encoder, actual native allocation
description validation, and both production ownership/mapping ledgers. It does
not mirror the query/allocation/cleanup algorithm. Deterministic fixtures exist
only at the AHB allocator, renderer and OS mapper boundaries; no fixture result
is claimed as Android AHB, KGSL or Gunyah runtime proof.

Coverage includes lost ALLOCATE/cleanup responses, retained failed native
release, failed import/metadata/allocator paths, failed and uncertain external
mapping, duplicate terminal cleanup, unknown-request sealing against delayed
allocation, stale Surface identity, host incarnation replacement, exact wire
fields, terminal journal capacity and legacy allocation collision. Personal CI
runs these production contracts on Linux x64 and native ARM64.

Optional recovery command numbers and layouts are in `include/dvsa_protocol.h`.
Neither these commands nor passing these tests admit rendering or presentation.
Legacy context destruction still has no idempotent terminal reply contract.
