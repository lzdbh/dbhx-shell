<script setup lang="ts">
import { computed, nextTick, onMounted, ref, watch } from 'vue'

import type { SessionId } from '@/lib/ipc/bindings'
import { focusTerminal } from '@/lib/terminal/registry'
import { useQuickStore } from '@/stores/quick'
import { useSessionsStore } from '@/stores/sessions'
import { useThemeStore } from '@/stores/theme'

/**
 * 快捷命令栏，形状照 Xshell 的「快捷命令」条：终端下面一个输入框，回车把整行
 * 原样发给会话，↑ / ↓ 翻本条栏的历史。
 *
 * 它解决的是「这行命令我不想在这台机器的 shell 里留痕」以及「分屏时要把同一条
 * 命令精确送进某一格」两件事——所以它是**每格一条**、各自发自己那格的会话，
 * 而不是全局一条跟着当前 tab 走。
 */
const props = defineProps<{ session: SessionId | null }>()

const quick = useQuickStore()
const sessions = useSessionsStore()
const themeStore = useThemeStore()

const text = ref('')
const input = ref<{ focus: () => void } | null>(null)
/** 正在翻第几条历史；-1 表示还没翻，框里是用户自己打的字。 */
const at = ref(-1)
/** 翻历史前把用户原本输入的东西存起来，翻完要能还回去。 */
let draft = ''

const tab = computed(() => (props.session === null ? undefined : sessions.tab(props.session)))

/**
 * 历史按归属分：同一台主机的多个会话共用一份，本机终端另算一份。
 * 按会话 id 分没意义——id 每次拨号都换，翻一次重连历史就空了。
 */
const owner = computed(() => {
  const current = tab.value
  if (!current) return null
  return current.hostId === null ? 'local' : `host:${current.hostId}`
})
const history = computed(() => (owner.value === null ? [] : quick.entries(owner.value)))

const hint = computed(() => (at.value < 0 ? '' : `历史 ${at.value + 1}/${history.value.length}`))

function recall(next: number): void {
  if (history.value.length === 0) return
  if (at.value === -1) draft = text.value
  const clamped = Math.min(Math.max(next, -1), history.value.length - 1)
  at.value = clamped
  text.value = clamped === -1 ? draft : history.value[clamped]
}

async function send(): Promise<void> {
  const command = text.value.trim()
  if (command === '' || props.session === null) return
  // 归属可能在发送这一瞬被换掉（会话刚好关掉），所以先取一次
  const key = owner.value
  if (key !== null) quick.remember(key, command)
  text.value = ''
  at.value = -1
  draft = ''
  await sessions.send(props.session, `${command}\r`)
}

function hide(): void {
  quick.visible = false
  focusTerminal(props.session)
}

function onKeydown(event: KeyboardEvent): void {
  switch (event.key) {
    case 'ArrowUp':
      event.preventDefault()
      recall(at.value + 1)
      break
    case 'ArrowDown':
      event.preventDefault()
      recall(at.value - 1)
      break
    case 'Enter':
      event.preventDefault()
      void send()
      break
    case 'Escape':
      // 这条不能冒到全局：Esc 在这里只是收栏，不该被别处当成「关掉会话」
      event.preventDefault()
      event.stopPropagation()
      hide()
      break
    default:
      break
  }
}

// 换格子（钉住的会话被换掉）时把翻历史的位置清掉：那是另一台机器的历史
watch(() => props.session, () => {
  at.value = -1
  draft = ''
})

// 栏子刚出来就接走焦点：用户按 Ctrl+Shift+K 的下一步一定是打字，不是点一下框。
// 只能挂在挂载时——组件本身是 `v-if` 出来的，`visible` 翻 true 的那一刻它还不存在，
// watch 那个标记等于是监听一个永远不会遇到的变化。
onMounted(async () => {
  await nextTick()
  input.value?.focus()
})
</script>

<template>
  <div
    v-if="session !== null"
    class="quick-bar"
    :style="{ background: themeStore.terminalTheme.background, color: themeStore.terminalTheme.foreground }"
  >
    <span class="tag">命令</span>
    <a-input
      ref="input"
      v-model:value="text"
      size="small"
      class="quick-input"
      placeholder="输入命令，回车发送到本格会话；↑ / ↓ 翻历史，Esc 收栏"
      :bordered="false"
      @keydown="onKeydown"
    />
    <span class="hint">{{ hint }}</span>
  </div>
</template>

<style scoped>
.quick-bar {
  display: flex;
  align-items: center;
  gap: 6px;
  flex: none;
  padding: 1px 8px;
  font-size: 12px;
  border-top: 1px solid rgba(128, 128, 128, 0.2);
}

.tag {
  opacity: 0.6;
  flex: none;
}

.quick-input {
  flex: 1;
  min-width: 0;
  background: transparent;
}

/* AntDV 给输入框内部铺了白底，压在终端配色上会亮出一块，这里让它透明 */
.quick-input :deep(.ant-input) {
  background: transparent;
  color: inherit;
  padding-left: 0;
}

.hint {
  flex: none;
  min-width: 60px;
  text-align: right;
  font-size: 11px;
  opacity: 0.7;
}
</style>
