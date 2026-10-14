<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue'

import { theme } from 'ant-design-vue'

import type { FileEntry } from '@/lib/ipc/bindings'
import { localApi } from '@/lib/ipc/client'
import { startEventBridge } from '@/lib/ipc/bus'
import { formatSize, formatTime, localJoin, localParent, parentPath } from '@/lib/format'
import { useFilesStore } from '@/stores/files'
import { useHostsStore } from '@/stores/hosts'
import { useThemeStore } from '@/stores/theme'
import { useTransfersStore } from '@/stores/transfers'
import TransferTray from '@/components/TransferTray.vue'

const props = defineProps<{ hostId: string }>()

const files = useFilesStore()
const hosts = useHostsStore()
const themeStore = useThemeStore()
const transfers = useTransfersStore()

const antTheme = computed(() => ({
  algorithm: themeStore.dark ? theme.darkAlgorithm : theme.defaultAlgorithm,
  token: { colorPrimary: themeStore.colorPrimary },
}))

const host = computed(() => hosts.byId(props.hostId) ?? null)
/** vault 记录里的端口是「缺省即按协议补」，直接插值会显示成 `:null`。 */
const subtitle = computed(() => {
  const record = host.value
  if (!record) return props.hostId
  const port = record.endpoint.port ?? (record.protocol === 'telnet' ? 23 : 22)
  return `${record.endpoint.username}@${record.endpoint.host}:${port}`
})

/* ── 本机栏 ────────────────────────────────────────────── */

const local = reactive({
  path: '',
  entries: [] as FileEntry[],
  loading: false,
  error: null as string | null,
  selected: [] as string[],
})
const drives = ref<string[]>([])
const home = ref('')

async function loadLocal(path: string | null): Promise<void> {
  local.loading = true
  try {
    const listing = await localApi.list(path)
    local.path = listing.path
    local.entries = listing.entries
    local.error = null
    local.selected = []
  }
  catch (err) {
    local.error = message(err)
  }
  finally {
    local.loading = false
  }
}

async function localUp(): Promise<void> {
  const parent = localParent(local.path)
  if (parent !== null) await loadLocal(parent)
}

function localEnter(entry: FileEntry): void {
  if (entry.kind === 'dir') void loadLocal(entry.path)
}

const localMkdirOpen = ref(false)
const localMkdirName = ref('')

async function submitLocalMkdir(): Promise<void> {
  const name = localMkdirName.value.trim()
  localMkdirOpen.value = false
  localMkdirName.value = ''
  if (!name) return
  try {
    await localApi.mkdir(localJoin(local.path, name))
    await loadLocal(local.path)
  }
  catch (err) {
    local.error = message(err)
  }
}

async function localRemove(raw: unknown): Promise<void> {
  const entry = raw as FileEntry
  try {
    await localApi.remove(entry.path, entry.kind === 'dir')
    await loadLocal(local.path)
  }
  catch (err) {
    local.error = message(err)
  }
}

const localRename = reactive({ open: false, from: null as FileEntry | null, to: '' })

function startLocalRename(raw: unknown): void {
  const entry = raw as FileEntry
  localRename.from = entry
  localRename.to = entry.name
  localRename.open = true
}

async function submitLocalRename(): Promise<void> {
  const from = localRename.from
  const name = localRename.to.trim()
  localRename.open = false
  localRename.from = null
  if (!from || !name) return
  const parent = localParent(from.path)
  if (parent === null) return
  try {
    await localApi.rename(from.path, localJoin(parent, name))
    await loadLocal(local.path)
  }
  catch (err) {
    local.error = message(err)
  }
}

/* ── 远端栏 ────────────────────────────────────────────── */

const conn = ref<number | null>(null)
const remoteError = ref<string | null>(null)
const remoteSelected = ref<string[]>([])

const panel = computed(() => (conn.value === null ? undefined : files.panel(conn.value)))
const remoteLoading = computed(() => panel.value?.loading ?? false)
const remotePath = computed(() => panel.value?.path ?? '')
const remoteEntries = computed<FileEntry[]>(() => panel.value?.entries ?? [])

async function openRemote(): Promise<void> {
  const opened = await files.openForHost(props.hostId)
  if (opened) conn.value = opened.connection
  else remoteError.value = files.openError
}

