<script setup lang="ts">
import { computed } from 'vue'
import { message } from 'ant-design-vue'

import { useFilesStore } from '@/stores/files'
import { useLayoutStore } from '@/stores/layout'
import { useSessionsStore } from '@/stores/sessions'

const sessions = useSessionsStore()
const files = useFilesStore()
const layout = useLayoutStore()
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
  const id = sessions.activeId
  if (id === null) {
    messageApi.warning('先打开一个会话')
    return
  }
  const panel = await files.openForSession(id)
  if (panel) messageApi.success(`文件面板：${panel.path}`)
  else messageApi.error(files.openError ?? '打开文件面板失败')
}
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
        <span class="label">{{ tab.label }}</span>
        <span v-if="tab.cwd" class="cwd">{{ tab.cwd }}</span>
        <span v-if="tab.error" class="error" :title="tab.error">!</span>
        <!-- 钉住状态直接标在 tab 上：右格显示的是哪个会话，看一眼就知道 -->
        <button
          class="pin"
          :class="{ on: tab.id === layout.pinned }"
          :title="tab.id === layout.pinned ? '取消分屏' : '钉到右格分屏'"
          @click.stop="layout.toggle(tab.id)"
        >⑂</button>
        <button class="close" title="关闭会话" @click.stop="void sessions.close(tab.id)">×</button>
      </div>
      <button class="add" title="新建本机终端" @click="void openLocal()">＋</button>
    </div>
    <div class="actions">
      <a-button size="small" :disabled="sessions.activeId === null" @click="void openPanel()">
        文件面板
      </a-button>
      <a-tag v-if="activeTab" :color="activeTab.status === 'connected' ? 'green' : 'default'">
        {{ STATUS_TEXT[activeTab.status] ?? '' }}
      </a-tag>
    </div>
  </div>
</template>

<style scoped>
.tab-bar {
  display: flex;
  align-items: center;
  gap: 8px;
  border-bottom: 1px solid rgba(128, 128, 128, 0.25);
  padding: 4px 8px;
  background: rgba(128, 128, 128, 0.05);
}

.tabs {
  display: flex;
  align-items: stretch;
  gap: 4px;
  overflow-x: auto;
  flex: 1;
  min-width: 0;
}

.tab {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 3px 8px;
  border-radius: 4px;
  cursor: pointer;
  white-space: nowrap;
  font-size: 12px;
  max-width: 260px;
}

.tab:hover {
  background: rgba(128, 128, 128, 0.14);
}

.tab.active {
  background: rgba(128, 128, 128, 0.22);
}

.dot {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: #8c8c8c;
  flex: none;
}

.dot.connected {
  background: #52c41a;
}

.dot.connecting {
  background: #faad14;
}

.dot.error {
  background: #ff4d4f;
}

.dot.closed {
  background: transparent;
  border: 1px solid #8c8c8c;
}

.label {
  overflow: hidden;
  text-overflow: ellipsis;
}

.cwd {
  color: rgba(128, 128, 128, 0.85);
  font-size: 11px;
  overflow: hidden;
  text-overflow: ellipsis;
  direction: rtl;
}

.error {
  color: #ff4d4f;
  font-weight: 700;
}

.pin {
  border: 0;
  background: transparent;
  color: inherit;
  cursor: pointer;
  font-size: 12px;
  line-height: 1;
  opacity: 0.35;
}

.pin:hover {
  opacity: 0.8;
}

/* 钉住的 tab 一直显色，不用鼠标悬停去猜哪个会话在右格 */
.pin.on {
  opacity: 1;
  color: #1677ff;
}

.close {
  border: 0;
  background: transparent;
  color: inherit;
  cursor: pointer;
  font-size: 14px;
  line-height: 1;
  opacity: 0.6;
}

.close:hover {
  opacity: 1;
}

.add {
  border: 0;
  background: transparent;
  color: inherit;
  cursor: pointer;
  font-size: 15px;
  padding: 0 8px;
}

.actions {
  display: flex;
  align-items: center;
  gap: 8px;
  flex: none;
}
</style>
