<script setup lang="ts">
import { computed, reactive, ref, watch } from 'vue'

import {
  ArrowUpOutlined,
  CopyOutlined,
  DeleteOutlined,
  DownloadOutlined,
  ExportOutlined,
  FileAddOutlined,
  FolderAddOutlined,
  FormOutlined,
  KeyOutlined,
  ReloadOutlined,
  SelectOutlined,
  UploadOutlined,
} from '@ant-design/icons-vue'

import type { FileEntry } from '../lib/ipc/bindings'
import { clipboardApi } from '../lib/ipc/client'
import { iconOf, typeName } from '../lib/fileType'
import { formatSize, formatTime } from '../lib/format'
import { openMenu } from '../lib/contextMenu'
import type { MenuItem, MenuPoint } from '../lib/contextMenu'
import { currentDrag, dropTargetOf, endDrag, writeDrag } from '../lib/fileDrag'
import type { PaneSide } from '../lib/fileDrag'
import FileIcon from './FileIcon.vue'

/**
 * 资源管理器风格的一栏文件列表。
 *
 * 之所以不用 `a-table`：资源管理器的选择是「单击高亮 + Ctrl/Shift 加选 + 方向键移动」，
 * 而表格组件的勾选列、分页、行 hover 都在跟这个观感抢地盘。这里自己画行，
 * 列宽、排序、键盘操作都按 Windows 的口径来，代价是多写了几十条比较逻辑。
 */
const props = defineProps<{
  side: PaneSide
  /** 当前目录，直接显示在地址栏里。 */
  path: string
  entries: FileEntry[]
  loading: boolean
  error: string | null
  selected: string[]
  /** 未连接时的远端栏：所有写操作置灰，但列表本身仍可看。 */
  enabled: boolean
  /** 已经在根（`C:\` 或 `/`）时「上级」置灰。 */
  canGoUp: boolean
  /** 对侧此刻能不能收东西——没连上远端时「传到对面」要灰掉。 */
  canTransfer: boolean
  /** 后台正在做的事（外部编辑器往返），显示在状态条上。 */
  busy: string | null
}>()

const emit = defineEmits<{
  (e: 'update:selected', value: string[]): void
  (e: 'go', path: string): void
  (e: 'up'): void
  (e: 'refresh'): void
  (e: 'mkdir', name: string): void
  (e: 'createFile', name: string): void
  (e: 'remove', entries: FileEntry[]): void
  (e: 'rename', from: FileEntry, name: string): void
  (e: 'chmod', entry: FileEntry, mode: number): void
  (e: 'open', entry: FileEntry): void
  /** 把选中的这些送到对侧。方向（上传还是下载）由父组件按 `side` 决定。 */
  (e: 'transfer', entries: FileEntry[]): void
  /** 对侧拖过来的这批路径，落到这一栏。 */
  (e: 'drop', paths: string[]): void
  /** 关掉那条错误提示（用户看完就知道了，不需要它一直占着位置）。 */
  (e: 'dismiss'): void
  /** 正在填名字：远端栏要据此压住目录跟随，否则用户敲一半面板就跳走了。 */
  (e: 'editing', value: boolean): void
}>()

/* ── 排序与显示顺序 ─────────────────────────────────────── */

type SortKey = 'name' | 'mtime' | 'type' | 'size'

const sort = reactive<{ key: SortKey, dir: 1 | -1 }>({ key: 'name', dir: 1 })

function toggleSort(key: SortKey): void {
  if (sort.key === key) sort.dir = sort.dir === 1 ? -1 : 1
  else {
    sort.key = key
    // 时间列第一次点按「最新在前」更符合用途：找刚动过的文件
    sort.dir = key === 'mtime' ? -1 : 1
  }
}

/** 目录恒在前面，和 Windows 默认一致；组内再按当前列排。 */
const rows = computed(() => {
  const rank = (e: FileEntry) => (e.kind === 'dir' ? 0 : 1)
  const list = [...props.entries]
  list.sort((a, b) => {
    const grouped = rank(a) - rank(b)
    if (grouped !== 0) return grouped
    const base = compare(a, b)
    return sort.dir === 1 ? base : -base
  })
  return list
})

