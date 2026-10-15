<script setup lang="ts">
import { computed, watch } from 'vue'
import { message } from 'ant-design-vue'

import { CloseCircleOutlined, CloseOutlined, ControlOutlined, FolderOpenOutlined, ForwardOutlined, PlusOutlined, PushpinFilled, PushpinOutlined, SyncOutlined, VideoCameraOutlined } from '@ant-design/icons-vue'

import { fileWindowApi } from '@/lib/ipc/client'
import { openMenu } from '@/lib/contextMenu'
import type { MenuItem, MenuPoint } from '@/lib/contextMenu'
import { useLayoutStore } from '@/stores/layout'
import { useQuickStore } from '@/stores/quick'
import { useSessionsStore } from '@/stores/sessions'
import type { SessionTab } from '@/stores/sessions'

const sessions = useSessionsStore()
const layout = useLayoutStore()
const quick = useQuickStore()
const [messageApi, holder] = message.useMessage()

/** 当前 tab。模板里不能把 `number | null` 收窄，所以在脚本里算好。 */
const activeTab = computed(() => (sessions.activeId === null ? undefined : sessions.tab(sessions.activeId)))

/** 状态点的颜色。closed/error 用同色系但不同形状，色盲用户也能分辨。 */
const STATUS_TEXT: Record<string, string> = {
  connecting: '连接中',
  connected: '已连接',
  closed: '已结束',
  error: '出错',
}

async function openLocal(): Promise<void> {
  const result = await sessions.openLocal('本机终端')
  if (!result.ok) messageApi.error(result.error ?? '无法打开本机终端')
}

async function openPanel(tab: SessionTab | undefined): Promise<void> {
  if (tab === undefined) {
    messageApi.warning('先打开一个会话')
    return
  }
  // 本机终端没有 hostId，telnet 也没有 SFTP 可开——两种情况都直接说明，
  // 而不是让后端回一句英文的「找不到主机」。
  if (tab.hostId === null) {
    messageApi.warning('本机终端没有远端文件')
    return
  }
  if (tab.protocol !== 'ssh') {
    messageApi.warning('该协议没有文件管理')
    return
  }
  try {
    await fileWindowApi.open(tab.hostId)
  } catch (err) {
    messageApi.error(err instanceof Error ? err.message : '打开文件管理窗口失败')
  }
}

async function toggleLog(tab: SessionTab | undefined): Promise<void> {
  if (tab === undefined) return
  const result = await sessions.toggleLog(tab.id)
  if (result.ok) messageApi.success(result.text)
  else messageApi.error(result.text)
}

/**
 * 重新连接。
 *
 * 只有已结束/出错的标签页才给这个入口：会话还活着时「重连」的真实含义是断开再拨，
 * 那是一个会把用户正在跑的命令掐掉的动作，不该藏在一次右键里。
 */
async function reconnect(tab: SessionTab): Promise<void> {
  const { hostId, label } = tab
  await sessions.close(tab.id)
  const result = hostId === null ? await sessions.openLocal(label) : await sessions.openRemote(hostId)
  if (!result.ok) messageApi.error(result.error ?? '重新连接失败')
}

async function closeOthers(tab: SessionTab): Promise<void> {
  // 先取快照再逐个关：close 会重排 tabs，边关边遍历会漏掉后半截
  for (const other of sessions.tabs.filter(t => t.id !== tab.id)) await sessions.close(other.id)
  sessions.activate(tab.id)
}

async function closeRight(tab: SessionTab): Promise<void> {
  const index = sessions.tabs.findIndex(t => t.id === tab.id)
  if (index < 0) return
  for (const other of sessions.tabs.slice(index + 1)) await sessions.close(other.id)
}

async function closeAll(): Promise<void> {
  for (const other of [...sessions.tabs]) await sessions.close(other.id)
}

/**
 * 标签页的右键菜单。
 *
 * 动作的对象永远是**被右键的那一格**，不是当前活动格：一屏七八个标签时，用户想关的
 * 就是手指下面那个，先激活再弹菜单会让键盘焦点跟着鼠标跑。
 */
