import { withAlpha } from '@/lib/color'

/**
 * 界面色的唯一出处。
 *
 * 这套值**由 JS 写成 `<html>` 上的 CSS 变量**，而不是写在 .css 文件里，理由有两个：
 * 一是 AntDV 的 theme token 只吃真实颜色值（cssinjs 要从它派生 hover/active 色），
 * 给它 `var(--bg-surface)` 它不会解析，只会产出无效声明；于是如果色值写在 .css 里，
 * TS 侧就得再抄一份，两份迟早对不上。二是主色是用户选的任意值，`--accent*` 本来
 * 就得现算，索性整套都在同一处生成。
 *
 * 半透明一律写成 `rgba(r, g, b, a)` 的传统逗号格式，不写 `rgb(255 255 255 / 10%)`：
 * 这些值除了进 CSS，还要进 AntDV 的 token，而它内部用 TinyColor 解析，传统格式才稳。
 *
 * 变量名是给组件的 `<style scoped>` 用的（`var(--line)`），字段名是给 TS 侧用的，
 * 只有这一张表，别再往别处加十六进制。
 */
export interface Surfaces {
  readonly bgApp: string
  readonly bgSurface: string
  readonly bgRaised: string
  readonly bgHover: string
  readonly bgActive: string
  readonly bgSelected: string
  readonly line: string
  readonly lineStrong: string
  readonly text1: string
  readonly text2: string
  readonly text3: string
  readonly ok: string
  readonly warn: string
  readonly err: string
  readonly paneBg: string
  readonly shadow: string
}

export const DARK: Surfaces = {
  bgApp: '#0b0f14',
  bgSurface: '#10151b',
  bgRaised: '#171e26',
  bgHover: 'rgba(255, 255, 255, 0.06)',
  bgActive: 'rgba(255, 255, 255, 0.1)',
  bgSelected: 'rgba(255, 255, 255, 0.13)',
  line: 'rgba(255, 255, 255, 0.1)',
  lineStrong: 'rgba(255, 255, 255, 0.18)',
  text1: '#dfe5ec',
  text2: '#a9b4c0',
  text3: '#78858f',
  ok: '#4ec976',
  warn: '#e0a836',
  err: '#e05c52',
  paneBg: '#12181f',
  shadow: '0 8px 24px rgba(0, 0, 0, 0.45)',
}

export const LIGHT: Surfaces = {
  bgApp: '#eef1f5',
  bgSurface: '#f8fafc',
  bgRaised: '#ffffff',
  bgHover: 'rgba(15, 30, 45, 0.05)',
  bgActive: 'rgba(15, 30, 45, 0.09)',
  bgSelected: 'rgba(15, 30, 45, 0.12)',
  line: 'rgba(15, 30, 45, 0.12)',
  lineStrong: 'rgba(15, 30, 45, 0.2)',
  text1: '#1f2933',
  text2: '#4a5866',
  text3: '#7c8894',
  ok: '#3f9c46',
  warn: '#a67c0d',
  err: '#c43c31',
  paneBg: '#ffffff',
  shadow: '0 8px 24px rgba(15, 30, 45, 0.14)',
}

/** CSS 变量名按 kebab 写，字段名按 camel 读，两边是同一张表。 */
const VAR_NAMES = {
  bgApp: '--bg-app',
  bgSurface: '--bg-surface',
  bgRaised: '--bg-raised',
  bgHover: '--bg-hover',
  bgActive: '--bg-active',
  bgSelected: '--bg-selected',
  line: '--line',
  lineStrong: '--line-strong',
  text1: '--text-1',
  text2: '--text-2',
  text3: '--text-3',
  ok: '--ok',
  warn: '--warn',
  err: '--err',
  paneBg: '--pane-bg',
  shadow: '--shadow',
} as const satisfies Record<keyof Surfaces, string>

/**
 * 把主题写进 `<html>`：界面侧的唯一出口。
 *
 * - `data-theme` 只是给调试和外部样式看的标记，色值走内联变量，所以不存在选择器权重之争。
 * - `color-scheme` 一句话，原生滚动条与表单控件就不再是白底。
 * - `--accent*` 由主色现算：界面里那些「主色的淡底」（选中行、焦点描边、录制灯环）
 *   不能跟着写死一个蓝，否则主色改成橙色之后立刻露馅。
 *
 * 每个窗口各写各的：文件窗口是独立 webview、独立 Pinia，但读的是同一份偏好，
 * `storage` 事件一到就把这个窗口的 `<html>` 也刷一遍。
 */
export function applyThemeVars(dark: boolean, primary: string): void {
  const surfaces = dark ? DARK : LIGHT
  const root = document.documentElement
  root.dataset.theme = dark ? 'dark' : 'light'
  root.style.setProperty('color-scheme', dark ? 'dark' : 'light')
  for (const [key, value] of Object.entries(surfaces)) {
    root.style.setProperty(VAR_NAMES[key as keyof Surfaces], value)
  }
  root.style.setProperty('--accent', primary)
  root.style.setProperty('--accent-soft', withAlpha(primary, dark ? 0.16 : 0.1))
  root.style.setProperty('--accent-line', withAlpha(primary, 0.45))
  root.style.setProperty('--accent-glow', withAlpha(primary, 0.3))
}