function compare(a: FileEntry, b: FileEntry): number {
  switch (sort.key) {
    case 'name':
      return a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: 'base' })
    case 'type':
      return typeName(a.name, a.kind).localeCompare(typeName(b.name, b.kind))
        || a.name.localeCompare(b.name, undefined, { numeric: true })
    case 'size':
      return (a.kind === 'dir' ? 0 : a.size) - (b.kind === 'dir' ? 0 : b.size)
    case 'mtime': {
      // 服务端没给 mtime 的排在最后，而不是当成 0 年
      const left = a.mtime_ms ?? Number.MAX_SAFE_INTEGER
      const right = b.mtime_ms ?? Number.MAX_SAFE_INTEGER
      return left - right
    }
  }
}

/* ── 选择 ───────────────────────────────────────────────── */

const anchor = ref(-1)

function pick(index: number, event: { ctrlKey: boolean, metaKey: boolean, shiftKey: boolean }): void {
  const entry = rows.value[index]
  if (!entry) return
  const keep = event.ctrlKey || event.metaKey
  if (event.shiftKey && anchor.value >= 0) {
    const [from, to] = [anchor.value, index].sort((x, y) => x - y)
    const range = rows.value.slice(from, to + 1).map(e => e.path)
    // Shift 扩选在资源管理器里是「替换」而不是「追加」，保持一致
    emit('update:selected', range)
    return
  }
  if (keep) {
    const next = props.selected.includes(entry.path)
      ? props.selected.filter(p => p !== entry.path)
      : [...props.selected, entry.path]
    emit('update:selected', next)
    anchor.value = index
    return
  }
  emit('update:selected', [entry.path])
  anchor.value = index
}

function selectAll(): void {
  emit('update:selected', rows.value.map(e => e.path))
}

function clearSelection(): void {
  emit('update:selected', [])
  anchor.value = -1
}

watch(() => props.path, () => { anchor.value = -1 })

const selectedEntries = computed(() => rows.value.filter(e => props.selected.includes(e.path)))
const single = computed(() => selectedEntries.value.length === 1 ? selectedEntries.value[0] : null)
const selectedBytes = computed(() =>
  selectedEntries.value.reduce((sum, e) => sum + (e.kind === 'dir' ? 0 : e.size), 0),
)

function onKeydown(event: KeyboardEvent): void {
  if (creating.value !== null || renaming.open) return
  const count = rows.value.length
  if (count === 0) return
  const current = anchor.value >= 0 ? anchor.value : props.selected.length > 0
    ? rows.value.findIndex(e => e.path === props.selected[props.selected.length - 1])
    : -1
  if (event.ctrlKey && event.key.toLowerCase() === 'a') {
    event.preventDefault()
    selectAll()
    return
  }
  if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
    event.preventDefault()
    const next = event.key === 'ArrowDown' ? Math.min(current + 1, count - 1) : Math.max(current - 1, 0)
    anchor.value = next
    emit('update:selected', [rows.value[next].path])
    return
  }
  if (event.key === 'Enter' && current >= 0) {
    event.preventDefault()
    emit('open', rows.value[current])
    return
  }
  if (event.key === 'Escape') {
    clearSelection()
    return
  }
  // Backspace 是「上一级」、Delete 是「删除」，和键盘上的资源管理器一致
  if (event.key === 'Backspace') {
    event.preventDefault()
    if (props.canGoUp && props.enabled) emit('up')
    return
  }
  if (event.key === 'Delete' && props.selected.length > 0 && props.enabled) {
    event.preventDefault()
    askRemove()
  }
}

/* ── 右键菜单 ───────────────────────────────────────────── */

