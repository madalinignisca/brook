import CoreGraphics
import Foundation

/// The decoder's reply is untrusted (spec §2, the equivalent of GTK's `frame_fits`): checked
/// in order, before any arithmetic can overflow, and turned into a `CGImage` only in a pixel
/// format the app chose itself, over a copy of the bytes.
enum PreviewValidator {
    /// `header` is the size core read from the image's header (`FfiImagePreview`).
    static func image(code: Int, width: Int, height: Int, rgba: Data,
                      header: (width: Int, height: Int)) -> CGImage? {
        guard code == ReplyCode.ok.rawValue else { return nil }
        let side = PreviewCaps.maxSide
        guard width > 0, height > 0, width <= side, height <= side else { return nil }
        // A thumbnail only shrinks; EXIF rotation may swap the sides, so compare them sorted.
        let (long, short) = (max(width, height), min(width, height))
        let (hLong, hShort) = (max(header.width, header.height), min(header.width, header.height))
        guard long <= hLong, short <= hShort else { return nil }
        let (row, o1) = width.multipliedReportingOverflow(by: 4)
        let (all, o2) = row.multipliedReportingOverflow(by: height)
        guard !o1, !o2, rgba.count == all else { return nil }
        guard let provider = CGDataProvider(data: Data(rgba) as CFData), // a copy the image owns
              let space = CGColorSpace(name: CGColorSpace.sRGB)
        else { return nil }
        return CGImage(width: width, height: height, bitsPerComponent: 8, bitsPerPixel: 32,
                       bytesPerRow: row, space: space,
                       bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue
                           | CGBitmapInfo.byteOrder32Big.rawValue),
                       provider: provider, decode: nil, shouldInterpolate: true, intent: .defaultIntent)
    }
}
