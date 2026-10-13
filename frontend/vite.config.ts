import process from 'node:process'

import { fileURLToPath, URL } from 'node:url'

import vue from '@vitejs/plugin-vue'
import Components from 'unplugin-vue-components/vite'
import { AntDesignVueResolver } from 'unplugin-vue-components/resolvers'
import { defineConfig } from 'vite'

// Tauri 期望固定端口，strictPort 让端口被占时直接失败而不是悄悄换端口
const hostPort = Number(process.env.VITE_PORT ?? 1420)

export default defineConfig({
  plugins: [
    vue(),
    Components({
      resolvers: [
        // AntDV 4 用 cssinjs 运行时注入样式，无需再 import 样式文件
        AntDesignVueResolver({ importStyle: false }),
      ],
      dts: 'src/components.d.ts',
    }),
  ],
  clearScreen: false,
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  server: {
    port: hostPort,
    strictPort: true,
    watch: {
      // src-tauri 的 Rust 改动会让 vite 无意义重启
      ignored: ['**/src-tauri/**'],
    },
  },
  envPrefix: ['VITE_', 'TAURI_ENV_'],
  build: {
    target: 'es2022',
    sourcemap: process.env.TAURI_ENV_DEBUG === 'true',
  },
})
