<script setup lang="ts">
import { computed, ref } from 'vue'
import { message } from 'ant-design-vue'

import { CloseOutlined } from '@ant-design/icons-vue'

import TerminalPane from './TerminalPane.vue'
import { currentHostDrag, endHostDrag } from '@/lib/hostDrag'
import { focusTerminal } from '@/lib/terminal/registry'
import { columnsOf, useLayoutStore } from '@/stores/layout'
import { useSessionsStore } from '@/stores/sessions'

/**
 * 宫格视图：一次摆 1 / 2 / 4 / 6 / 9 个终端，每格钉着一个会话。
 *
 * 它解决的是「同时盯几台机器」——标签模式一次只看得见一台，切过去看另一台就把这台
 * 的输出滚走了。这里每一格都是同一个 `TerminalPane`，所以状态栏、快捷命令栏、
 * 右键菜单、查找全都自带，不必为宫格重做一遍。
 *
 * 格子和会话的关系全在 `layout.cells` 里，这个组件只负责画和接拖放。
 */
const emit = defineEmits<{
  (e: 'find'): void
}>()

const layout = useLayoutStore()
const sessions = useSessionsStore()
const [messageApi, holder] = message.useMessage()

const columns = computed(() => `repeat(${columnsOf(layout.gridSize)}, minmax(0, 1fr))`)
/** 此刻鼠标悬在哪一格等着放开；-1 表示没有。 */
const over = ref(-1)

function focusIt(index: number): void {
  layout.focusOnCell(index)
  focusTerminal(layout.cells[index])
}

/**
 * 先认领这一格，再去开会话。
 *
 * 顺序反了会丢会话：`openLocal` / `openRemote` 一成功就把 `activeId` 切到新会话，
 * `App.vue` 那个「新会话落到焦点格」的 watch 于是把新会话钉在**旧**焦点格里，
 * 回头这里再 `placeCell` 把它挪走，顺手把旧格里原本那位踢出了看板。
 */
async function openLocalIn(index: number): Promise<void> {
  layout.focusOnCell(index)
  const result = await sessions.openLocal('本机终端')
  if (!result.ok) messageApi.error(result.error ?? '开不了本机终端')
  else if (result.id !== null) layout.placeCell(index, result.id)
}

/**
 * 拖进来一台主机 → 就地拨号，把新会话钉在这一格。
 *
 * 拨号失败不把格子改成空：这一格原来是别的会话，失败的原因多半是口令错了或机器不通，
 * 把用户原来那个好好的终端擦掉才是最大的伤害。
 */
async function dropOn(index: number, event: DragEvent): Promise<void> {
  const hostId = currentHostDrag()
  endHostDrag()
  over.value = -1
  if (hostId === null) return
  event.preventDefault()
  layout.focusOnCell(index)
  const result = await sessions.openRemote(hostId)
  if (!result.ok) messageApi.error(result.error ?? '连接失败')
  else if (result.id !== null) layout.placeCell(index, result.id)
}

function onDragOver(index: number, event: DragEvent): void {
  if (currentHostDrag() === null) return
  event.preventDefault()
  over.value = index
}

function onDragLeave(index: number): void {
  if (over.value === index) over.value = -1
}
</script>

<template>
  <div class="grid-board">
    <holder />
    <div class="cells" :style="{ gridTemplateColumns: columns }">
      <div
        v-for="(id, index) in layout.cells"
        :key="index"
        class="cell"
        :class="{ focused: layout.focusCell === index, over: over === index }"
        @click="focusIt(index)"
        @dragover="onDragOver(index, $event)"
        @dragleave="onDragLeave(index)"
        @drop="void dropOn(index, $event)"
      >
        <TerminalPane v-if="id !== null" :session="id" @find="emit('find')" />
        <div v-else class="blank">
          <span class="tip">拖一台主机进来</span>
          <a-button size="small" @click.stop="void openLocalIn(index)">开本机终端</a-button>
        </div>
        <a-tooltip v-if="id !== null" title="把这一格腾出来（会话还在，标签栏里能找回）">
          <button class="kick" type="button" @click.stop="layout.clearCell(index)">
            <CloseOutlined />
          </button>
        </a-tooltip>
      </div>
    </div>
  </div>
</template>

<style scoped>
.grid-board {
  display: flex;
  flex: 1;
  min-height: 0;
  min-width: 0;
}

.cells {
  display: grid;
  flex: 1;
  gap: 6px;
  min-height: 0;
}

/* 格子自己是 flex 容器：`TerminalPane` 的根写着 `flex: 1`，
   不在 flex 容器里的话它撑不满这一格，底下会空出一条没底色的缝 */
.cell {
  position: relative;
  display: flex;
  min-width: 0;
  min-height: 0;
  border-radius: var(--radius);
}

/* 焦点描在格子这一层而不是改 pane 的边框：空格子根本没有 pane，
   两种状态要能画在同一条线上，否则点亮起的位置会随有没有会话而跳 */
.cell.focused {
  box-shadow: 0 0 0 1px var(--accent-line);
}

.cell.over {
  box-shadow: 0 0 0 2px var(--accent-soft);
}

.blank {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 10px;
  flex: 1;
  min-height: 0;
  font-size: 12px;
  color: var(--text-3);
  background: var(--bg-app);
  border: 1px dashed var(--line-strong);
  border-radius: var(--radius);
}

.tip {
  letter-spacing: 0.02em;
}

/* 右上角那颗「腾出这一格」：只在悬停或焦点格上露出来，
   九格同时挂着九个 × 会把整个板子变成待办清单 */
.kick {
  position: absolute;
  top: 4px;
  right: 4px;
  display: none;
  align-items: center;
  justify-content: center;
  width: 18px;
  height: 18px;
  padding: 0;
  font-size: 11px;
  color: var(--text-2);
  background: var(--bg-raised);
  border: 1px solid var(--line-strong);
  border-radius: var(--radius-sm);
  cursor: pointer;
}

.cell:hover .kick,
.cell.focused .kick {
  display: inline-flex;
}
</style>
