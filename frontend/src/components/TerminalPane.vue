<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from 'vue'

import type { SessionId } from '@/lib/ipc/bindings'
import { attachTerminal, detachTerminal, ensureTerminal, focusTerminal } from '@/lib/terminal/registry'
import { useSessionsStore } from '@/stores/sessions'
import { useSettingsStore } from '@/stores/settings'
import { useThemeStore } from '@/stores/theme'

/**
 * 一个终端格子。
 *
 * 它**自己不持有终端**，只提供一个 `div` 插槽，并告诉登记处「这个会话现在归我这里」。
 * 终端实例与它的 DOM 都在 `lib/terminal/registry.ts`，于是：
 * - 分屏 / 收屏 / 换钉住的会话都只是换插槽，滚动历史不会凭空消失；
 * - 同一会话在两个格子之间移动时不会出现两个副本。
 *
 * `session` 为 null 表示这个格子暂时没有归属（没分屏时的右格就是 null）。
 */
const props = defineProps<{ session: SessionId | null }>()

const sessions = useSessionsStore()
const settings = useSettingsStore()
const themeStore = useThemeStore()

const slot = ref<HTMLElement | null>(null)

/** 这个格子当前认领的会话。认领关系变了才动 DOM，避免每次响应式更新都 appendChild。 */
let attached: SessionId | null = null

function claim(id: SessionId): void {
  const host = slot.value
  if (host === null) return
  if (attached !== null && attached !== id) detachTerminal(attached, host)
  attached = id
  ensureTerminal(id, themeStore.terminalTheme, settings.fontSize, {
    onData: data => { void sessions.send(id, data) },
    onResize: (rows, cols) => { void sessions.reportResize(id, { rows, cols }) },
  })
  attachTerminal(id, host)
}

function release(): void {
  const host = slot.value
  if (host !== null && attached !== null) detachTerminal(attached, host)
  attached = null
}

watch(() => props.session, id => {
  if (id === null) {
    release()
    return
  }
  claim(id)
  // 格子换了主人就把焦点跟过去，否则敲键盘打在看不见的终端上
  focusTerminal(id)
})

onMounted(() => {
  if (props.session !== null) claim(props.session)
})

onBeforeUnmount(release)
</script>

<template>
  <div ref="slot" class="pane" />
</template>

<style scoped>
.pane {
  /* 格子的尺寸完全由父层的 flex 决定，这里只保证终端撑满 */
  position: relative;
  flex: 1;
  min-width: 0;
  min-height: 0;
  overflow: hidden;
}

/* 宿主元素是登记处 createElement 造的，带不上 scoped 的 data-v 属性，只能 :deep 进去 */
.pane :deep(.xterm-host) {
  position: absolute;
  inset: 0;
  padding: 4px 0 0 6px;
}

.pane :deep(.xterm) {
  height: 100%;
}
</style>
