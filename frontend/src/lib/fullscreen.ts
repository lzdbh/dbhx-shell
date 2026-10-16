import { onBeforeUnmount, onMounted, ref, type Ref } from 'vue'

/**
 * 窗口当前是否处于 macOS 原生全屏。
 *
 * 用途只有一个：全屏时系统把红绿灯收进「指针移到顶部才滑出来」的浮层，工具栏不该再
 * 为它们留那 78px。
 *
 * Tauri 没有全屏事件（JS 侧只有 `tauri://resize`），所以沿用 `WindowControls.vue`
 * 同步最大化态那个办法：resize 之后回读 `isFullscreen()`。原生全屏是一段动画，
 * 期间会连续收到 resize，最后一次读到的就是落定后的状态。
 *
 * `isFullscreen` 属于 `core:window:default` 里的 getter，不需要额外 ACL。
 * 纯浏览器（vite 不带 tauri）里 `getCurrentWindow` 会抛，那时恒为 false。
 */
export function useMacFullscreen(): Ref<boolean> {
  const fullscreen = ref(false)
  let stop: (() => void) | null = null

  onMounted(async () => {
    try {
      const { getCurrentWindow } = await import('@tauri-apps/api/window')
      const win = getCurrentWindow()
      fullscreen.value = await win.isFullscreen()
      stop = await win.onResized(async () => {
        fullscreen.value = await win.isFullscreen()
      })
    }
    catch {
      fullscreen.value = false
    }
  })

  onBeforeUnmount(() => {
    stop?.()
    stop = null
  })

  return fullscreen
}
