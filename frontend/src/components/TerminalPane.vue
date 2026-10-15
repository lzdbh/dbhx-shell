<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from 'vue'

import { message } from 'ant-design-vue'
import { CheckSquareOutlined, ClearOutlined, CopyOutlined, SearchOutlined, SnippetsOutlined, VideoCameraOutlined, ZoomInOutlined, ZoomOutOutlined } from '@ant-design/icons-vue'

import type { SessionId } from '@/lib/ipc/bindings'
import { attachTerminal, clearView, copySelection, detachTerminal, ensureTerminal, focusTerminal, pasteClipboard, selectAllText } from '@/lib/terminal/registry'
import { closeMenu, openMenu } from '@/lib/contextMenu'
import type { MenuPoint } from '@/lib/contextMenu'
import QuickCommandBar from '@/components/QuickCommandBar.vue'
import TerminalStatus from '@/components/TerminalStatus.vue'
import { useQuickStore } from '@/stores/quick'
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

const emit = defineEmits<{
  /** 把查找条叫出来。开不开归 App 管，它才知道焦点在哪一格。 */
  (e: 'find'): void
}>()

const sessions = useSessionsStore()
const settings = useSettingsStore()
const themeStore = useThemeStore()
const quick = useQuickStore()
const [messageApi, holder] = message.useMessage()

const slot = ref<HTMLElement | null>(null)

/** 这个格子当前认领的会话。认领关系变了才动 DOM，避免无谓的 appendChild。 */
let attached: SessionId | null = null

/**
 * 右键菜单。
 *
 * 项是按「这一格此刻能干什么」现搭的，不是常驻结构：没有选区就没有复制，
 * 正在记录就把第二项写成「停止」，省掉一颗点了没反应的灰按钮。
 * 字号那三条走 settings（全局偏好），和 `Ctrl+滚轮` 改的是同一个值。
 */
function openMenuHere(at: MenuPoint, hasSelection: boolean): void {
  const id = props.session
  if (id === null) return
  const logging = sessions.tab(id)?.logPath != null
  openMenu(at, [
    { key: 'copy', label: '复制', hint: 'Ctrl+C', icon: CopyOutlined, disabled: !hasSelection, onClick: () => copySelection(id) },
    { key: 'paste', label: '粘贴', hint: 'Ctrl+V', icon: SnippetsOutlined, onClick: () => pasteClipboard(id) },
    { key: 'all', label: '全选', icon: CheckSquareOutlined, onClick: () => selectAllText(id) },
    { key: 'sep1', separator: true },
    { key: 'find', label: '查找', hint: 'Ctrl+F', icon: SearchOutlined, onClick: () => emit('find') },
    { key: 'clear', label: '清屏', hint: '只滚走这一页', icon: ClearOutlined, onClick: () => clearView(id) },
    { key: 'sep2', separator: true },
    {
      key: 'log',
      label: logging ? '停止记录日志' : '记录日志到文件',
      icon: VideoCameraOutlined,
      onClick: () => { void toggleLog(id) },
    },
    { key: 'sep3', separator: true },
    { key: 'zoomin', label: '放大字号', hint: 'Ctrl+=', icon: ZoomInOutlined, onClick: () => settings.zoom(1) },
    { key: 'zoomout', label: '缩小字号', hint: 'Ctrl+-', icon: ZoomOutOutlined, onClick: () => settings.zoom(-1) },
  ])
}

/** 日志开关：结果只有一句话，弹出来就行，不值得占一个对话框 */
async function toggleLog(id: SessionId): Promise<void> {
  const result = await sessions.toggleLog(id)
  if (result.ok) messageApi.success(result.text)
  else messageApi.error(result.text)
}

function claim(id: SessionId): void {
  const host = slot.value
  if (host === null) return
  if (attached !== null && attached !== id) detachTerminal(attached, host)
  attached = id
  ensureTerminal(id, themeStore.terminalTheme, settings.fontSize, {
    onData: data => { void sessions.send(id, data) },
    onResize: (rows, cols) => { void sessions.reportResize(id, { rows, cols }) },
    onZoom: step => { settings.zoom(step) },
    onContextMenu: openMenuHere,
  })
  attachTerminal(id, host)
}

function release(): void {
  const host = slot.value
  if (host !== null && attached !== null) detachTerminal(attached, host)
  attached = null
  // 格子没了，为它弹的菜单也不该留着：点了会指向一个已经不存在的会话
  closeMenu()
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
  <div class="pane">
    <holder />
    <div ref="slot" class="screen" />
    <TerminalStatus :session="props.session" />
    <!-- 命令栏在状态栏下面：它一出一退会把屏幕高度改一截，格子靠 ResizeObserver 重新 fit -->
    <QuickCommandBar v-if="quick.visible" :session="props.session" />
  </div>
</template>

<style scoped>
.pane {
  /* 格子的尺寸完全由父层的 flex 决定，这里只保证终端撑满。
     边框颜色由外层决定：单屏时它几乎看不见，分屏时焦点那格被涂成主色淡描边 */
  position: relative;
  display: flex;
  flex-direction: column;
  flex: 1;
  min-width: 0;
  min-height: 0;
  overflow: hidden;
  background: var(--pane-bg);
  border: 1px solid var(--line);
  border-radius: var(--radius);
  transition: border-color var(--tap) ease;
}

/* 状态条占掉的那一行高度由 flex 算，屏幕元素只管剩下的部分 */
.screen {
  position: relative;
  flex: 1;
  min-height: 0;
}

/* 宿主元素是登记处 createElement 造的，带不上 scoped 的 data-v 属性，只能 :deep 进去。
   四周留一点内边距，光标和最后一列字符才不会贴着圆角边被削掉 */
.screen :deep(.xterm-host) {
  position: absolute;
  inset: 0;
  padding: 6px 8px 0;
}

.screen :deep(.xterm) {
  height: 100%;
}
</style>
