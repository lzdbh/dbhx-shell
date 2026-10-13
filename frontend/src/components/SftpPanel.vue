<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { message } from 'ant-design-vue'

import type { FileEntry } from '@/lib/ipc/bindings'
import { HOLD_TEXT, useFilesStore } from '@/stores/files'
import { useSessionsStore } from '@/stores/sessions'
import { useTransfersStore } from '@/stores/transfers'
import { formatSize, formatTime, parentPath } from '@/lib/format'

const files = useFilesStore()
const sessions = useSessionsStore()
const transfers = useTransfersStore()
const [messageApi, holder] = message.useMessage()

const panel = computed(() => files.panelForSession(sessions.activeId))
const conn = computed(() => panel.value?.connection ?? null)

const columns = [
  { title: '名称', dataIndex: 'name', key: 'name', ellipsis: true },
  { title: '大小', dataIndex: 'size', key: 'size', width: 88, align: 'right' as const },
  { title: '修改时间', dataIndex: 'mtime_ms', key: 'mtime', width: 128 },
  { title: '', key: 'actions', width: 120 },
]

const mkdirOpen = ref(false)
const mkdirName = ref('')
const renameTarget = ref<FileEntry | null>(null)
const renameTo = ref('')

let stopDragDrop: (() => void) | null = null

/**
 * a-table 的插槽把行交回来时类型是 `Record<string, any>`，而模板里不能写 `as` 断言，
 * 所以所有行回调都在这一层收 unknown、进来先收窄成 FileEntry。
 */
function toEntry(raw: unknown): FileEntry {
  return raw as FileEntry
}

/** 目录可以「上一级」，文件不行；符号链接按服务端给的 kind 判断，不猜它指向什么。 */
function enterable(raw: unknown): boolean {
  return toEntry(raw).kind === 'dir'
}

async function onRowClick(raw: unknown, double: boolean): Promise<void> {
  if (conn.value === null) return
  const entry = toEntry(raw)
  if (double && entry.kind === 'dir') await files.browse(conn.value, entry.name)
}

async function refresh(): Promise<void> {
  if (conn.value !== null) await files.browse(conn.value, null)
}

async function up(): Promise<void> {
  const p = panel.value
  if (!p || p.path === '/') return
  await files.browse(p.connection, parentPath(p.path))
}

/** 输入框打开期间通知后端别跳目录，否则用户正在改名的行会突然换页。 */
async function beginEdit(): Promise<void> {
  if (conn.value !== null) await files.setEditing(conn.value, true)
}

async function endEdit(): Promise<void> {
  if (conn.value !== null) await files.setEditing(conn.value, false)
}

async function submitMkdir(): Promise<void> {
  const p = panel.value
  if (!p || !mkdirName.value.trim()) return
  const ok = await files.mkdir(p.connection, mkdirName.value.trim())
  mkdirName.value = ''
  mkdirOpen.value = false
  await endEdit()
  if (!ok) messageApi.error(p.error ?? '新建目录失败')
}

function startRename(raw: unknown): void {
  renameTarget.value = toEntry(raw)
  renameTo.value = renameTarget.value.name
  void beginEdit()
}

async function submitRename(): Promise<void> {
  const p = panel.value
  const target = renameTarget.value
  if (!p || !target || !renameTo.value.trim()) return
  const ok = await files.rename(p.connection, target, renameTo.value.trim())
  renameTarget.value = null
  await endEdit()
  if (!ok) messageApi.error(p.error ?? '重命名失败')
}

async function removeEntry(raw: unknown): Promise<void> {
  const p = panel.value
  if (!p) return
  if (!(await files.remove(p.connection, toEntry(raw)))) messageApi.error(p.error ?? '删除失败')
}

async function downloadEntry(raw: unknown): Promise<void> {
  const p = panel.value
  if (!p) return
  const result = await files.download(p.connection, [toEntry(raw).path])
  if (!result.ok) messageApi.warning(result.error ?? '下载失败')
}

async function cancelBatch(): Promise<void> {
  if (conn.value === null) return
  if (!(await transfers.cancelFor(conn.value))) messageApi.info('这批传输已经结束了')
}

async function closePanel(): Promise<void> {
  if (conn.value !== null) await files.close(conn.value)
}

onMounted(async () => {
  // Tauri 的拖放给的是本机绝对路径；浏览器里跑 vite 时这个 API 不存在，忽略即可
  try {
    const { getCurrentWindow } = await import('@tauri-apps/api/window')
    const unlisten = await getCurrentWindow().onDragDropEvent((event) => {
      if (event.payload.type !== 'drop') return
      const p = panel.value
      if (!p) return
      void files.upload(p.connection, event.payload.paths).then(result => {
        if (!result.ok) messageApi.error(result.error ?? '上传失败')
      })
    })
    stopDragDrop = unlisten
  }
  catch {
    // 没有拖拽就用不了上传，面板其余功能不受影响
  }
})

