import { computed, ref, watch } from 'vue'

import { defineStore } from 'pinia'

import { applyThemeVars, DARK as DARK_SURFACE, LIGHT as LIGHT_SURFACE } from '../styles/palette'

/** 终端调色板：与 xterm.js 的具体形状解耦，ansi 按 0..15 顺序。 */
export interface TerminalTheme {
  id: string
  background: string
  foreground: string
  cursor: string
  cursorAccent: string
  selectionBackground: string
  ansi: [string, string, string, string, string, string, string, string, string, string, string, string, string, string, string, string]
}

/**
 * 两套调色板与 `--pane-bg` 同色，所以底/前景直接从 `styles/palette.ts` 取：
 * 终端格子是一块独立的表面，xterm 自己画底，界面画边框，两边不一致就会看见一圈色差缝。
 */
const DARK: TerminalTheme = {
  id: 'dbhx-dark',
  background: DARK_SURFACE.paneBg,
  foreground: DARK_SURFACE.text1,
  cursor: '#7cc4ff',
  cursorAccent: DARK_SURFACE.paneBg,
  selectionBackground: '#243240',
  ansi: [
    '#1b2128', '#d05a53', '#6fbf73', '#d9b457', '#5b9dd9', '#b07ad0', '#56b6c2', '#c8cfd6',
    '#5b6773', '#ff6b61', '#77d77b', '#ffd97a', '#7cc4ff', '#d79bff', '#7adfee', '#f2f5f8',
  ],
}

const LIGHT: TerminalTheme = {
  id: 'dbhx-light',
  background: LIGHT_SURFACE.paneBg,
  foreground: LIGHT_SURFACE.text1,
  cursor: '#1677ff',
  cursorAccent: LIGHT_SURFACE.paneBg,
  selectionBackground: '#d6e6fb',
  ansi: [
    '#22303c', '#c43c31', '#3f9c46', '#a67c0d', '#1677ff', '#8f4bb0', '#1f8c9c', '#6b7a89',
    '#5b6773', '#e05247', '#54b45c', '#c99a17', '#3d8fe8', '#a866c9', '#3aa7b8', '#33414f',
  ],
}

/**
 * 主题存在哪：`localStorage`，不是 vault。
 *
 * 它是「这台机器上看着顺不顺眼」的偏好，与凭据无关；更关键的是**多个窗口共用同一份**——
 * 文件管理窗口是独立 webview、独立 Pinia，不共享的话它永远从默认深色起步，
 * 主窗口切成浅色它也不动。
 */
const KEY = 'dbhx:theme'

interface PersistedTheme {
  dark: boolean
  colorPrimary: string
}

/** 没存过（或存坏了）时的起步值：深色系 + 默认蓝，和 Xshell 用户的直觉一致。 */
const DEFAULTS: PersistedTheme = { dark: true, colorPrimary: '#1677ff' }

/** 读磁盘上的偏好。形状不对就当没存过，绝不拿半个坏值去渲染。 */
function readTheme(): PersistedTheme | null {
  try {
    const raw = localStorage.getItem(KEY)
    if (!raw) return null
    const saved = JSON.parse(raw) as Partial<PersistedTheme>
    if (typeof saved.dark !== 'boolean' || typeof saved.colorPrimary !== 'string') return null
    return { dark: saved.dark, colorPrimary: saved.colorPrimary }
  } catch {
    return null
  }
}

/**
 * 挂载前先把变量写上。
 *
 * 这一步不能等 store 建好：`main.ts` 里两个窗口根组件的 `<style>` 一挂上去就要读
 * `var(--bg-app)`，晚一步第一帧就是 WebView2 的白底，深色主题下看得见闪一下。
 */
export function applyStoredTheme(): void {
  const stored = readTheme() ?? DEFAULTS
  applyThemeVars(stored.dark, stored.colorPrimary)
}

/**
 * 两层主题的单一来源：
 * - `terminalTheme` 喂 xterm.js 的 ITheme（M1 在 TerminalRuntime 里做 ansi→具名字段映射）
 * - `dark` / `colorPrimary` 由 `styles/antd.ts` 组合成 ConfigProvider 的 theme
 *   （AntDV 走 cssinjs token，不是 CSS 变量，所以 store 不掺组件库的具体形状）
 *
 * 两者必须由同一份状态派生，切主题时才不会半深半浅。
 */
export const useThemeStore = defineStore('theme', () => {
  const stored = readTheme()
  const dark = ref(stored?.dark ?? DEFAULTS.dark)
  const colorPrimary = ref(stored?.colorPrimary ?? DEFAULTS.colorPrimary)

  const terminalTheme = computed<TerminalTheme>(() => (dark.value ? DARK : LIGHT))

  function toggleDark() {
    dark.value = !dark.value
  }

  watch([dark, colorPrimary], ([nextDark, nextPrimary]) => {
    applyThemeVars(nextDark, nextPrimary)
    localStorage.setItem(KEY, JSON.stringify({ dark: nextDark, colorPrimary: nextPrimary } satisfies PersistedTheme))
  })

  // `storage` 事件只在**别的**文档里触发，所以本窗口写自己那份不会绕回来抖一遍；
  // 主窗口一切换，已经开着的文件窗口立刻跟着变。
  window.addEventListener('storage', (event) => {
    if (event.key !== KEY) return
    const next = readTheme()
    if (!next) return
    dark.value = next.dark
    colorPrimary.value = next.colorPrimary
  })

  return { dark, colorPrimary, terminalTheme, toggleDark }
})
