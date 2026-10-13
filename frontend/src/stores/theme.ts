import { computed, ref } from 'vue'

import { defineStore } from 'pinia'

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

const DARK: TerminalTheme = {
  id: 'dbhx-dark',
  background: '#101418',
  foreground: '#d7dde4',
  cursor: '#7cc4ff',
  cursorAccent: '#101418',
  selectionBackground: '#2c3947',
  ansi: [
    '#1b2128', '#d05a53', '#6fbf73', '#d9b457', '#5b9dd9', '#b07ad0', '#56b6c2', '#c8cfd6',
    '#5b6773', '#ff6b61', '#77d77b', '#ffd97a', '#7cc4ff', '#d79bff', '#7adfee', '#f2f5f8',
  ],
}

const LIGHT: TerminalTheme = {
  id: 'dbhx-light',
  background: '#fbfcfd',
  foreground: '#22303c',
  cursor: '#1677ff',
  cursorAccent: '#fbfcfd',
  selectionBackground: '#d6e6fb',
  ansi: [
    '#22303c', '#c43c31', '#3f9c46', '#a67c0d', '#1677ff', '#8f4bb0', '#1f8c9c', '#6b7a89',
    '#5b6773', '#e05247', '#54b45c', '#c99a17', '#3d8fe8', '#a866c9', '#3aa7b8', '#33414f',
  ],
}

/**
 * 两层主题的单一来源：
 * - `terminalTheme` 喂 xterm.js 的 ITheme（M1 在 TerminalRuntime 里做 ansi→具名字段映射）
 * - `dark` / `colorPrimary` 由消费侧组合成 ConfigProvider 的 theme
 *   （AntDV 走 cssinjs token，不是 CSS 变量，所以 store 不掺组件库的具体形状）
 *
 * 两者必须由同一份状态派生，切主题时才不会半深半浅。
 */
export const useThemeStore = defineStore('theme', () => {
  const dark = ref(true)
  const colorPrimary = ref('#1677ff')

  const terminalTheme = computed<TerminalTheme>(() => (dark.value ? DARK : LIGHT))

  function toggleDark() {
    dark.value = !dark.value
  }

  return { dark, colorPrimary, terminalTheme, toggleDark }
})
