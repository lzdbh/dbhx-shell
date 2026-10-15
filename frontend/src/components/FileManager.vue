<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue'
import { message as antdMessage } from 'ant-design-vue'

import type { FileEntry } from '@/lib/ipc/bindings'
import { localApi } from '@/lib/ipc/client'
import { startEventBridge } from '@/lib/ipc/bus'
import { localJoin, localParent, parentPath } from '@/lib/format'
import { antdTheme } from '@/styles/antd'
import { useFilesStore } from '@/stores/files'
import { useHostsStore } from '@/stores/hosts'
import { useThemeStore } from '@/stores/theme'
import { useTransfersStore } from '@/stores/transfers'
import ContextMenu from '@/components/ContextMenu.vue'
import FilePane from '@/components/FilePane.vue'
import TransferTray from '@/components/TransferTray.vue'
import WindowControls from '@/components/WindowControls.vue'

const props = defineProps<{ hostId: string }>()

const files = useFilesStore()
const hosts = useHostsStore()
const themeStore = useThemeStore()
const transfers = useTransfersStore()

// 这个组件里 `message()` 已经是「把异常翻成中文」的那个小工具，别名导入免得撞名
const [messageApi, msgHolder] = antdMessage.useMessage()

const antTheme = computed(() => antdTheme(themeStore.dark, themeStore.colorPrimary))

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

/** 资源管理器的地址栏就是「敲一个路径过去」，本机栏要接受盘符根、UNC 之外的任意绝对路径。 */
function localGo(target: string): void {
  void loadLocal(target)
}

function localUp(): void {
  const parent = localParent(local.path)
  if (parent !== null) void loadLocal(parent)
}

async function localCreate(name: string, kind: 'dir' | 'file'): Promise<void> {
  try {
    if (kind === 'dir') await localApi.mkdir(localJoin(local.path, name))
    else await localApi.createFile(localJoin(local.path, name))
    await loadLocal(local.path)
  }
  catch (err) {
    local.error = message(err)
  }
}

async function localRemoveMany(entries: FileEntry[]): Promise<void> {
  let failure: string | null = null
  for (const entry of entries) {
    try {
      await localApi.remove(entry.path, entry.kind === 'dir')
    }
    catch (err) {
      failure = `${entry.name}：${message(err)}`
      break
    }
  }
  await loadLocal(local.path)
  // loadLocal 成功会把 error 清掉，所以这条要补在它后面
  if (failure !== null) local.error = failure
}