async function browseRemote(target: string | null): Promise<void> {
  if (conn.value === null) return
  await files.browse(conn.value, target)
  remoteSelected.value = []
}

async function remoteUp(): Promise<void> {
  if (remotePath.value === '/') return
  await browseRemote(parentPath(remotePath.value))
}

function remoteEnter(entry: FileEntry): void {
  if (entry.kind === 'dir') void browseRemote(entry.path)
}

const remoteMkdirOpen = ref(false)
const remoteMkdirName = ref('')

async function submitRemoteMkdir(): Promise<void> {
  const name = remoteMkdirName.value.trim()
  remoteMkdirOpen.value = false
  remoteMkdirName.value = ''
  if (!name || conn.value === null) return
  if (!(await files.mkdir(conn.value, name))) remoteError.value = panel.value?.error ?? '新建目录失败'
}

async function remoteRemove(raw: unknown): Promise<void> {
  if (conn.value === null) return
  const entry = raw as FileEntry
  if (!(await files.remove(conn.value, entry))) remoteError.value = panel.value?.error ?? '删除失败'
}

const remoteRename = reactive({ open: false, from: null as FileEntry | null, to: '' })

function startRemoteRename(raw: unknown): void {
  const entry = raw as FileEntry
  remoteRename.from = entry
  remoteRename.to = entry.name
  remoteRename.open = true
}

async function submitRemoteRename(): Promise<void> {
  const from = remoteRename.from
  const name = remoteRename.to.trim()
  remoteRename.open = false
  remoteRename.from = null
  if (!from || !name || conn.value === null) return
  if (!(await files.rename(conn.value, from, name))) remoteError.value = panel.value?.error ?? '重命名失败'
}

/* ── 两栏之间传输 ──────────────────────────────────────── */

const canUpload = computed(() => conn.value !== null && local.selected.length > 0)
const canDownload = computed(() => conn.value !== null && remoteSelected.value.length > 0)

/**
 * 批次收尾后再刷那一栏。
 *
 * 上传/下载命令只回一个批次号，文件是后端异步搬的：发起瞬间就刷，列表里必然还没有，
 * 用户看到的是「传输栏写着已完成、这一栏却空空如也」。所以把刷新挂到「该连接不再有
 * 未完成批次」那一刻；小文件在挂上之前就已经结束，那种情况直接刷。
 */
const pendingRefresh = ref<(() => Promise<void>) | null>(null)

watch(() => transfers.runningBatches.length, async (remaining) => {
  if (remaining > 0) return
  const job = pendingRefresh.value
  pendingRefresh.value = null
  await job?.()
})

async function refreshWhenBatchEnds(job: () => Promise<void>): Promise<void> {
  if (transfers.runningBatches.length === 0) await job()
  else pendingRefresh.value = job
}

async function uploadSelected(): Promise<void> {
  if (conn.value === null || local.selected.length === 0) return
  const result = await files.upload(conn.value, [...local.selected])
  if (!result.ok) remoteError.value = result.error ?? '上传失败'
  await refreshWhenBatchEnds(() => browseRemote(null))
}

async function downloadSelected(): Promise<void> {
  if (conn.value === null || remoteSelected.value.length === 0) return
  const result = await files.download(conn.value, [...remoteSelected.value], local.path)
  if (!result.ok) remoteError.value = result.error ?? '下载失败'
  else await refreshWhenBatchEnds(() => loadLocal(local.path))
}

/* ── 表格列与选择 ──────────────────────────────────────── */

const columns = [
  { title: '名称', dataIndex: 'name', key: 'name', ellipsis: true },
  { title: '大小', dataIndex: 'size', key: 'size', width: 84, align: 'right' as const },
  { title: '修改时间', dataIndex: 'mtime_ms', key: 'mtime', width: 120 },
  { title: '', key: 'actions', width: 108 },
]

const localSelection = computed(() => ({
  selectedRowKeys: local.selected,
  onChange: (keys: (string | number)[]) => { local.selected = keys.map(String) },
}))
const remoteSelection = computed(() => ({
  selectedRowKeys: remoteSelected.value,
  onChange: (keys: (string | number)[]) => { remoteSelected.value = keys.map(String) },
}))

function message(err: unknown): string {
  return err instanceof Error ? err.message : String(err)
}

let stopBridge: (() => void) | null = null

