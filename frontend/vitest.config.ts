import { fileURLToPath, URL } from 'node:url'

import { defineConfig } from 'vitest/config'

/**
 * 单独一份 vitest 配置，而不是往 vite.config.ts 里塞 `test` 字段。
 *
 * vite 8 的插件类型挂在 rolldown 上，vitest 自带的那份 vite 类型还挂在 rollup 上，
 * 用 `vitest/config` 的 defineConfig 复用主配置会让 vue 插件在 `vue-tsc` 下报不兼容。
 * 测试只跑 `.ts`（store 与纯函数），不需要 vue 插件，所以这里只留别名和环境。
 */
export default defineConfig({
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  test: {
    environment: 'happy-dom',
    include: ['src/**/*.spec.ts'],
  },
})