/**
 * 这一行该不该被算进「选中的这批」。
 *
 * 资源管理器的口径：右键落在已经多选出来的区域里时保持整组选择，否则只选中这一行。
 * 这里要自己判而不是读 `props.selected`，因为上面那次 `emit` 要等父组件重渲染才生效，
 * 而菜单项的禁用态在同一个tick 里就得算出来。
 */
function targetsFor(entry: FileEntry): FileEntry[] {
  return props.selected.includes(entry.path) ? selectedEntries.value : [entry]
}

function copyPath(entry: FileEntry): void {
  void clipboardApi.write(entry.path).catch(() => undefined)
}

/** 某一行的菜单。方向词按栏来说：本机栏是「上传到远端」，远端栏是「下载到本机」。 */
function rowMenu(at: MenuPoint, entry: FileEntry, index: number): void {
  if (!props.selected.includes(entry.path)) {
    emit('update:selected', [entry.path])
    anchor.value = index
  }
  const targets = targetsFor(entry)
  const one = targets.length === 1
  const to = props.side === 'local' ? '上传到远端' : '下载到本机'
  openMenu(at, [
    { key: 'open', label: entry.kind === 'dir' ? '打开' : '打开 / 编辑', hint: '双击', icon: ExportOutlined, disabled: !one || !props.enabled, onClick: () => { emit('open', entry) } },
    { key: 'transfer', label: one ? `${to} ${entry.name}` : `${to} ${targets.length} 项`, icon: props.side === 'local' ? UploadOutlined : DownloadOutlined, disabled: !props.canTransfer, onClick: () => { emit('transfer', targets) } },
    { key: 'copy', label: '复制路径', hint: '到剪贴板', icon: CopyOutlined, disabled: !one, onClick: () => { copyPath(entry) } },
    { key: 'sep1', separator: true },
    { key: 'rename', label: '重命名', icon: FormOutlined, disabled: !one || !props.enabled, onClick: startRename },
    ...(props.side === 'remote'
      ? [{ key: 'chmod', label: '权限', icon: KeyOutlined, disabled: !one || !props.enabled, onClick: openChmod } satisfies MenuItem]
      : []),
    { key: 'sep2', separator: true },
    { key: 'remove', label: one ? '删除' : `删除 ${targets.length} 项`, icon: DeleteOutlined, danger: true, disabled: !props.enabled, onClick: askRemove },
    { key: 'refresh', label: '刷新', icon: ReloadOutlined, onClick: () => { emit('refresh') } },
  ])
}

/** 空白处的菜单：这里没有「目标行」，能做的只有造新东西和重读目录。 */
function blankMenu(at: MenuPoint): void {
  openMenu(at, [
    { key: 'mkdir', label: '新建文件夹', icon: FolderAddOutlined, disabled: !props.enabled, onClick: () => { startCreate('dir') } },
    { key: 'file', label: '新建空文件', icon: FileAddOutlined, disabled: !props.enabled, onClick: () => { startCreate('file') } },
    { key: 'sep1', separator: true },
    { key: 'all', label: '全选', icon: SelectOutlined, disabled: rows.value.length === 0, onClick: selectAll },
    { key: 'none', label: '取消选择', disabled: props.selected.length === 0, onClick: clearSelection },
    { key: 'sep2', separator: true },
    { key: 'refresh', label: '刷新', icon: ReloadOutlined, onClick: () => { emit('refresh') } },
  ])
}

function onRowContextmenu(entry: FileEntry, index: number, event: MouseEvent): void {
  // 不拦住的话 WebView2 会弹自己的菜单，我们的浮层会被它盖住
  event.preventDefault()
  event.stopPropagation()
  if (creating.value !== null) return
  rowMenu({ x: event.clientX, y: event.clientY }, entry, index)
}

function onBlankContextmenu(event: MouseEvent): void {
  event.preventDefault()
  if (creating.value !== null) return
  blankMenu({ x: event.clientX, y: event.clientY })
}

/* ── 跨栏拖拽 ───────────────────────────────────────────── */

const dropping = ref(false)

