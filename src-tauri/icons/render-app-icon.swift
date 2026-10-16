// dbhx shell 应用图标的唯一来源：CoreGraphics 矢量绘制，无外部依赖、无网络。
//
//   swift src-tauri/icons/render-app-icon.swift src-tauri/icons/app-icon.png
//   pnpm exec tauri icon src-tauri/icons/app-icon.png
//
// 画在 1024 逻辑坐标上（原点在左下）。所有线宽都按「缩到 32px 还剩 ≥1.2px」反推过，
// 改细任何一条都要回看 32/64 的小样，别只看大图。
import CoreGraphics
import Foundation
import ImageIO

let outPath = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "src-tauri/icons/app-icon.png"
let S = 1024.0

let inkTop = rgb(30, 41, 53)
let inkBot = rgb(13, 18, 24)
let green = rgb(63, 179, 122)
let greenHi = rgb(110, 226, 160)
let chalk = rgb(232, 238, 243)
let dim = rgb(126, 146, 164)
let faint = rgb(255, 255, 255, 0.20)

func rgb(_ r: Int, _ g: Int, _ b: Int, _ a: CGFloat = 1) -> CGColor {
    CGColor(srgbRed: CGFloat(r) / 255, green: CGFloat(g) / 255, blue: CGFloat(b) / 255, alpha: a)
}
func rounded(_ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat, _ r: CGFloat) -> CGPath {
    CGPath(roundedRect: CGRect(x: x, y: y, width: w, height: h), cornerWidth: r, cornerHeight: r, transform: nil)
}
func stroke(_ c: CGContext, _ pts: [CGPoint], _ w: CGFloat, _ color: CGColor) {
    c.beginPath()
    c.move(to: pts[0])
    for p in pts.dropFirst() { c.addLine(to: p) }
    c.setLineCap(.round)
    c.setLineJoin(.round)
    c.setStrokeColor(color)
    c.setLineWidth(w)
    c.strokePath()
}
func dot(_ c: CGContext, _ p: CGPoint, _ r: CGFloat, _ color: CGColor) {
    c.setFillColor(color)
    c.fillEllipse(in: CGRect(x: p.x - r, y: p.y - r, width: r * 2, height: r * 2))
}
func fillPath(_ c: CGContext, _ path: CGPath, _ color: CGColor) {
    c.setFillColor(color)
    c.addPath(path)
    c.fillPath()
}

// 底板：四周留透明边距的圆角方块 + 深色渐变 + 左上柔光 + 一层描边高光
func plate(_ c: CGContext, inset: CGFloat = 56) {
    let side = S - inset * 2
    let path = rounded(inset, inset, side, side, side * 0.225)
    c.saveGState()
    c.addPath(path)
    c.clip()
    let base = CGGradient(colorsSpace: CGColorSpaceCreateDeviceRGB(), colors: [inkTop, inkBot] as CFArray, locations: [0, 1])!
    c.drawLinearGradient(base, start: CGPoint(x: 0, y: S), end: CGPoint(x: 0, y: 0), options: [])
    let glow = CGGradient(colorsSpace: CGColorSpaceCreateDeviceRGB(),
                          colors: [rgb(255, 255, 255, 0.11), rgb(255, 255, 255, 0)] as CFArray, locations: [0, 1])!
    c.drawRadialGradient(glow, startCenter: CGPoint(x: S * 0.3, y: S * 0.8), startRadius: 0,
                         endCenter: CGPoint(x: S * 0.3, y: S * 0.8), endRadius: S * 0.6, options: [])
    c.restoreGState()
    c.addPath(path)
    c.setStrokeColor(rgb(255, 255, 255, 0.10))
    c.setLineWidth(3)
    c.strokePath()
}

let c = CGContext(data: nil, width: Int(S), height: Int(S), bitsPerComponent: 8, bytesPerRow: 0,
                  space: CGColorSpaceCreateDeviceRGB(),
                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
plate(c)

// 主机树：一条主干挂三个节点，中间那个是当前会话，它的分支直接拐进提示符
let trunkX: CGFloat = 232
stroke(c, [CGPoint(x: trunkX, y: 748), CGPoint(x: trunkX, y: 296)], 44, faint)
for y in [748.0, 296.0] {
    stroke(c, [CGPoint(x: trunkX, y: y), CGPoint(x: 392, y: y)], 44, faint)
    dot(c, CGPoint(x: trunkX, y: y), 40, dim)
}
stroke(c, [CGPoint(x: trunkX, y: 522), CGPoint(x: 360, y: 522)], 48, green)
dot(c, CGPoint(x: trunkX, y: 522), 50, green)

// 会话本身：`>` 与光标块
stroke(c, [CGPoint(x: 452, y: 670), CGPoint(x: 574, y: 522), CGPoint(x: 452, y: 374)], 88, greenHi)
fillPath(c, rounded(690, 456, 210, 68, 19), chalk)

let img = c.makeImage()!
let dest = CGImageDestinationCreateWithURL(URL(fileURLWithPath: outPath) as CFURL, "public.png" as CFString, 1, nil)!
CGImageDestinationAddImage(dest, img, nil)
guard CGImageDestinationFinalize(dest) else {
    FileHandle.standardError.write("写入失败: \(outPath)\n".data(using: .utf8)!)
    exit(1)
}
print("wrote \(outPath)")