onMounted(async () => {
  stopBridge = await startEventBridge()
  await Promise.all([hosts.refresh(), void transfers.refresh()])
  home.value = await localApi.home().catch(() => '')
  drives.value = await localApi.drives().catch(() => [])
  await loadLocal(home.value || null)
  await openRemote()
})

onBeforeUnmount(() => {
  stopBridge?.()
  if (conn.value !== null) void files.close(conn.value)
})
</script>

<template>
  <a-config-provider :theme="antTheme">
    <!-- 根必须是 a-layout：它带来主题的背景色与文字色。用普通 div 的话页面底色就是
         WebView2 默认白，而 AntD 深色主题里「禁用态」用的是 rgba(255,255,255,.08) 这种
         半透明填充——压在白底上等于看不见，禁用按钮看着就是两块空白。 -->
    <a-layout class="file-manager">
      <div class="bar">
        <span class="title">{{ host ? host.label : '文件管理' }}</span>
        <span class="subtitle">{{ subtitle }}</span>
        <a-tag v-if="conn !== null" color="green">已连接</a-tag>
        <a-tag v-else color="orange">未连接</a-tag>
      </div>

      <div class="panes">
        <!-- 本机栏 -->
        <section class="pane">
          <div class="toolbar">
            <a-select
              size="small"
              style="width: 96px"
              :value="drives.includes(local.path) ? local.path : undefined"
              placeholder="盘符"
              @change="v => void loadLocal(String(v))"
            >
              <a-select-option v-for="d in drives" :key="d" :value="d">{{ d }}</a-select-option>
            </a-select>
            <a-button size="small" @click="void loadLocal(home || null)">主目录</a-button>
            <a-button size="small" :disabled="localParent(local.path) === null" @click="void localUp()">上级</a-button>
            <a-button size="small" @click="void loadLocal(local.path)">刷新</a-button>
            <a-button size="small" @click="localMkdirOpen = true">新建目录</a-button>
          </div>
          <a-input
            v-if="localMkdirOpen"
            v-model:value="localMkdirName"
            size="small"
            placeholder="新目录名，回车创建"
            autofocus
            @press-enter="void submitLocalMkdir()"
            @blur="localMkdirOpen = false"
          />
          <div class="path" :title="local.path">本机 · {{ local.path }}</div>
          <a-alert v-if="local.error" :message="local.error" type="error" show-icon closable @close="local.error = null" />
          <a-table
            class="grid"
            :columns="columns"
            :data-source="local.entries"
            :loading="local.loading"
            :pagination="false"
            :row-selection="localSelection"
            row-key="path"
            size="small"
            :custom-row="(record: FileEntry) => ({ onDblclick: () => localEnter(record) })"
          >
            <template #bodyCell="{ column, record }">
              <template v-if="column.key === 'name'">
                <span :class="{ dir: record.kind === 'dir' }">{{ record.name }}</span>
              </template>
              <template v-else-if="column.key === 'size'">
                <span v-if="record.kind === 'dir'">-</span>
                <span v-else>{{ formatSize(record.size) }}</span>
              </template>
              <template v-else-if="column.key === 'mtime'">{{ formatTime(record.mtime_ms) }}</template>
              <template v-else-if="column.key === 'actions'">
                <a-space :size="2">
                  <a-button size="small" type="text" @click.stop="startLocalRename(record)">改名</a-button>
                  <a-popconfirm title="删除这个条目？" ok-text="删除" cancel-text="取消" @confirm="void localRemove(record)">
                    <a-button size="small" type="text" danger @click.stop>删除</a-button>
                  </a-popconfirm>
                </a-space>
              </template>
            </template>
            <template #emptyText><a-empty :image-style="{ height: '40px' }" description="空目录" /></template>
          </a-table>
        </section>

        <!-- 传输按钮 -->
        <div class="transfer">
          <a-tooltip title="把左栏选中的文件上传到右栏当前目录">
            <a-button size="small" type="primary" :disabled="!canUpload" @click="void uploadSelected()">
              {{ local.selected.length > 0 ? `上传 ${local.selected.length} 个 →` : '上传 →' }}
            </a-button>
          </a-tooltip>
          <a-tooltip title="把右栏选中的文件下载到左栏当前目录">
            <a-button size="small" type="primary" :disabled="!canDownload" @click="void downloadSelected()">
              {{ remoteSelected.length > 0 ? `← 下载 ${remoteSelected.length} 个` : '← 下载' }}
            </a-button>
          </a-tooltip>
        </div>

        <!-- 远端栏 -->
        <section class="pane">
          <div class="toolbar">
            <a-button size="small" :disabled="remotePath === '/'" @click="void remoteUp()">上级</a-button>
            <a-button size="small" @click="void browseRemote(null)">刷新</a-button>
            <a-button size="small" :disabled="conn === null" @click="remoteMkdirOpen = true">新建目录</a-button>
          </div>
          <a-input
            v-if="remoteMkdirOpen"
            v-model:value="remoteMkdirName"
            size="small"
            placeholder="新目录名，回车创建"
            @press-enter="void submitRemoteMkdir()"
            @blur="remoteMkdirOpen = false"
          />
          <div class="path" :title="remotePath">远端 · {{ remotePath }}</div>
          <a-alert v-if="remoteError" :message="remoteError" type="error" show-icon closable @close="remoteError = null" />
          <a-table
            class="grid"
            :columns="columns"
            :data-source="remoteEntries"
            :loading="remoteLoading"
            :pagination="false"
            :row-selection="remoteSelection"
            row-key="path"
            size="small"
            :custom-row="(record: FileEntry) => ({ onDblclick: () => remoteEnter(record) })"
          >
            <template #bodyCell="{ column, record }">
              <template v-if="column.key === 'name'">
                <span :class="{ dir: record.kind === 'dir' }">{{ record.name }}</span>
                <a-tag v-if="record.kind === 'symlink'" color="blue">链接</a-tag>
              </template>
              <template v-else-if="column.key === 'size'">
                <span v-if="record.kind === 'dir'">-</span>
                <span v-else>{{ formatSize(record.size) }}</span>
              </template>
              <template v-else-if="column.key === 'mtime'">{{ formatTime(record.mtime_ms) }}</template>
              <template v-else-if="column.key === 'actions'">
                <a-space :size="2">
                  <a-button size="small" type="text" @click.stop="startRemoteRename(record)">改名</a-button>
                  <a-popconfirm title="删除这个条目？" ok-text="删除" cancel-text="取消" @confirm="void remoteRemove(record)">
                    <a-button size="small" type="text" danger @click.stop>删除</a-button>
                  </a-popconfirm>
                </a-space>
              </template>
            </template>
            <template #emptyText><a-empty :image-style="{ height: '40px' }" description="空目录" /></template>
          </a-table>
        </section>
      </div>

      <TransferTray />

      <a-modal :open="localRename.open" title="重命名（本机）" ok-text="确定" cancel-text="取消"
        @update:open="localRename.open = false" @ok="void submitLocalRename()" @cancel="localRename.open = false">
        <a-input v-model:value="localRename.to" placeholder="新名称" @press-enter="void submitLocalRename()" />
      </a-modal>
      <a-modal :open="remoteRename.open" title="重命名（远端）" ok-text="确定" cancel-text="取消"
        @update:open="remoteRename.open = false" @ok="void submitRemoteRename()" @cancel="remoteRename.open = false">
        <a-input v-model:value="remoteRename.to" placeholder="新名称" @press-enter="void submitRemoteRename()" />
      </a-modal>
    </a-layout>
  </a-config-provider>
</template>

<style scoped>
.file-manager {
  display: flex;
  flex-direction: column;
  height: 100vh;
  overflow: hidden;
}

.bar {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 8px 12px;
  border-bottom: 1px solid rgba(128, 128, 128, 0.2);
  flex: none;
}

.title {
  font-weight: 600;
}

.subtitle {
  font-size: 12px;
  opacity: 0.7;
  font-family: ui-monospace, Menlo, Consolas, monospace;
}

.panes {
  display: flex;
  flex: 1;
  min-height: 0;
  gap: 8px;
  padding: 8px;
}

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
}

.path {
  font-size: 12px;
  font-family: ui-monospace, Menlo, Consolas, monospace;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  opacity: 0.85;
}

.grid {
  flex: 1;
  min-height: 0;
  overflow: auto;
}

.dir {
  font-weight: 600;
}

.transfer {
  display: flex;
  flex-direction: column;
  justify-content: center;
  gap: 8px;
  flex: none;
}
</style>
