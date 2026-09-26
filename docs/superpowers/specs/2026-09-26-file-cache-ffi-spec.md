# The file cache through the Apple bindings (#67)

Status: spec and plan. Review dial: **Standard** (the bindings mirror core's API from
#144; no new storage, auth or wire behaviour).

## Why

#144 gave core an encrypted file cache: `cache_file`, `open_file`, `save_cached_file`,
`file_state` and `clear_open_copies`, plus a `CacheEvent::Files` event. GTK uses them in
#146. The Mac needs them bound before its Open and offline Save UI, which waits on local
data and so on the provisioning profile (#79). The bindings don't wait on anything.

## Done means

1. `FfiBrookClient` gains, each a thin call into core that **throws** `LoginError` (so
   `file.open_refused`, `local.unavailable`, `file.unknown` and `file.gone` reach the UI):
   - `cache_file(transfer_id, file_id)`: progress and cancel under `transfer_id`, as
     `download_file`;
   - `open_file(transfer_id, file_id) throws -> String`: the path of the private copy to hand to
     `NSWorkspace`. `file.open_refused` means Save only;
   - `save_cached_file(file_id, destination) throws -> Bool`: false means it isn't cached, so the
     caller downloads it. The destination is truncated and removed on failure, so the Mac
     passes a temporary path and then calls `replaceItemAt`, as its Save does now;
   - `file_state(file_id) throws -> FfiFileCacheState` (`NotCached`, `Partial { done, size }`,
     `Cached`);
   - `clear_open_copies()`. The Mac app calls it on quit (as GTK's shutdown hook). A
     crash leaves copies until the next open, where core's reconcile clears them.
2. `FfiCacheEvent.Files { ids }` already maps (#144). Nothing more to do there.
3. Tests:
   - brook-ffi: `FfiFileCacheState` maps each variant with its fields (mutation-checked);
   - Swift, no server: with local data off, `open_file` and `file_state` answer
     `local.unavailable`, and `save_cached_file` answers the same;
   - the live suite (`itest.sh`): with local data on over a temp `MemorySlot`, a file is
     cached, reads `Cached`, opens into a copy whose bytes match, a `.html` file is
     refused with `file.open_refused`, and `clear_open_copies` removes the copy.

## Not doing

- The Mac UI for Open, "Available offline", and a Save that works offline: after #79.
- Pinning (#67's "keep available offline"): core has the column, but no API yet.
- A cache size setting: core uses `FILE_CACHE_CAP` for now.

## Plan

- `bindings/apple/src/offline.rs`: the record, the five methods, and tests in
  `offline_tests.rs`.
- `OfflineAPITests.swift`: the no-server checks. `ChatIntegrationTests.swift` or a new
  `FileCacheIntegrationTests` holds the live one, added to `itest.sh`'s SUITES.
- **Where it fails:** `open_file` returns a `PathBuf`; a non-UTF-8 path can't become a
  `String`. Core builds it from a hex directory and `safe_leaf` of a server filename,
  which is a Rust `String`, so it's always UTF-8. Convert with `to_string_lossy` anyway,
  and note why it can't be lossy.
- **If it stops halfway:** each method is independent. A partial PR is still coherent.

## Review round 1

The reviewer read the return types as dropping errors; every method throws `LoginError`,
now written into each signature. Taken: the Mac clears copies on quit. A crash's leftovers
were already covered by reconcile at the next open, and are now stated.