function onRowDragstart(entry: FileEntry, index: number, event: DragEvent): void {
  if (!props.enabled) return
  if (!props.selected.includes(entry.path)) {
    emit('update:selected', [entry.path])
    anchor.value = index
  }
  const targets = targetsFor(entry)
  writeDrag(event, { side: props.side, paths: targets.map(t => t.path) })
}

/**
 * 只有「对面拖过来的」才接。
 *
 * `dragover` 阶段浏览器不给读载荷内容（安全限制），所以来源栏取自模块里那份快照。
 */
function onDragover(event: DragEvent): void {
  const payload = dropTargetOf(currentDrag(), props.side)
  if (payload === null || !props.canTransfer) return
  event.preventDefault()
  if (event.dataTransfer !== null) event.dataTransfer.dropEffect = 'copy'
  dropping.value = true
}

function onDrop(event: DragEvent): void {
  dropping.value = false
  const payload = dropTargetOf(currentDrag(), props.side)
  endDrag()
  if (payload === null || !props.canTransfer) return
  event.preventDefault()
  emit('drop', payload.paths)
}

/**
 * 拖拽收尾。
 *
 * 没落到任何一栏上（松在窗口外、松在本栏里）时浏览器不会替我们清快照，而下一轮
 * `dragover` 判的就是这份快照——留着它，之后鼠标划过对侧就会凭空亮起来。
 */
function onDragend(): void {
  endDrag()
  dropping.value = false
}

/* ── 地址栏 ─────────────────────────────────────────────── */

const draftPath = ref(props.path)
const pathFocused = ref(false)

watch(() => props.path, (value) => {
  if (!pathFocused.value) draftPath.value = value
})

function commitPath(): void {
  const target = draftPath.value.trim()
  if (target !== '' && target !== props.path) emit('go', target)
  else draftPath.value = props.path
}

/* ── 新建 / 重命名 的内联输入 ───────────────────────────── */

const creating = ref<'dir' | 'file' | null>(null)
const draftName = ref('')

const renaming = reactive<{ open: boolean, from: FileEntry | null, to: string }>({ open: false, from: null, to: '' })

function startCreate(kind: 'dir' | 'file'): void {
  creating.value = kind
  draftName.value = ''
  emit('editing', true)
}

function endCreate(): void {
  const kind = creating.value
  const name = draftName.value.trim()
  creating.value = null
  draftName.value = ''
  emit('editing', false)
  if (!name || kind === null) return
  if (kind === 'dir') emit('mkdir', name)
  else emit('createFile', name)
}

function startRename(): void {
  const entry = single.value
  if (!entry) return
  renaming.open = true
  renaming.from = entry
  renaming.to = entry.name
  emit('editing', true)
}

function submitRename(): void {
  const from = renaming.from
  const name = renaming.to.trim()
  renaming.open = false
  renaming.from = null
  emit('editing', false)
  if (from && name !== '' && name !== from.name) emit('rename', from, name)
}

/* ── 删除 ───────────────────────────────────────────────── */

const removeOpen = ref(false)

function askRemove(): void {
  if (selectedEntries.value.length === 0) return
  removeOpen.value = true
}

function confirmRemove(): void {
  removeOpen.value = false
  emit('remove', [...selectedEntries.value])
}

/* ── 权限 ───────────────────────────────────────────────── */

const chmodOpen = ref(false)
const chmodMode = ref(0)
const chmodText = ref('')
/** 越界的输入不猜：直接把「确定」置灰。 */
const chmodValid = computed(() => /^[0-7]{1,4}$/.test(chmodText.value.trim()))

const chmodBits = computed(() => [2, 1, 0].map(shift => ({
  who: ['所有者', '分组', '其他'][2 - shift],
  shift,
})))

function openChmod(): void {
  const entry = single.value
  if (!entry) return
  chmodMode.value = entry.mode ?? 0o644
  chmodText.value = (entry.mode ?? 0o644).toString(8).padStart(3, '0')
  chmodOpen.value = true
}

