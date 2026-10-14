<script setup lang="ts">
import { computed, nextTick, reactive, ref, watch } from 'vue'

import { clearFind, findText, focusTerminal, lookup, type FindHit } from '@/lib/terminal/registry'
import type { SessionId } from '@/lib/ipc/bindings'
import { useThemeStore } from '@/stores/theme'

/**
 * 终端内的查找条，形状照 Xshell 的「编辑 → 查找」：一个输入框、三个匹配开关、
 * 上一个/下一个，以及当前是第几条共几条。
 *
 * 刻意做成浮层而不是插进布局：插进去会把终端高度改一截，fit 之后整屏重排，
 * 用户只是想找个词，不该看到画面跳一下。
 */
const props = defineProps<{ session: SessionId | null; open: boolean }>()
const emit = defineEmits<{ 'update:open': [boolean] }>()

const text = ref('')
/** 三个匹配开关。合成一个对象是为了能在模板里按名字切换——模板会把 ref 拆成值，传不进 `toggle(ref)`。 */
const flags = reactive({ caseSensitive: false, wholeWord: false, regex: false })
const hit = ref<FindHit | null>(null)
// 只用到 focus()，就按这个最小形状声明：AntDV 的输入框实例包装着真正的元素，
// 写成 HTMLElement 反而拿不到它。
const input = ref<{ focus: () => void } | null>(null)

const themeStore = useThemeStore()

const options = computed(() => ({
  caseSensitive: flags.caseSensitive,
  wholeWord: flags.wholeWord,
  regex: flags.regex,
}))

/** 计数文案：没输内容时什么都不显示，免得空查找条上挂着个「无匹配」。 */
const summary = computed(() => {
  if (text.value === '') return ''
  const found = hit.value
  if (!found || found.total === 0) return '无匹配'
  // 插件给的是 0 起的下标，用户数的是第 1 条
  return `${found.index >= 0 ? found.index + 1 : '?'} / ${found.total}`
})

function search(forward: boolean): void {
  if (props.session === null) return
  hit.value = findText(props.session, text.value, options.value, forward)
}

/** 开关一拨、字一打就重新定位：正则和全字匹配的命中集完全不同，等回车太迟。 */
function refind(): void {
  search(true)
}

/** 三个开关都是「拨一下就要重查」，不然点上一次的命中还挂在屏上，看着像没生效。 */
function toggle(flag: keyof typeof flags): void {
  flags[flag] = !flags[flag]
  refind()
}

watch(text, () => { refind() })

function close(): void {
  if (props.session !== null) clearFind(props.session)
  hit.value = null
  emit('update:open', false)
  focusTerminal(props.session)
}

function onKeydown(event: KeyboardEvent): void {
  if (event.key === 'Escape') {
    // 这条不能被全局快捷键吃掉：查找条里的 Esc 只关查找条
    event.stopPropagation()
    close()
    return
  }
  if (event.key === 'Enter') {
    event.preventDefault()
    search(!event.shiftKey)
  }
}

watch(() => props.open, async opened => {
  if (!opened) return
  await nextTick()
  input.value?.focus()
  // 选中一段再按 Ctrl+F：拿选区当关键词，这是终端类客户端的通行做法
  if (text.value === '' && props.session !== null) {
    const selected = lookup(props.session)?.getSelection() ?? ''
    if (selected !== '' && !selected.includes('\n')) {
      // 赋值本身会触发下面的 watch(text) 去查，这里再查一次会跳到第 2 条
      text.value = selected
      return
    }
  }
  // 重开时把上一次的关键词留着：连续找同一个东西是常态
  if (text.value !== '') refind()
})

watch(() => props.session, () => {
  // 换了会话就把高亮清掉：旧会话的命中点在新会话里毫无意义
  hit.value = null
  if (props.session !== null) clearFind(props.session)
})
</script>

<template>
  <div
    v-if="open"
    class="find-bar"
    :style="{ background: themeStore.terminalTheme.background, color: themeStore.terminalTheme.foreground }"
    @keydown="onKeydown"
  >
    <a-input
      ref="input"
      v-model:value="text"
      size="small"
      class="find-input"
      placeholder="查找"
      :bordered="false"
    />
    <span class="count">{{ summary }}</span>
    <!-- mousedown 不交给按钮抢焦点：连续按下一个时输入框必须一直是焦点，
         否则用户第三下就得先用鼠标把光标放回去 -->
    <div class="actions" @mousedown.prevent>
      <a-tooltip title="区分大小写">
        <a-button size="small" :type="flags.caseSensitive ? 'primary' : 'text'" @click="toggle('caseSensitive')">
          Aa
        </a-button>
      </a-tooltip>
      <a-tooltip title="全字匹配">
        <a-button size="small" :type="flags.wholeWord ? 'primary' : 'text'" @click="toggle('wholeWord')">
          \b
        </a-button>
      </a-tooltip>
      <a-tooltip title="正则表达式">
        <a-button size="small" :type="flags.regex ? 'primary' : 'text'" @click="toggle('regex')">
          .*
        </a-button>
      </a-tooltip>
      <a-tooltip title="上一个（Shift+Enter）">
        <a-button size="small" @click="search(false)">
          ↑
        </a-button>
      </a-tooltip>
      <a-tooltip title="下一个（Enter）">
        <a-button size="small" @click="search(true)">
          ↓
        </a-button>
      </a-tooltip>
      <a-tooltip title="关闭（Esc）">
        <a-button size="small" @click="close()">
          ×
        </a-button>
      </a-tooltip>
    </div>
  </div>
</template>

<style scoped>
.find-bar {
  position: absolute;
  top: 6px;
  right: 18px;
  z-index: 5;
  display: flex;
  align-items: center;
  gap: 2px;
  padding: 2px 4px;
  border: 1px solid rgba(128, 128, 128, 0.35);
  border-radius: 4px;
  /* 底色由主题绑定（见模板）：浮在终端上，必须不透明，否则字压在输出上读不出来 */
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.18);
}

.find-input {
  width: 180px;
}

.actions {
  display: flex;
  align-items: center;
  gap: 2px;
}

.count {
  min-width: 52px;
  font-size: 12px;
  text-align: center;
  opacity: 0.75;
}
</style>