function openTabMenu(at: MenuPoint, tab: SessionTab): void {
  const dead = tab.status === 'closed' || tab.status === 'error'
  const pinned = tab.id === layout.pinned
  const index = sessions.tabs.findIndex(t => t.id === tab.id)
  const list: MenuItem[] = [
    { key: 'reconnect', label: '重新连接', icon: SyncOutlined, disabled: !dead, onClick: () => { void reconnect(tab) } },
    { key: 'panel', label: '打开文件面板', icon: FolderOpenOutlined, disabled: tab.hostId === null || tab.protocol !== 'ssh', onClick: () => { void openPanel(tab) } },
    // 分屏是「两格跟着 tab 走」那一套的动作，宫格模式里没有右格可言；
    // 那里对应的问题是「这个会话摆进哪一格」，所以换成放进当前格
    layout.mode === 'tabs'
      ? { key: 'pin', label: pinned ? '取消分屏' : '钉到右格分屏', hint: 'Ctrl+E', icon: pinned ? PushpinFilled : PushpinOutlined, onClick: () => { layout.toggle(tab.id) } }
      : { key: 'cell', label: '放进当前格', icon: PushpinOutlined, onClick: () => { layout.placeCell(layout.focusCell, tab.id) } },
    { key: 'log', label: tab.logPath === null ? '记录日志到文件' : '停止记录日志', icon: VideoCameraOutlined, onClick: () => { void toggleLog(tab) } },
    { key: 'sep', separator: true },
    { key: 'close', label: '关闭', hint: 'Ctrl+W', icon: CloseOutlined, onClick: () => { void sessions.close(tab.id) } },
    { key: 'others', label: '关闭其他', icon: CloseCircleOutlined, disabled: sessions.tabs.length < 2, onClick: () => { void closeOthers(tab) } },
    { key: 'right', label: '关闭右侧', icon: ForwardOutlined, disabled: index < 0 || index === sessions.tabs.length - 1, onClick: () => { void closeRight(tab) } },
    { key: 'sep2', separator: true },
    { key: 'all', label: '关闭全部', danger: true, onClick: () => { void closeAll() } },
  ]
  openMenu(at, list)
}

/** 轨道空白处的菜单：没有标签页可指时，右键至少要能开一个。 */
function openTrackMenu(at: MenuPoint): void {
  openMenu(at, [
    { key: 'new', label: '新建本机终端', hint: 'Ctrl+T', icon: PlusOutlined, onClick: () => { void openLocal() } },
    { key: 'sep', separator: true },
    { key: 'all', label: '关闭全部', danger: true, disabled: sessions.tabs.length === 0, onClick: () => { void closeAll() } },
  ])
}

// 日志写不下去 ⇒ 后端自己收了灯。这条只在这一处出现，所以取完就清，不重复弹
watch(() => sessions.logNotice, (text) => {
  if (text === null) return
  sessions.takeLogNotice()
  messageApi.warning(text)
})
</script>

<template>
  <div class="tab-bar">
    <holder />
    <div class="tabs" @contextmenu.self.prevent="openTrackMenu({ x: $event.clientX, y: $event.clientY })">
      <div
        v-for="tab in sessions.tabs"
        :key="tab.id"
        class="tab"
        :class="{ active: tab.id === sessions.activeId }"
        @click="sessions.activate(tab.id)"
        @contextmenu.prevent.stop="openTabMenu({ x: $event.clientX, y: $event.clientY }, tab)"
      >
        <span class="dot" :class="tab.status" />
        <!-- 正在记录的那一格自己带灯：多标签时不用切过去看按钮状态 -->
        <span v-if="tab.logPath" class="rec" :title="`正在记录：${tab.logPath}`" />
        <span class="label">{{ tab.label }}</span>
        <span v-if="tab.cwd" class="cwd">{{ tab.cwd }}</span>
        <span v-if="tab.error" class="error" :title="tab.error">!</span>
        <!-- 钉住状态直接标在 tab 上：右格显示的是哪个会话，看一眼就知道 -->
        <button
          class="icon-btn pin"
          :class="{ on: tab.id === layout.pinned }"
          :title="tab.id === layout.pinned ? '取消分屏' : '钉到右格分屏'"
          @click.stop="layout.toggle(tab.id)"
        >
          <PushpinFilled v-if="tab.id === layout.pinned" />
          <PushpinOutlined v-else />
        </button>
        <button class="icon-btn close" title="关闭会话" @click.stop="void sessions.close(tab.id)">
          <CloseOutlined />
        </button>
      </div>
      <button class="icon-btn add" title="新建本机终端" @click="void openLocal()">
        <PlusOutlined />
      </button>
    </div>
    <div class="actions">
      <!-- 三个开关只画图标，名字交给悬浮提示：这条栏优先把宽度让给标签页。
           开/关状态不能只靠图标，所以留着 primary 高亮 -->
      <!-- 开关是全局的，栏子却是每格一条、各发各的会话。一个会话都没有时开了也只
           剩一条空栏，所以直接禁用，别让用户按了没反应去猜是不是坏了 -->
      <a-tooltip :title="quick.visible ? '收起快捷命令栏（Ctrl+Shift+K）' : '打开快捷命令栏（Ctrl+Shift+K）'">
        <a-button
          size="small"
          :type="quick.visible ? 'primary' : 'default'"
          :disabled="sessions.tabs.length === 0"
          @click="quick.toggle()"
        >
          <template #icon>
            <ControlOutlined />
          </template>
        </a-button>
      </a-tooltip>
      <a-tooltip title="打开文件面板">
        <a-button size="small" :disabled="sessions.activeId === null" @click="void openPanel(activeTab)">
          <template #icon>
            <FolderOpenOutlined />
          </template>
        </a-button>
      </a-tooltip>
      <a-tooltip
        :title="activeTab?.logPath ? '停止记录会话日志' : '把当前会话的输出原样抄进本地文件（口令也会被抄进去）'"
      >
        <a-button
          size="small"
          :type="activeTab?.logPath ? 'primary' : 'default'"
          :disabled="activeTab === undefined"
          @click="void toggleLog(activeTab)"
        >
          <template #icon>
            <VideoCameraOutlined />
          </template>
        </a-button>
      </a-tooltip>
      <!-- 状态自己画点：a-tag 的 green 是组件库预设色，和标签页里那个点是两套颜色，
           同一件事在一条栏里出现两种绿 -->
      <span v-if="activeTab" class="status">
        <span class="dot" :class="activeTab.status" />
        {{ STATUS_TEXT[activeTab.status] ?? '' }}
      </span>
    </div>
  </div>
