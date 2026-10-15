<script setup lang="ts">
import { computed, watch } from 'vue'
import { message } from 'ant-design-vue'

import { CloseOutlined, ControlOutlined, FolderOpenOutlined, PlusOutlined, PushpinFilled, PushpinOutlined, VideoCameraOutlined } from '@ant-design/icons-vue'

import { fileWindowApi } from '@/lib/ipc/client'
import { useLayoutStore } from '@/stores/layout'
import { useQuickStore } from '@/stores/quick'
import { useSessionsStore } from '@/stores/sessions'

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

async function openPanel(): Promise<void> {
  const tab = activeTab.value
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

async function toggleLog(): Promise<void> {
  const tab = activeTab.value
  if (tab === undefined) return
  const result = await sessions.toggleLog(tab.id)
  if (result.ok) messageApi.success(result.text)
  else messageApi.error(result.text)
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
    <div class="tabs">
      <div
        v-for="tab in sessions.tabs"
        :key="tab.id"
        class="tab"
        :class="{ active: tab.id === sessions.activeId }"
        @click="sessions.activate(tab.id)"
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
          命令栏
        </a-button>
      </a-tooltip>
      <a-button size="small" :disabled="sessions.activeId === null" @click="void openPanel()">
        <template #icon>
          <FolderOpenOutlined />
        </template>
        文件面板
      </a-button>
      <a-tooltip
        :title="activeTab?.logPath ? '停止记录会话日志' : '把当前会话的输出原样抄进本地文件（口令也会被抄进去）'"
      >
        <a-button
          size="small"
          :type="activeTab?.logPath ? 'primary' : 'default'"
          :disabled="activeTab === undefined"
          @click="void toggleLog()"
        >
          <template #icon>
            <VideoCameraOutlined />
          </template>
          日志
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
