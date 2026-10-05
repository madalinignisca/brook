# Mac: "Show image previews" setting (#259)

Review dial: Standard (one external reviewer plus the Claude review). A local, per-device UI
preference; no server, auth, storage-format or FFI change. Not Light: it changes when image bytes
are fetched and decoded (a privacy behaviour the owner asked for) and sits next to the decoder queue.

## Decision (owner, on the ticket)
Default **OFF**: an absent or unset value means off. A fresh install and every existing install stop
auto-previewing small images until the setting is turned on. Release notes of the first beta with it must say so.
The default is one constant (`Settings.showImagePreviewsDefault = false`) used by `Settings.showImagePreviews`
and every `@AppStorage` initial value.

## Where a file is drawn on the Mac
Only `AttachmentRow` (timeline, online and offline cached; the kept-offline toggle is part of the same row)
draws a preview (`ChatViews.swift` AttachmentRow, `FileRowModel.swift`). Search results, quotes, unsent
bubbles, staged files and notifications show text only. The only code that fetches or decodes a preview is
`FileRowModel` (`previewFile`, `ImageDecoder.shared.thumbnail`), so the setting is enforced there, not per view.

## Done means
1. Settings shows "Show image previews" under "Show usernames", caption: "Small images in conversations show
   by themselves. When off, an image loads only when you click Show preview." Key `ShowImagePreviews`, per device.
2. Pure, `nonisolated static` decisions in `FileRowModel.swift`:
   `canPreview(type:size:hasLocalData:decoderOff:)` (declared png/jpeg/gif/webp up to `previewMaxBytes()`, local
   data, decoder not off) and `shouldAutoPreview(setting:size:type:hasLocalData:decoderOff:expensive:here:)`
   (`setting && canPreview && size <= autoMaxBytes && (!expensive || here)`). Table tests over every input.
3. Off: opening a conversation fetches and decodes nothing; each image row a preview can work for shows the
   plain row plus "Show preview"; no `previewFile`, no `fileState` for the preview, no decode. Clicking shows that one.
4. The button is not offered for a non-image or unsupported type, a file over 16 MiB, no local data, or a decoder
   that is off (setting off or on): those rows stay `.none`.
5. A row model reads the stored setting when created, so a surface that never pushes it still starts correct
   (injected `UserDefaults` in tests, never the owner's real defaults: the test host is Brook.app).
6. Turning off with previews on screen puts every `.shown`/`.loading` row back to `.offer` at once; an image
   whose decode finishes after the switch is dropped (a `previewEpoch` generation checked after each await).
7. Turning on re-decides every `.offer` row as on opening (small show; large or expensive-not-here keep the button);
   rows already `.shown` stay and are not fetched again.
8. The setting never pins, unpins, opens or deletes anything; Open, Save, Keep available offline work with it off.
9. `ImageDecoder.isOff` is readable from any thread (a `Flag` set where `disabled` is set), so the row can refuse
   to fetch for a dead decoder (today a click on a dead decoder still downloads the image).
10. `clients/macos/build.sh test` passes; each new test is watched failing once (mutation).

Untested widget wiring, named in the PR: the `SettingsView` toggle; `AttachmentRow`'s `@AppStorage`,
`.onChange` and the `.task` call to `previewSetting`. Seen by hand on the owner's Mac or stated as unseen.

## Not doing
GTK (another agent); moving the rule into core (a few booleans whose inputs are each platform's own);
previews in search/quotes/pending/composer (none exist); per-message or per-channel settings; clearing decoded
images or cached bytes when turned off; promising reaction to `defaults write` while running.

## Where it fails
- Tests reading the owner's real setting: every test row takes a fresh defaults suite.
- A late image after turning off: epoch check after each await in `showPreview`.
- A row off screen during the toggle (LazyVStack): `.task` runs again on reappearing and calls
  `previewSetting(current)` first (no-op if unchanged); a discarded model is rebuilt from defaults.
- Decoder switches off after a button was offered: the click checks `decoderOff` first, row goes `.none`, nothing fetched.
- Offline and not cached, "Show preview" clicked: `previewFile` fails, row goes `.none` as today.
- Two windows share the key through `@AppStorage`.