function toggleBit(shift: number, bit: number): void {
  const at = bit << (shift * 3)
  const next = chmodMode.value ^ at
  chmodMode.value = next & 0o7777
  chmodText.value = (next & 0o7777).toString(8).padStart(3, '0')
}

function onChmodText(): void {
  const raw = chmodText.value.trim()
  if (!/^[0-7]{1,4}$/.test(raw)) return
  chmodMode.value = Number.parseInt(raw, 8) & 0o7777
}

function bitOn(shift: number, bit: number): boolean {
  return (chmodMode.value & (bit << (shift * 3))) !== 0
}

function submitChmod(): void {
  const entry = single.value
  chmodOpen.value = false
  if (entry && chmodValid.value) emit('chmod', entry, Number.parseInt(chmodText.value.trim(), 8))
}

function onRemoveCancel(): void {
  removeOpen.value = false
}
</script>

<template>
  <section class="pane">
    <div class="toolbar">
      <a-button size="small" :disabled="!canGoUp || !enabled" @click="emit('up')">
        <template #icon>
          <ArrowUpOutlined />
        </template>
        上级
      </a-button>
      <a-button size="small" :disabled="!enabled" @click="emit('refresh')">刷新</a-button>
      <!-- 新建/重命名/权限/删除不在这里了：它们都必须先有一个「对着谁做」的目标，
           而目标就是鼠标正指着的那一行。放在工具栏上等于让用户先点行再抬头找按钮，
           放在右键里则一步到位。 -->
      <slot name="tools" />
    </div>

    <div class="address">
      <span class="side">{{ side === 'local' ? '本机' : '远端' }}</span>
      <a-input
        v-model:value="draftPath"
        class="path-input"
        size="small"
        :disabled="!enabled"
        @focus="pathFocused = true"
        @blur="pathFocused = false"
        @press-enter="commitPath"
      />
    </div>

    <a-input
      v-if="creating !== null"
      v-model:value="draftName"
      size="small"
      :placeholder="creating === 'dir' ? '新文件夹名，回车创建' : '新文件名，回车创建'"
      autofocus
      @press-enter="endCreate"
      @blur="endCreate"
    />

    <a-alert v-if="error" :message="error" type="error" show-icon closable @close="emit('dismiss')" />

    <div
      class="list"
      tabindex="0"
      :class="{ loading, dropping }"
      @keydown="onKeydown"
      @contextmenu.self.prevent="onBlankContextmenu"
      @dragover="onDragover"
      @dragleave="dropping = false"
      @drop="onDrop"
    >
      <div class="head">
        <button class="h name" :class="{ on: sort.key === 'name' }" @click="toggleSort('name')">名称<i>{{ sort.key === 'name' && sort.dir < 0 ? '▾' : '▴' }}</i></button>
        <button class="h mtime" :class="{ on: sort.key === 'mtime' }" @click="toggleSort('mtime')">修改日期<i>{{ sort.key === 'mtime' && sort.dir < 0 ? '▾' : '▴' }}</i></button>
        <button class="h type" :class="{ on: sort.key === 'type' }" @click="toggleSort('type')">类型<i>{{ sort.key === 'type' && sort.dir < 0 ? '▾' : '▴' }}</i></button>
        <button class="h size" :class="{ on: sort.key === 'size' }" @click="toggleSort('size')">大小<i>{{ sort.key === 'size' && sort.dir < 0 ? '▾' : '▴' }}</i></button>
      </div>

      <!-- 资源管理器的「..」：单击就回上级，不用先把光标挪到工具栏那颗按钮上。
           放在列表内部而不是表头上方，是因为它和行共用这套滚动区——长目录里往上滚
           到头就能看见它，符合用户已经在列表里找东西的预期。 -->
      <div
        v-if="enabled && canGoUp"
        class="row up-row"
        title="返回上级"
        @click="emit('up')"
        @dblclick="emit('up')"
        @contextmenu.prevent.stop="onBlankContextmenu"
      >
        <span class="c name">
          <FileIcon icon="folder" />
          <span class="label">...</span>
        </span>
        <span class="c mtime" />
        <span class="c type">上级</span>
        <span class="c size" />
      </div>

      <div v-if="rows.length === 0" class="empty">{{ loading ? '载入中…' : '此文件夹为空' }}</div>
      <div
        v-for="(entry, index) in rows"
        :key="entry.path"
        class="row"
        :class="{ sel: selected.includes(entry.path) }"
        :draggable="enabled ? 'true' : 'false'"
        :title="entry.path"
        @click="(event: MouseEvent) => pick(index, event)"
        @dblclick="emit('open', entry)"
        @contextmenu.prevent.stop="onRowContextmenu(entry, index, $event)"
        @dragstart="onRowDragstart(entry, index, $event)"
        @dragend="onDragend"
      >
        <span class="c name">
          <FileIcon :icon="iconOf(entry.name, entry.kind)" />
          <span class="label">{{ entry.name }}</span>
          <span v-if="entry.kind === 'symlink'" class="arrow">→</span>
        </span>
        <span class="c mtime">{{ formatTime(entry.mtime_ms ?? null) }}</span>
        <span class="c type">{{ typeName(entry.name, entry.kind) }}</span>
        <span class="c size">{{ entry.kind === 'dir' ? '' : formatSize(entry.size) }}</span>
      </div>
    </div>

    <div class="status">
      <span>{{ rows.length }} 个项目</span>
      <span v-if="selected.length > 0">已选择 {{ selected.length }} 项{{ selectedBytes > 0 ? ` · ${formatSize(selectedBytes)}` : '' }}</span>
      <span v-if="busy" class="busy">{{ busy }}</span>
    </div>

    <a-modal
      :open="renaming.open"
      :title="`重命名${side === 'local' ? '（本机）' : '（远端）'}`"
      ok-text="确定"
      cancel-text="取消"
      @update:open="v => { renaming.open = v; if (!v) emit('editing', false) }"
      @ok="submitRename"
      @cancel="() => { renaming.open = false; emit('editing', false) }"
    >
      <a-input v-model:value="renaming.to" placeholder="新名称" @press-enter="submitRename" />
    </a-modal>

    <a-modal
      :open="removeOpen"
      title="删除"
      ok-text="删除"
      cancel-text="取消"
      :ok-button-props="{ danger: true }"
      @update:open="v => { removeOpen = v }"
      @ok="confirmRemove"
      @cancel="onRemoveCancel"
    >
      <p v-if="selectedEntries.length === 1">
        删除 <code>{{ selectedEntries[0]?.name }}</code>？{{ selectedEntries[0]?.kind === 'dir' ? '目录必须为空才能删除。' : '' }}
      </p>
      <p v-else>删除选中的 {{ selectedEntries.length }} 个条目？逐项执行，遇到失败会停下来并说明是哪一个。</p>
    </a-modal>

    <a-modal
      :open="chmodOpen"
      :title="`权限（${single?.name ?? ''}）`"
      ok-text="应用"
      cancel-text="取消"
      :ok-button-props="{ disabled: !chmodValid }"
      @update:open="v => { chmodOpen = v }"
      @ok="submitChmod"
      @cancel="() => { chmodOpen = false }"
    >
      <div class="chmod">
        <div v-for="row in chmodBits" :key="row.who" class="chmod-row">
          <span class="who">{{ row.who }}</span>
          <label><input type="checkbox" :checked="bitOn(row.shift, 4)" @change="toggleBit(row.shift, 4)">读取(r)</label>
          <label><input type="checkbox" :checked="bitOn(row.shift, 2)" @change="toggleBit(row.shift, 2)">写入(w)</label>
          <label><input type="checkbox" :checked="bitOn(row.shift, 1)" @change="toggleBit(row.shift, 1)">执行(x)</label>
        </div>
        <div class="chmod-octal">
          八进制
          <a-input v-model:value="chmodText" size="small" class="octal" @change="onChmodText" @input="onChmodText" />
          <span class="hint">{{ chmodValid ? `=${chmodText}` : '只能是 1 到 4 位八进制数字' }}</span>
        </div>
      </div>
    </a-modal>
  </section>
