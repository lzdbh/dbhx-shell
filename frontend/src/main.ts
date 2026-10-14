import { createApp } from 'vue'
import { createPinia } from 'pinia'
import 'ant-design-vue/dist/reset.css'
import App from './App.vue'
import FileManager from './components/FileManager.vue'

/**
 * 一个前端产物、两种窗口：主窗口是终端 shell，`files-<hex>` 窗口是文件管理。
 *
 * 靠窗口 label 分流而不是 URL 路由——Tauri 的 `WebviewUrl::App` 带 fragment 并不可靠，
 * 而 label 天然是「谁创建了我」的凭据，且文件窗口需要的唯一上下文（为哪台主机开）
 * 就编在 label 后缀里。纯浏览器（vite 不带 tauri）里 `getCurrentWindow` 会抛，
 * 那种环境本来也开不出文件窗口，回落到主界面即可。
 */
async function fileWindowHostId(): Promise<string | null> {
  try {
    const { getCurrentWindow } = await import('@tauri-apps/api/window')
    const label = getCurrentWindow().label
    return label.startsWith('files-') ? label.slice('files-'.length) : null
  }
  catch {
    return null
  }
}

const hostId = await fileWindowHostId()

if (hostId !== null) {
  createApp(FileManager, { hostId }).use(createPinia()).mount('#app')
}
else {
  createApp(App).use(createPinia()).mount('#app')
}
