# Plan: Mac "Show image previews" (#259)

Each step builds and passes `clients/macos/build.sh test` on its own.

1. **The setting** (`Brook/Settings.swift`): `showImagePreviewsKey = "ShowImagePreviews"`,
   `static let showImagePreviewsDefault = false` (owner: off), `var showImagePreviews: Bool`.
   Tests: `SettingsTests` default is the one constant and off; a stored choice is read.
2. **Decoder flag** (`Brook/Previews/ImageDecoder.swift`): `nonisolated let offFlag = Flag()`, set in
   `disableOnce`; `nonisolated var isOff: Bool`. Test: `ImageDecoderQueueTests.testAnUnreachableDecoderReadsAsOffFromAnyThread`.
3. **Pure decisions** (`FileRowModel.swift`): `canPreview`, `shouldAutoPreview`; `previewable` keeps today's
   rule (existing `testOnlyDeclaredImagesUpTo16MiBArePreviewable` unchanged). The test helper `model(...)` passes a fresh defaults suite.
4. **The row follows the setting**: `FileRowModel.init(defaults:decoderOff:)`; `startPreview` with setting off sets
   `.offer` if `canPreview` else `.none`, with no `fileState`/fetch/decode; `showPreview` guards `canPreview` (decoder
   included) and checks `previewEpoch` after both awaits; `func previewSetting(_ on: Bool) async` (same value: nothing;
   off: bump epoch, `.shown`/`.loading` to `.offer`; on: `.offer` to `.none` then `startPreview()` if on screen).
   Tests: auto-preview false whenever setting off; on keeps today's rule (4 MiB, expensive, already-here); off fetches/decodes
   nothing until Show preview; no button where a preview cannot work; a row starts from the stored setting without being told;
   turning off returns shown rows at once; an image arriving after turning off is dropped; turning on shows rows as on opening;
   the setting never touches keep/open.
5. **Wiring and UI**: `AttachmentRow` `@AppStorage(Settings.showImagePreviewsKey)`, `.task` calls `previewSetting(showPreviews)`
   before `startPreview()`, `.onChange` calls it from a Task; `SettingsView` toggle and caption.
   By hand on the owner's Mac: small and large images on; off shows buttons only and no decode in the `previews` log;
   off with previews showing turns them into buttons; back on shows small ones; Open/Save/Keep work off; relaunch keeps it.

If it stops halfway: after 1 to 3 nothing visible changes; after 4 without a toggle the key is absent so previews are off by
default (the owner's decision) and a `defaults write` turns them on at the next channel open; after 5 without the hand check the
screen behaviour is unproven and the PR says so. Nothing deletes or migrates data; rollback is a revert.