async function localRename(from: FileEntry, name: string): Promise<void> {
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

/** 双击：目录就进去，文件交给系统关联程序。 */
function localOpen(entry: FileEntry): void {
  if (entry.kind === 'dir') void loadLocal(entry.path)
  else void localApi.open(entry.path).catch((err: unknown) => { local.error = message(err) })
}

/* ── 远端栏 ────────────────────────────────────────────── */

const conn = ref<number | null>(null)
const remoteError = ref<string | null>(null)
const remoteSelected = ref<string[]>([])

const panel = computed(() => (conn.value === null ? undefined : files.panel(conn.value)))
const remotePath = computed(() => panel.value?.path ?? '')
const remoteEntries = computed<FileEntry[]>(() => panel.value?.entries ?? [])
const remoteLoading = computed(() => panel.value?.loading ?? false)
const remoteBusy = computed(() => panel.value?.busy ?? null)

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

async function remoteMkdir(name: string): Promise<void> {
  if (conn.value === null) return
  if (!(await files.mkdir(conn.value, name))) remoteError.value = panel.value?.error ?? '新建目录失败'
}

async function remoteCreateFile(name: string): Promise<void> {
  if (conn.value === null) return
  if (!(await files.createFile(conn.value, name))) remoteError.value = panel.value?.error ?? '新建文件失败'
}

async function remoteRename(from: FileEntry, name: string): Promise<void> {
  if (conn.value === null) return
  if (!(await files.rename(conn.value, from, name))) remoteError.value = panel.value?.error ?? '重命名失败'
}

async function remoteRemoveMany(entries: FileEntry[]): Promise<void> {
  if (conn.value === null) return
  if (!(await files.removeMany(conn.value, entries))) remoteError.value = panel.value?.error ?? '删除失败'
}

async function remoteChmod(entry: FileEntry, mode: number): Promise<void> {
  if (conn.value === null) return
  if (!(await files.chmod(conn.value, entry, mode))) remoteError.value = panel.value?.error ?? '改权限失败'
}

/** 填名字期间压住目录跟随，否则用户敲一半面板被终端拽走。 */
function remoteEditing(editing: boolean): void {
  if (conn.value !== null) void files.setEditing(conn.value, editing)
}

function remoteOpen(entry: FileEntry): void {
  if (conn.value === null) return
  if (entry.kind === 'dir') void browseRemote(entry.path)
  else void files.editRemote(conn.value, entry)
}

/* ── 两栏之间传输 ──────────────────────────────────────── */

/**
 * 传输入口只有两个：行上的右键「传到对侧」，和把行直接拖到对面那一栏。
 *
 * 原来中间那两根按钮先删了——它作用在「这一栏选中的东西」上，而两栏各自都能有选择，
 * 于是按钮的宾语其实是一直在变的：刚在右栏点了两下，左栏那根按钮就变成「上传刚才
 * 选中的那批」。右键和拖拽的宾语都在手边，不会有这种歧义。
 */

/**
 * 批次收尾后再刷那一栏。
 *
 * 上传/下载命令只回一个批次号，文件是后端异步搬的：发起瞬间就刷，列表里必然还没有，
 * 用户看到的是「传输栏写着已完成、这一栏却空空如也」。所以把刷新挂到「该连接不再有
 * 未完成批次」那一刻；小文件在挂上之前就已经结束，那种情况直接刷。
 */
const pendingRefresh = ref<(() => Promise<void>) | null>(null)

/**
 * 发起传输。
 *
 * 这里刻意不 `await` 传输本身：批次可能搬几百兆，把界面挂在上面等于禁用整个窗口。
 * 「什么时候刷新」交给 `runningBatches` 归零那一刻，见 onMounted 里那个订阅。
 */
async function uploadPaths(paths: string[]): Promise<void> {
  if (conn.value === null || paths.length === 0) return
  const result = await files.upload(conn.value, [...paths])
  if (!result.ok) remoteError.value = result.error ?? '上传失败'
  else {
    messageApi.success(`已交给上传：${paths.length} 个 → ${remotePath.value || '~'}`)
    await refreshWhenBatchEnds(() => browseRemote(null))
  }
}

async function downloadPaths(paths: string[]): Promise<void> {
  if (conn.value === null || paths.length === 0) return
  const result = await files.download(conn.value, [...paths], local.path)
  if (!result.ok) remoteError.value = result.error ?? '下载失败'
  else {
    messageApi.success(`已交给下载：${paths.length} 个 → ${local.path}`)
    await refreshWhenBatchEnds(() => loadLocal(local.path))
  }
}

/** 本机栏交出来的东西一律往远端送，远端栏反过来。 */
function transferFromLocal(entries: FileEntry[]): void {
  void uploadPaths(entries.map(e => e.path))
}

function transferFromRemote(entries: FileEntry[]): void {
  void downloadPaths(entries.map(e => e.path))
}

async function refreshWhenBatchEnds(job: () => Promise<void>): Promise<void> {
  if (transfers.runningBatches.length === 0) await job()
  else pendingRefresh.value = job
}

function message(err: unknown): string {
  return err instanceof Error ? err.message : String(err)
}

let stopBridge: (() => void) | null = null

/**
 * 两个栏的刷新只挂一个订阅。
 *
 * 上传与下载都只回批次号，收尾时刻由 `runningBatches` 归零来说；分成两个 watcher
 * 会在同一瞬间刷两次，所以谁发起的传输由 `pendingRefresh` 那一个槽位记住。
 */
let stopWatch: (() => void) | null = null

onMounted(async () => {
  stopBridge = await startEventBridge()
  stopWatch = watch(() => transfers.runningBatches.length, (remaining) => {
    if (remaining > 0) return
    const job = pendingRefresh.value
    pendingRefresh.value = null
    void job?.()
  })
  await Promise.all([hosts.refresh(), void transfers.refresh()])
  home.value = await localApi.home().catch(() => '')
  drives.value = await localApi.drives().catch(() => [])
  await loadLocal(home.value || null)
  await openRemote()
})

onBeforeUnmount(() => {
  stopWatch?.()
  stopBridge?.()
  // close 里会把这台主机的编辑往返一起收掉：窗口没了还留一个轮询在改远端文件是不能接受的
  if (conn.value !== null) void files.close(conn.value)
})
</script>

<template>
  <a-config-provider :theme="antTheme">
    <!-- 根必须是 a-layout：它带来主题的背景色与文字色。用普通 div 的话页面底色就是
         WebView2 默认白，而 AntD 深色主题里「禁用态」用的是 rgba(255,255,255,.08) 这种
         半透明填充——压在白底上等于看不见，禁用按钮看着就是两块空白。 -->
    <a-layout class="file-manager">
      <!-- 这条工具栏顶替了原生标题栏：整条是拖拽区（deep：子元素里也能拖），
           窗控按钮是 clickable 标签，drag.js 会自动豁免 -->
      <div class="bar" data-tauri-drag-region="deep">
        <span class="title">{{ host ? host.label : '文件管理' }}</span>
        <span class="subtitle">{{ subtitle }}</span>
        <!-- 和标签栏同一个点：a-tag 的 green 是组件库预设色，跟 --ok 不是一套，
             同一件事在两个窗口里会给出两种绿 -->
        <span class="status"><span class="dot" :class="conn !== null ? 'ok' : 'down'" />{{ conn !== null ? '已连接' : '未连接' }}</span>
        <span class="spacer" />
        <WindowControls />
      </div>

      <div class="panes">
        <FilePane
          side="local"
          :path="local.path"
          :entries="local.entries"
          :loading="local.loading"
          :error="local.error"
          :selected="local.selected"
          :enabled="true"
          :can-go-up="localParent(local.path) !== null"
          :can-transfer="conn !== null"
          :busy="null"
          @update:selected="v => { local.selected = v }"
          @go="localGo"
          @up="localUp"
          @refresh="() => loadLocal(local.path)"
          @mkdir="name => void localCreate(name, 'dir')"
          @create-file="name => void localCreate(name, 'file')"
          @remove="entries => void localRemoveMany(entries)"
          @rename="(from, name) => void localRename(from, name)"
          @open="localOpen"
          @transfer="transferFromLocal"
          @drop="paths => void downloadPaths(paths)"
          @dismiss="local.error = null"
        >
          <template #tools>
            <a-select
              size="small"
              style="width: 92px"
              :value="drives.includes(local.path) ? local.path : undefined"
              placeholder="盘符"
              @change="v => void loadLocal(String(v))"
            >
              <a-select-option v-for="d in drives" :key="d" :value="d">{{ d }}</a-select-option>
            </a-select>
            <a-button size="small" @click="void loadLocal(home || null)">主目录</a-button>
          </template>
        </FilePane>

        <FilePane
          side="remote"
          :path="remotePath"
          :entries="remoteEntries"
          :loading="remoteLoading"
          :error="remoteError"
          :selected="remoteSelected"
          :enabled="conn !== null"
          :can-go-up="remotePath !== '' && remotePath !== '/'"
          :can-transfer="conn !== null"
          :busy="remoteBusy"
          @update:selected="v => { remoteSelected = v }"
          @go="target => void browseRemote(target)"
          @up="() => void browseRemote(parentPath(remotePath))"
          @refresh="() => browseRemote(null)"
          @mkdir="name => void remoteMkdir(name)"
          @create-file="name => void remoteCreateFile(name)"
          @remove="entries => void remoteRemoveMany(entries)"
          @rename="(from, name) => void remoteRename(from, name)"
          @chmod="(entry, mode) => void remoteChmod(entry, mode)"
          @open="remoteOpen"
          @transfer="transferFromRemote"
          @drop="paths => void uploadPaths(paths)"
          @editing="remoteEditing"
          @dismiss="remoteError = null"
        />
      </div>

      <TransferTray />
      <!-- 右键菜单的浮层：文件窗口是另一个 webview，`App.vue` 里那个实例在这儿不存在，
           所以这一窗自己挂一个。 -->
      <ContextMenu />
      <msgHolder />
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

/* 顶条即标题栏（窗口去边框了），高度向主窗口看齐；右边不留内边距，
   窗控按钮通到窗口右上角。 */
.bar {
  display: flex;
  align-items: center;
  gap: 10px;
  height: 44px;
  padding: 0 0 0 14px;
  background: var(--bg-surface);
  border-bottom: 1px solid var(--line);
  flex: none;
}

.spacer {
  flex: 1;
}

.title {
  font-weight: 600;
}

.subtitle {
  font-size: 12px;
  color: var(--text-3);
  font-family: ui-monospace, Menlo, Consolas, monospace;
  /* 地址里的数字要对齐，扫一眼就能比较两台机器 */
  font-variant-numeric: tabular-nums;
}

/* 连接状态点：尺寸与配色和标签栏那套一致（.dot 在两个组件里各写一份，
   scoped 样式不共享，但值都取自同一批变量） */
.status {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  font-size: 12px;
  color: var(--text-2);
}

.dot {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  flex: none;
}

.dot.ok {
  background: var(--ok);
}

.dot.down {
  background: var(--warn);
}

.panes {
  display: flex;
  flex: 1;
  min-height: 0;
  gap: 8px;
  padding: 8px;
}
</style>
