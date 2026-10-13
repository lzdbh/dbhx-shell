<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from 'vue'

import '@xterm/xterm/css/xterm.css'

import type { SessionId } from '@/lib/ipc/bindings'
import { applyTheme, createTerminal, destroyTerminal, fitTerminal, lookup } from '@/lib/terminal/registry'
import { useSessionsStore } from '@/stores/sessions'
import { useSettingsStore } from '@/stores/settings'
import { useThemeStore } from '@/stores/theme'

const props = defineProps<{ session: SessionId, active: boolean }>()

const host = ref<HTMLDivElement | null>(null)
const sessions = useSessionsStore()
const settings = useSettingsStore()
const themeStore = useThemeStore()

/** 观察者与 xterm 事件句柄都不进响应式：它们持有 DOM 与终端实例本身。 */
let observer: ResizeObserver | null = null
let stopData: { dispose: () => void } | null = null
let stopResize: { dispose: () => void } | null = null

onMounted(() => {
  const element = host.value
  if (!element) return
  const { term, fit } = createTerminal(props.session, themeStore.terminalTheme)
  term.open(element)
  term.options.fontSize = settings.fontSize

  stopData = term.onData(text => void sessions.send(props.session, text))
  stopResize = term.onResize(({ rows, cols }) => void sessions.reportResize(props.session, { rows, cols }))

  // open() 之后立刻 fit 会量到 0 高度，等这一帧布局结束
  requestAnimationFrame(() => {
    fit.fit()
    if (props.active) term.focus()
  })

  observer = new ResizeObserver(() => {
    fitTerminal(props.session)
  })
  observer.observe(element)
})

onBeforeUnmount(() => {
  stopData?.dispose()
  stopResize?.dispose()
  observer?.disconnect()
  observer = null
  // 标签页关闭时后端已经关掉了会话，这里只负责把实例从登记处摘掉
  if (lookup(props.session)) destroyTerminal(props.session)
})

watch(() => props.active, (active) => {
  const handle = lookup(props.session)
  if (!handle || !active) return
  fitTerminal(props.session)
  handle.term.focus()
})

watch(() => themeStore.terminalTheme, next => applyTheme(props.session, next))
watch(() => settings.fontSize, size => {
  const handle = lookup(props.session)
  if (handle) handle.term.options.fontSize = size
})
</script>

<template>
  <!-- 外层容器必须有确定高度，FitAddon 是按容器像素尺寸反算行列数的 -->
  <div ref="host" class="terminal-host" />
</template>

<style scoped>
.terminal-host {
  height: 100%;
  min-height: 120px;
  overflow: hidden;
}
</style>