onBeforeUnmount(() => stopDragDrop?.())
</script>

<template>
  <div class="sftp-panel">
    <holder />
    <template v-if="panel">
      <div class="head">
        <a-button size="small" :disabled="panel.path === '/'" @click="void up()">上级</a-button>
        <a-button size="small" @click="void refresh()">刷新</a-button>
        <a-button size="small" @click="void closePanel()">关闭</a-button>
      </div>
      <div class="path" :title="panel.path">{{ panel.path }}</div>
      <div class="follow">
        <a-switch
          size="small"
          :checked="panel.following"
          @change="value => void files.toggleFollow(panel!.connection, Boolean(value))"
        />
        <span class="follow-label">跟随终端目录</span>
        <a-tooltip v-if="panel.hold" :title="HOLD_TEXT[panel.hold]">
          <a-tag color="orange">{{ HOLD_TEXT[panel.hold] }}</a-tag>
        </a-tooltip>
      </div>
      <a-alert v-if="panel.error" :message="panel.error" type="error" show-icon closable />
      <div v-if="transfers.batchFor(panel.connection) !== null" class="batch">
        <span>传输中 {{ transfers.active.length }} 项</span>
        <a-button size="small" danger @click="void cancelBatch()">取消</a-button>
      </div>
      <div class="row-actions">
        <a-button size="small" @click="mkdirOpen = !mkdirOpen; void beginEdit()">
          新建目录
        </a-button>
        <a-input
          v-if="mkdirOpen"
          v-model:value="mkdirName"
          size="small"
          placeholder="目录名"
          style="width: 140px"
          @press-enter="void submitMkdir()"
          @blur="mkdirOpen = false; void endEdit()"
        />
        <span class="hint">把文件拖进窗口即可上传到当前目录</span>
      </div>
      <a-table
        :columns="columns"
        :data-source="panel.entries"
        :loading="panel.loading"
        :pagination="false"
        row-key="path"
        size="small"
        :custom-row="(record: FileEntry) => ({ onClick: () => void onRowClick(record, false), onDblclick: () => void onRowClick(record, true) })"
      >
        <template #bodyCell="{ column, record }">
          <template v-if="column.key === 'name'">
            <span :class="{ dir: enterable(record) }">{{ record.name }}</span>
            <a-tag v-if="record.kind === 'symlink'" color="blue">链接</a-tag>
          </template>
          <template v-else-if="column.key === 'size'">
            <span v-if="record.kind === 'dir'">-</span>
            <span v-else>{{ formatSize(record.size) }}</span>
          </template>
          <template v-else-if="column.key === 'mtime'">
            {{ formatTime(record.mtime_ms) }}
          </template>
          <template v-else-if="column.key === 'actions'">
            <a-space :size="2">
              <a-button v-if="record.kind === 'file'" size="small" type="text" @click.stop="void downloadEntry(record)">下载</a-button>
              <a-button size="small" type="text" @click.stop="startRename(record)">改名</a-button>
              <a-popconfirm title="删除这个条目？" ok-text="删除" cancel-text="取消" @confirm="void removeEntry(record)">
                <a-button size="small" type="text" danger @click.stop>删除</a-button>
              </a-popconfirm>
            </a-space>
          </template>
        </template>
        <template #emptyText>
          <a-empty description="空目录" />
        </template>
      </a-table>
      <a-modal
        :open="renameTarget !== null"
        title="重命名"
        ok-text="确定"
        cancel-text="取消"
        @update:open="renameTarget = null"
        @ok="void submitRename()"
        @cancel="renameTarget = null; void endEdit()"
      >
        <a-input v-model:value="renameTo" placeholder="新名称" @press-enter="void submitRename()" />
      </a-modal>
    </template>
    <a-empty v-else description="选中一个 SSH 会话后点『文件面板』，会复用那条登录" style="margin-top: 48px" />
  </div>
</template>

<style scoped>
.sftp-panel {
  display: flex;
  flex-direction: column;
  gap: 6px;
  height: 100%;
  padding: 8px;
  overflow: hidden;
}

.head,
.follow,
.batch,
.row-actions {
  display: flex;
  align-items: center;
  gap: 6px;
  flex: none;
}

.path {
  font-size: 12px;
  font-family: ui-monospace, Menlo, Consolas, monospace;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  direction: rtl;
  text-align: left;
  opacity: 0.85;
}

.follow-label {
  font-size: 12px;
}

.hint {
  font-size: 11px;
  opacity: 0.6;
}

.dir {
  font-weight: 600;
}

.batch {
  justify-content: space-between;
  font-size: 12px;
}
</style>