</template>

<style scoped>
/* 标签轨道：和窗口底同色，比终端那块暗一档，当前标签因此能「浮」到内容上。
   高度写死而不是靠内容撑：标签一多一行行高变了，下面的格子会跟着跳。 */
.tab-bar {
  display: flex;
  align-items: center;
  gap: 8px;
  flex: none;
  height: 40px;
  padding: 0 8px;
  background: var(--bg-app);
  border-bottom: 1px solid var(--line);
}

.tabs {
  display: flex;
  align-items: center;
  gap: 2px;
  overflow-x: auto;
  flex: 1;
  min-width: 0;
}

.tab {
  display: flex;
  align-items: center;
  gap: 6px;
  height: 30px;
  padding: 0 10px;
  border-radius: var(--radius) var(--radius) 0 0;
  cursor: pointer;
  white-space: nowrap;
  font-size: 12px;
  max-width: 260px;
  color: var(--text-2);
  transition: background var(--tap) ease, color var(--tap) ease;
}

.tab:hover {
  background: var(--bg-hover);
  color: var(--text-1);
}

/* 当前标签：底色用终端那一格的表面，顶部压一条主色，和旁边「没在看」的标签一眼分开。
   不做「标签与内容连成一体」的负边框那套：横向滚动的容器会把溢出裁掉，一半连上一半没连上。 */
.tab.active {
  background: var(--pane-bg);
  color: var(--text-1);
  box-shadow: inset 0 2px 0 var(--accent);
}

.dot {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: var(--text-3);
  flex: none;
}

.dot.connected {
  background: var(--ok);
}

.dot.connecting {
  background: var(--warn);
}

.dot.error {
  background: var(--err);
}

.dot.closed {
  background: transparent;
  border: 1px solid var(--text-3);
}

/* 录制中的红点。和状态点同尺寸，靠颜色和呼吸区分：多标签时一眼看出哪一格在写文件 */
.rec {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: var(--err);
  flex: none;
  animation: rec-breathe 1.8s ease-in-out infinite;
}

@keyframes rec-breathe {
  0%,
  100% {
    opacity: 1;
  }

  50% {
    opacity: 0.3;
  }
}

/* 前庭敏感的用户不该被一个 blinking 的点折磨，指示灯灭成常亮就行 */
@media (prefers-reduced-motion: reduce) {
  .rec {
    animation: none;
  }
}

.label {
  overflow: hidden;
  text-overflow: ellipsis;
}

.cwd {
  color: var(--text-3);
  font-size: 12px;
  font-family: ui-monospace, Menlo, Consolas, monospace;
  overflow: hidden;
  text-overflow: ellipsis;
  direction: rtl;
}

.error {
  color: var(--err);
  font-weight: 700;
}

/* 标签里的三个图标按钮共用这一套：靠透明度分层，不用各自描一遍边框 */
.icon-btn {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  flex: none;
  width: 18px;
  height: 18px;
  padding: 0;
  border: 0;
  border-radius: var(--radius-sm);
  background: transparent;
  color: inherit;
  font-size: 11px;
  cursor: pointer;
  opacity: 0.5;
  transition: opacity var(--tap) ease, background var(--tap) ease;
}

.icon-btn:hover {
  opacity: 1;
  background: var(--bg-active);
}

/* 钉住的 tab 一直显色，不用鼠标悬停去猜哪个会话在右格 */
.pin.on {
  opacity: 1;
  color: var(--accent);
}

.add {
  width: 24px;
  height: 24px;
  font-size: 13px;
  opacity: 0.7;
}

.actions {
  display: flex;
  align-items: center;
  gap: 8px;
  flex: none;
}

/* 状态自己画点，和标签页里那个点同一套颜色，所以复用 .dot */
.status {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  font-size: 12px;
  color: var(--text-2);
  flex: none;
}
</style>
