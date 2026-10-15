<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from 'vue'

/**
 * 自绘窗控按钮（最小化/最大化还原/关闭），供去边框窗口的工具栏充当标题栏。
 *
 * 三个动作都要显式 ACL 权限（`core:window:allow-*`），`core:window:default` 里没有——
 * 这点在 `capabilities/default.json` 有注释，缺了会在点击那一刻才报错。
 *
 * 最大化状态只能靠 `onResized` 回读：双击标题栏、Win+方向键、系统右键菜单都能改变它，
 * 而这些路径不经过本组件，唯一同步源是窗口自己发的事件。
 *
 * 纯浏览器（vite 不带 tauri）里 `getCurrentWindow` 会抛，那时整组按钮不渲染：
 * 画出来只会是三个点了没反应的死键。关闭走 `close()` 而不是 `destroy()`，保留
 * closeRequested 语义，与原生关闭按钮完全同一条路径。
 */

type TauriWindow = import('@tauri-apps/api/window').Window

const ready = ref(false)
const maximized = ref(false)
let win: TauriWindow | null = null
let stopResize: (() => void) | null = null

onMounted(async () => {
  try {
    const { getCurrentWindow } = await import('@tauri-apps/api/window')
    win = getCurrentWindow()
    maximized.value = await win.isMaximized()
    stopResize = await win.onResized(async () => {
      maximized.value = (await win?.isMaximized()) ?? false
    })
    ready.value = true
  }
  catch {
    win = null
  }
})

onBeforeUnmount(() => {
  stopResize?.()
  stopResize = null
})

function minimize(): void {
  void win?.minimize()
}

function toggleMaximize(): void {
  void win?.toggleMaximize()
}

function close(): void {
  void win?.close()
}
</script>

<template>
  <!-- 整组按钮对拖拽脚本显式关闭：容器自身不是可点击标签，不写这个的话按钮间隙
       一按就开始拖窗口（见 drag.js 的 data-tauri-drag-region 语义） -->
  <div v-if="ready" class="win-controls" data-tauri-drag-region="false">
    <button class="ctl" title="最小化" @click="minimize">
      <svg viewBox="0 0 10 10" width="10" height="10" aria-hidden="true">
        <path d="M0.5 5.5h9" />
      </svg>
    </button>
    <button class="ctl" :title="maximized ? '向下还原' : '最大化'" @click="toggleMaximize">
      <svg v-if="!maximized" viewBox="0 0 10 10" width="10" height="10" aria-hidden="true">
        <rect x="0.5" y="0.5" width="9" height="9" />
      </svg>
      <svg v-else viewBox="0 0 10 10" width="10" height="10" aria-hidden="true">
        <path d="M0.5 2.5h6v6h-6z" />
        <path d="M3.5 2.5V0.5h6v6h-3" />
      </svg>
    </button>
    <button class="ctl close" title="关闭" @click="close">
      <svg viewBox="0 0 10 10" width="10" height="10" aria-hidden="true">
        <path d="M0.5 0.5l9 9M9.5 0.5l-9 9" />
      </svg>
    </button>
  </div>
</template>

<style scoped>
.win-controls {
  display: flex;
  align-self: stretch;
  flex: none;
}

/* 46px 宽、通栏高：Windows 原生 caption 按钮的度量，鼠标扫到角落的容错面积靠它。
   `cursor: default` 是系统惯例——标题栏按钮不提示可点，用箭头。 */
.ctl {
  width: 46px;
  display: grid;
  place-items: center;
  border: 0;
  padding: 0;
  background: transparent;
  color: var(--text-2);
  cursor: default;
  transition: background-color var(--tap) ease, color var(--tap) ease;
}

.ctl svg {
  stroke: currentColor;
  stroke-width: 1;
  fill: none;
  /* 1px 细线落在非整数像素上会糊，crispEdges 保住 Windows 那种锐利描边 */
  shape-rendering: crispEdges;
}

.ctl:hover {
  background: var(--bg-hover);
  color: var(--text-1);
}

.ctl:active {
  background: var(--bg-active);
}

/* 关闭键的悬停红是系统约定（#c42b1c），不跟主题色走：红底白叉在深浅主题下
   都是「这是个危险动作」的最快信号，跟主色混会失掉这层意思 */
.ctl.close:hover {
  background: #c42b1c;
  color: #ffffff;
}

.ctl.close:active {
  background: #b02617;
}
</style>
