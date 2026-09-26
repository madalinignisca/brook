# Inline image previews (#66, remainder): spec and plan

> A preview decodes bytes a stranger chose, so it's a security surface. The rules below come
> from the server review: an allowlist, sniffed never named; a sandboxed decoder; caps before
> decoding; the local encrypted cache only; no animation.

## 1. What the user gets

- An image attachment (PNG, JPEG, GIF, WebP) shows a thumbnail under its row: at most
  360 × 240, aspect kept. Clicking it opens the image (core's `open_file`).
- **Small images (≤ 4 MiB) preview by themselves** when the row is shown: fetched into the
  encrypted cache if they aren't there yet.
- **Larger ones** (up to 16 MiB) get a "Show preview" button: nothing is downloaded until the
  user asks.
- **Anything else**, including images over 16 MiB, huge dimensions, or a decoder that isn't
  available, keeps the plain file row. There's never a fallback decoder.
- GIF and animated WebP show their first frame only.

## 2. Core: `preview_file(id, file_id) -> ImagePreview`

`ImagePreview { kind: ImageKind (Png | Jpeg | Gif | Webp), width: u32, height: u32,
bytes: Vec<u8> }`

1. The file is looked up in the cached messages, like every file-cache call (`file.unknown`).
   `FileInfo.size > PREVIEW_MAX_BYTES` (16 MiB) is refused with `file.preview_refused`, before
   anything is fetched.
2. `cache_file(id, file_id)`: the bytes come only from the encrypted cache, fetched through
   core with progress and cancel under `id`. A URL is never handed to anyone.
3. The file is decrypted into memory (the snapshot reader), never onto disk.
4. **The kind is sniffed** from the bytes (`\x89PNG`, `\xff\xd8\xff`, `GIF87a`/`GIF89a`,
   `RIFF….WEBP`). The name and the declared type play no part. Anything else is
   `file.preview_refused`.
5. **The dimensions are read from the header** before anything decodes. They're refused
   (`file.preview_refused`) when missing, when either side is over 8192 px, or when the image
   is over 40 megapixels:
   - PNG: IHDR;
   - GIF: the logical screen;
   - JPEG: the first SOFn;
   - WebP: VP8, VP8L or VP8X.
6. The bytes and the dimensions are returned.

The dimension parsers are small, bounded and pure (they read at most the first 64 KiB), and
are tested with a table, including headers that lie.

## 3. GTK: decoding in a sandbox

- **glycin** (the GNOME sandboxed image loader, crate 3.1) decodes from the bytes:
  - under bubblewrap on a normal install, and `flatpak-spawn --sandbox` in a Flatpak;
  - `accepted_memory_formats(R8g8b8a8)`, then `load()`;
  - `details()` is checked against the same caps again (the decoder's own reading of the
    size);
  - then `next_frame()`, **one frame**, and never `specific_frame` or a loop, so nothing
    animates.

  The frame becomes a `gdk::MemoryTexture` built by us, so glycin's gtk-rs version doesn't
  matter, shown in a `gtk::Picture` capped at 360 × 240.
- **The sandbox is chosen, never inferred.** The loader gets an explicit
  `sandbox_selector`: `FlatpakSpawn` inside a Flatpak (`/.flatpak-info`), `Bwrap` everywhere
  else. It's never `Auto`, which picks `NotSandboxed` in a Flatpak development environment, and
  never `NotSandboxed`. So no decoder ever runs unsandboxed, not even to read a header. After
  `load()`, `active_sandbox_mechanism()` is asserted to be `Bwrap` or `FlatpakSpawn`, or the
  image is dropped.
- If glycin can't run (no loaders installed, bwrap missing or refused), there's no preview:
  the row stays as it is. It's logged once at `info`.
- **A bounded queue:** at most 2 previews decode at a time, newest-requested first (the rows
  just scrolled into view). A request whose row has gone is dropped before it starts.
- **A timeout:** each decode (load plus the one frame) gets 10 s, then its `gio::Cancellable`
  is cancelled and there's no preview.
- **Metered networks:** automatic previews (4 MiB or less) only fetch when
  `gio::NetworkMonitor` says the connection isn't metered. A file already in the cache still
  previews. "Show preview" always works.
- Decoding runs on the Tokio runtime, off the GTK loop. A row that scrolls away drops its
  task.
- **Install note:** glycin's loaders are the distro's `glycin` package (`glycin-loaders` on
  Debian and Ubuntu). INSTALL.md says previews need it. The Flatpak (#51) gets them from the
  GNOME runtime.

## 4. Work, in order

1. **Core PR:** `preview_file`, the sniff, the dimension parsers, and the caps.
   Tests:
   - a table of kinds and dimensions, including lying headers, zero sizes, oversize
     dimensions, a truncated header, and a `.png` name over JPEG bytes (it's a JPEG);
   - `preview_refused` for over 16 MiB, checked before any request;
   - nothing plaintext written to disk (the store holds only the blob);
   - `file.unknown`.
2. **GTK PR:**
   - the glycin decode;
   - the thumbnail in the attachment row (auto at 4 MiB or less, "Show preview" otherwise);
   - click to Open;
   - no preview when glycin fails;
   - a unit test that the selector is `Bwrap` outside a Flatpak and `FlatpakSpawn` inside,
     and never `Auto` or `NotSandboxed`, plus the mechanism check that drops anything else;
   - the queue (2 in flight, newest first, gone rows dropped) and the timeout, tested without
     GTK;
   - an ignored live test that decodes a real PNG through glycin, run by hand on this
     machine.
3. **Then drag and drop** onto the message box: it's the composer's existing staged-files
   path, from a `gtk::DropTarget` for `gdk::FileList`.
