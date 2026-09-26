import CoreGraphics
import Darwin
import Foundation
import ImageIO
import UniformTypeIdentifiers

// One image, one process (spec 2026-09-26-mac-previews-spec.md §1): spawned by the broker,
// sandboxed by inheritance with no entitlements of its own. It reads `kind` (1 byte) and the
// image from stdin, writes one frame to stdout and exits. Nothing it decodes outlives it.

func fail(_ code: ReplyCode) -> Never {
    FileHandle.standardOutput.write(Frame(code: UInt32(code.rawValue), width: 0, height: 0, body: Data()).encoded())
    exit(0)
}

// 1. Limits first, hard equal to soft so they can't be raised again. No fork or spawn: an
//    exploit can't leave a process behind. CPU is a second bound; the broker's kill is the
//    real one.
func limit(_ resource: Int32, _ value: rlim_t) {
    var l = rlimit(rlim_cur: value, rlim_max: value)
    if setrlimit(resource, &l) != 0 { _exit(1) }
}
limit(RLIMIT_NPROC, 0)
limit(RLIMIT_CPU, 10)

// 2. Only the four kinds may decode, in this whole process.
let allowed = [UTType.png, .jpeg, .gif, .webP].map(\.identifier) as CFArray
if CGImageSourceSetAllowableTypes(allowed) != noErr { _exit(1) }

// 3. The request: `kind`, then the bytes to EOF, never more than core's cap (the broker, the
//    only writer, has already refused anything bigger; this is the second check).
let input = FileHandle.standardInput.readDataToEndOfFile()
guard input.count >= 1, input.count - 1 <= PreviewCaps.maxInputBytes,
      let kind = ImageKindCode(rawValue: input[input.startIndex])
else { _exit(1) }
let bytes = input.dropFirst()

#if DEBUG
if kind == .hang {
    while true {} // the broker's deadline must end this
}
if kind == .linger {
    let one = Frame(code: 0, width: 1, height: 1, body: Data(count: 4)).encoded()
    FileHandle.standardOutput.write(one)
    close(1)
    while true { sleep(60) }
}
if kind == .probe {
    FileHandle.standardOutput.write(Frame(code: UInt32(ReplyCode.report.rawValue), width: 0, height: 0,
                                          body: Data(probeReport().utf8)).encoded())
    exit(0)
}
#endif

// 4. The decode: ImageIO must agree with core's kind, and read sizes within core's caps.
let uti: UTType
switch kind {
case .png: uti = .png
case .jpeg: uti = .jpeg
case .gif: uti = .gif
case .webp: uti = .webP
#if DEBUG
case .hang, .probe, .linger: _exit(1)
#endif
}
let hint = [kCGImageSourceTypeIdentifierHint: uti.identifier] as CFDictionary
guard let source = CGImageSourceCreateWithData(Data(bytes) as CFData, hint) else { fail(.noImage) }
guard let type = CGImageSourceGetType(source) as String?, type == uti.identifier else { fail(.refused) }
guard let props = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
      let w = props[kCGImagePropertyPixelWidth] as? Int, let h = props[kCGImagePropertyPixelHeight] as? Int
else { fail(.noImage) }
guard w > 0, h > 0, w <= PreviewCaps.imageMaxSide, h <= PreviewCaps.imageMaxSide,
      w * h <= PreviewCaps.imageMaxPixels
else { fail(.overCaps) }

// Frame 0 only, at reduced size; `Always` so a sender's EXIF thumbnail never stands in.
let options = [
    kCGImageSourceThumbnailMaxPixelSize: PreviewCaps.maxSide,
    kCGImageSourceCreateThumbnailFromImageAlways: true,
    kCGImageSourceCreateThumbnailWithTransform: true,
    kCGImageSourceShouldCacheImmediately: true,
] as CFDictionary
guard let thumb = CGImageSourceCreateThumbnailAtIndex(source, 0, options) else { fail(.noImage) }
let (tw, th) = (thumb.width, thumb.height)
guard tw > 0, th > 0, tw <= PreviewCaps.maxSide, th <= PreviewCaps.maxSide else { fail(.overCaps) }

// Drawn into the one format the app accepts: sRGB, 8 bits, RGBA premultiplied.
var rgba = Data(count: tw * th * 4)
let drawn = rgba.withUnsafeMutableBytes { buf -> Bool in
    guard let ctx = CGContext(data: buf.baseAddress, width: tw, height: th, bitsPerComponent: 8,
                              bytesPerRow: tw * 4, space: CGColorSpace(name: CGColorSpace.sRGB)!,
                              bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
                                  | CGBitmapInfo.byteOrder32Big.rawValue)
    else { return false }
    ctx.draw(thumb, in: CGRect(x: 0, y: 0, width: tw, height: th))
    return true
}
guard drawn else { fail(.drawFailed) }
FileHandle.standardOutput.write(Frame(code: 0, width: UInt32(tw), height: UInt32(th), body: rgba).encoded())
exit(0)
