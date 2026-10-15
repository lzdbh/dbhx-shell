import type { ThemeConfig } from 'ant-design-vue/es/config-provider/context'

import { theme } from 'ant-design-vue'

import { DARK, LIGHT } from './palette'

/**
 * ConfigProvider 用的那份 theme，由 `styles/palette.ts` 的同一张表派生。
 *
 * 为什么不能只靠 CSS 变量：AntDV 走 cssinjs，token 值会被 TinyColor 拿去派生
 * hover/active/边框的一整套色阶，它不解析 `var(--x)`。所以色值必须同时存在于
 * TS 与 CSS 两侧——那就只能有一个出处，这里是 TS，组件的 `<style scoped>` 读
 * `applyThemeVars()` 写上去的变量。
 */
export function antdTheme(dark: boolean, colorPrimary: string): ThemeConfig {
  const s = dark ? DARK : LIGHT
  return {
    algorithm: dark ? theme.darkAlgorithm : theme.defaultAlgorithm,
    token: {
      colorPrimary,
      // 三层表面：窗口底 → 条带/输入底 → 浮层。和终端格子的 --pane-bg 是同一系，
      // 抽屉与弹窗浮起来时不会跳出一个不同色系的白块。
      colorBgLayout: s.bgApp,
      colorBgContainer: s.bgSurface,
      colorBgElevated: s.bgRaised,
      colorText: s.text1,
      colorTextSecondary: s.text2,
      colorTextTertiary: s.text3,
      colorBorder: s.lineStrong,
      colorBorderSecondary: s.line,
      colorSuccess: s.ok,
      colorWarning: s.warn,
      colorError: s.err,
      borderRadius: 6,
      borderRadiusLG: 10,
      // 界面里的小字（状态条、树副标题）统一走这一档，不再各处手写 11px
      fontSizeSM: 12,
      controlHeightSM: 26,
    },
    components: {
      Layout: {
        // 4.2 的 Layout 只开放这三个色，条带高度/内边距不在 token 里，由 App.vue 的样式管
        colorBgHeader: s.bgSurface,
        colorBgBody: s.bgApp,
        colorBgTrigger: s.bgRaised,
      },
    },
  }
}
