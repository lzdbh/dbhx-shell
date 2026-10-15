/**
 * 颜色换算。界面里凡是「主色的某个透明度」都由这里现算，而不是再手写一个十六进制：
 * 主色是用户在设置页随便选的，硬编码的淡蓝会在主色改成橙色后露馅。
 */

/**
 * `#rgb` / `#rrggbb` 加透明度 ⇒ `rgba(...)`。
 *
 * 不是合法 hex 时回中性灰——CSS 变量一旦被写成非法值，浏览器会**整条声明丢掉**，
 * 于是那一处不是变灰而是根本没底色，比灰更难查，所以宁可回退也不放行。
 */
export function withAlpha(hex: string, alpha: number): string {
  const rgb = parseHex(hex)
  const a = Math.min(1, Math.max(0, alpha))
  if (rgb === null) return `rgba(128, 128, 128, ${a})`
  return `rgba(${rgb[0]}, ${rgb[1]}, ${rgb[2]}, ${a})`
}

function parseHex(input: string): [number, number, number] | null {
  const text = input.trim().toLowerCase()
  const short = /^#([0-9a-f])([0-9a-f])([0-9a-f])$/.exec(text)
  if (short) return [Number(`0x${short[1]}${short[1]}`), Number(`0x${short[2]}${short[2]}`), Number(`0x${short[3]}${short[3]}`)]
  const long = /^#([0-9a-f]{6})$/.exec(text)
  if (!long) return null
  const value = Number(`0x${long[1]}`)
  return [(value >> 16) & 0xff, (value >> 8) & 0xff, value & 0xff]
}
