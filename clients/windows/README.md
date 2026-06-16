# Windows client — C# / .NET + WinUI 3

Native Windows app over the shared Rust [`core`](../../core).

## Stack
- **WinUI 3 (Windows App SDK)**, C#/.NET.
- `core` via **C ABI** (e.g. `csbindgen` / P-Invoke over a `cdylib`).
- Media: platform WebRTC + **Media Foundation / NVENC / AMF / QSV** (HW H.264), or GStreamer via `MediaEngine`. See [../../docs/MEDIA.md](../../docs/MEDIA.md).

## Native UX commitments (Fluent design)
- Mica/Acrylic materials, native title bar, NavigationView sidebar.
- System light/dark/accent followed natively; native file picker; toast notifications; taskbar integration.

## Packaging
**MSIX**.