</template>

<style scoped>
.pane {
  display: flex;
  flex-direction: column;
  gap: 6px;
  flex: 1;
  min-width: 0;
}

.toolbar {
  display: flex;
  gap: 6px;
  flex: none;
  flex-wrap: wrap;
  align-items: center;
}

.address {
  display: flex;
  align-items: center;
  gap: 6px;
  flex: none;
}

.side {
  font-size: 11px;
  opacity: 0.65;
  flex: none;
}

.path-input {
  flex: 1;
  min-width: 0;
  font-family: ui-monospace, Menlo, Consolas, monospace;
  font-size: 12px;
}

.list {
  flex: 1;
  min-height: 0;
  overflow: auto;
  border: 1px solid var(--line);
  border-radius: 4px;
  outline: none;
}

.list:focus-visible {
  border-color: var(--line-strong);
}

.head {
  position: sticky;
  top: 0;
  z-index: 1;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 3px 8px;
  font-size: 12px;
  border-bottom: 1px solid var(--line);
  background: var(--bg-hover);
}

.h {
  all: unset;
  color: inherit;
  cursor: pointer;
  display: flex;
  align-items: center;
  gap: 2px;
  border-radius: 3px;
  padding: 1px 4px;
}

.h:hover {
  background: var(--bg-active);
}

