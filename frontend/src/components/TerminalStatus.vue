<script setup lang="ts">
import { computed } from 'vue'

import type { SessionId } from '@/lib/ipc/bindings'
import { useSessionsStore } from '@/stores/sessions'
import { useSettingsStore } from '@/stores/settings'

/**
 * 格子底部的状态条，照 Xshell 的状态栏：连接状态、连到哪、当前目录、行列、字号缩放。
 *
 * 它是**每格一条**而不是全局一条：分屏时两格往往是两台不同主机，只看得到一格的
 * 状态就等于没有。数据全从 sessions store 里取，这里不持有任何状态。
 */
const props = defineProps<{ session: SessionId | null }>()

const sessions = useSessionsStore()
const settings = useSettingsStore()

const tab = computed(() => (props.session === null ? undefined : sessions.tab(props.session)))

const STATUS_TEXT: Record<string, string> = {
  connecting: '连接中',
  connected: '已连接',
  closed: '已结束',
  error: '出错',
}

const PROTOCOL_TEXT: Record<string, string> = {
  ssh: 'SSH',
  local: '本机',
  telnet: 'Telnet',
}

const statusText = computed(() => (tab.value ? STATUS_TEXT[tab.value.status] ?? '' : ''))
/** 本机终端没有端点，用协议名占位，免得状态条左边空出一截。 */
const target = computed(() => {
  const current = tab.value
  if (!current) return ''
  const endpoint = current.endpoint
  if (!endpoint) return PROTOCOL_TEXT[current.protocol] ?? current.protocol
  return `${endpoint.username}@${endpoint.host}:${endpoint.port}`
})
const size = computed(() => (tab.value ? `${tab.value.pty.cols}×${tab.value.pty.rows}` : ''))
const zoom = computed(() => `${settings.zoomPercent()}%`)
</script>

<template>
  <div v-if="tab" class="status-bar">
    <span class="item"><span class="dot" :class="tab.status" />{{ statusText }}</span>
    <span class="item">{{ target }}</span>
    <!-- 目录只有拿到 OSC 7 才有：没有 shell 集成的远端永远报不出，宁可空着也不要一行「未知」 -->
    <span v-if="tab.cwd" class="item cwd" :title="tab.cwd">{{ tab.cwd }}</span>
    <span class="spacer" />
    <span v-if="tab.error" class="item error" :title="tab.error">{{ tab.error }}</span>
    <span class="item">{{ size }}</span>
    <span class="item">{{ zoom }}</span>
  </div>
</template>

<style scoped>
.status-bar {
  display: flex;
  align-items: center;
  gap: 14px;
  flex: none;
  height: 22px;
  padding: 0 8px;
  font-size: 11px;
  line-height: 22px;
  opacity: 0.85;
  border-top: 1px solid rgba(128, 128, 128, 0.2);
  white-space: nowrap;
  overflow: hidden;
}

.item {
  display: inline-flex;
  align-items: center;
  gap: 5px;
}

/* 目录可能很长，只让它这一项收缩省略，其余项保持完整可读 */
.cwd {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  /* rtl 让省略号吃开头、留住末尾——末尾才是用户关心的那一层目录 */
  direction: rtl;
  text-align: left;
}

.spacer {
  flex: 1;
  min-width: 8px;
}

.error {
  color: #ff4d4f;
  max-width: 40%;
  overflow: hidden;
  text-overflow: ellipsis;
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
</style>