.h i {
  font-style: normal;
  font-size: 9px;
  opacity: 0.35;
}

.h.on i {
  opacity: 1;
}

.h.name,
.c.name {
  flex: 1;
  min-width: 0;
}

.h.mtime,
.c.mtime {
  width: 116px;
  flex: none;
}

.h.type,
.c.type {
  width: 96px;
  flex: none;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.h.size,
.c.size {
  width: 72px;
  flex: none;
  text-align: right;
}

.h.size,
.h.mtime,
.h.type {
  justify-content: flex-end;
}

.row {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 2px 8px;
  font-size: 12px;
  line-height: 20px;
  cursor: default;
  user-select: none;
}

.row:hover {
  background: var(--bg-hover);
}

.row.sel {
  background: var(--accent-soft);
  box-shadow: inset 0 0 0 1px var(--accent-line);
}

/* 「...」那行走的是和条目一样的「图标 + 文字」结构，只是图标换成文件夹、
   字距拉开一点：资源管理器里上级也是这么颗图标，扫一眼就知道能进。 */
.up-row .label {
  letter-spacing: 1px;
  color: var(--text-2);
}

/* 可以接收对侧拖过来的东西：整块描一圈主色，鼠标一放开就传 */
.list.dropping {
  border-color: var(--accent-line);
  box-shadow: inset 0 0 0 2px var(--accent-soft);
}

.c {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.c.name {
  display: flex;
  align-items: center;
  gap: 6px;
}

.label {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.row.sel .label {
  font-weight: 600;
}

.arrow {
  opacity: 0.5;
}

.empty {
  padding: 18px;
  text-align: center;
  font-size: 12px;
  opacity: 0.55;
}

.status {
  display: flex;
  gap: 12px;
  font-size: 11px;
  opacity: 0.7;
  flex: none;
}

.busy {
  opacity: 1;
  color: inherit;
}

.chmod {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.chmod-row {
  display: flex;
  gap: 14px;
  align-items: center;
  font-size: 12px;
}

.chmod-row .who {
  width: 52px;
  flex: none;
}

.chmod-row label {
  display: flex;
  gap: 4px;
  align-items: center;
}

.chmod-octal {
  display: flex;
  gap: 8px;
  align-items: center;
  font-size: 12px;
}

.octal {
  width: 78px;
  font-family: ui-monospace, Menlo, Consolas, monospace;
}

.hint {
  opacity: 0.7;
}
</style>
