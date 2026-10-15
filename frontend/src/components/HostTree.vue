<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { message } from 'ant-design-vue'

import { DeleteOutlined, EditOutlined, FolderOpenOutlined, LinkOutlined, PlusOutlined } from '@ant-design/icons-vue'

import type { HostInfo } from '@/lib/ipc/bindings'
import { fileWindowApi } from '@/lib/ipc/client'
import { useHostsStore } from '@/stores/hosts'
import { useSessionsStore } from '@/stores/sessions'
import HostFormModal from './HostFormModal.vue'

const hosts = useHostsStore()
const sessions = useSessionsStore()
const [messageApi, holder] = message.useMessage()

const keyword = ref('')
const selectedKey = ref<string | null>(null)
const formOpen = ref(false)
const editing = ref<HostInfo | null>(null)
const connecting = ref(false)

const HOST_PREFIX = 'h:'

const filtered = computed(() => {
  const text = keyword.value.trim().toLowerCase()
  if (!text) return hosts.hosts
  return hosts.hosts.filter(h =>
    [h.label, h.group ?? '', h.endpoint.host, h.endpoint.username].join(' ').toLowerCase().includes(text),
  )
})

const treeData = computed(() =>
  hosts.groupsOf(filtered.value).map(group => ({
    title: `${group.name}（${group.hosts.length}）`,
    key: `g:${group.name}`,
    selectable: false,
    children: group.hosts.map(host => ({
      // 标题里带上 user@host：同名主机在树里撞车时，只有地址能区分它们
      title: `${host.label} · ${hosts.subtitle(host)}`,
      key: `${HOST_PREFIX}${host.id}`,
    })),
  })),
)

const selected = computed<HostInfo | null>(() =>
  selectedKey.value?.startsWith(HOST_PREFIX) ? hosts.byId(selectedKey.value.slice(HOST_PREFIX.length)) ?? null : null,
)

async function connect(host: HostInfo): Promise<void> {
  connecting.value = true
  const result = await sessions.openRemote(host.id)
  connecting.value = false
  if (!result.ok) messageApi.error(result.error ?? '连接失败')
  else messageApi.success(`正在连接 ${host.label}`)
}

async function openPanel(host: HostInfo): Promise<void> {
  // 文件管理是独立窗口：这里只让 Rust 把窗口弹/聚起来，远端栏由那个窗口自己按
  // label 里的 hostId 去开。主窗口不再嵌面板。
  try {
    await fileWindowApi.open(host.id)
  }
  catch (err) {
    messageApi.error(err instanceof Error ? err.message : '打开文件管理窗口失败')
  }
}

async function remove(host: HostInfo): Promise<void> {
  if (await hosts.remove(host.id)) {
    if (selectedKey.value === `${HOST_PREFIX}${host.id}`) selectedKey.value = null
    messageApi.success('已删除')
  }
  else messageApi.error(hosts.error ?? '删除失败')
}

function edit(host: HostInfo): void {
  editing.value = host
  formOpen.value = true
}

function create(): void {
  editing.value = null
  formOpen.value = true
}

onMounted(() => void hosts.refresh())
</script>

<template>
  <div class="host-tree">
    <holder />
    <div class="toolbar">
      <a-input v-model:value="keyword" size="small" placeholder="搜索主机" allow-clear />
      <a-button size="small" type="primary" @click="create()">
        <template #icon>
          <PlusOutlined />
        </template>
        新建
      </a-button>
    </div>
    <a-alert v-if="hosts.error" :message="hosts.error" type="error" banner />
    <a-spin :spinning="hosts.loading">
      <!-- 空状态只给一句话：组件库那台插画在侧栏 260px 里又大又抢，而且「还没有主机」
           这件事上面那颗「新建」按钮已经说清楚了 -->
      <div v-if="filtered.length === 0" class="empty">
        {{ keyword ? '没有匹配的主机' : '还没有主机，先新建一台' }}
      </div>
      <a-tree
        v-else
        :tree-data="treeData"
        :selected-keys="selectedKey ? [selectedKey] : []"
        default-expand-all
        block-node
        @select="keys => selectedKey = (keys[0] ?? null) as string | null"
      />
    </a-spin>
    <div v-if="selected" class="actions">
      <a-button size="small" type="primary" :loading="connecting" @click="void connect(selected)">
        <template #icon>
          <LinkOutlined />
        </template>
        连接
      </a-button>
      <a-button size="small" :disabled="selected.protocol !== 'ssh'" @click="void openPanel(selected)">
        <template #icon>
          <FolderOpenOutlined />
        </template>
        文件面板
      </a-button>
      <a-button size="small" @click="edit(selected)">
        <template #icon>
          <EditOutlined />
        </template>
        编辑
      </a-button>
      <a-popconfirm title="删除这台主机及其已存口令？" ok-text="删除" cancel-text="取消" @confirm="void remove(selected)">
        <a-button size="small" danger>
          <template #icon>
            <DeleteOutlined />
          </template>
          删除
        </a-button>
      </a-popconfirm>
    </div>
    <HostFormModal v-model:open="formOpen" :host="editing" />
  </div>
</template>

<style scoped>
.host-tree {
  display: flex;
  flex-direction: column;
  gap: 8px;
  height: 100%;
  padding: 8px;
  overflow: hidden;
}

.toolbar {
  display: flex;
  gap: 6px;
  flex: none;
}

.empty {
  padding: 24px 8px;
  text-align: center;
  font-size: 12px;
  color: var(--text-3);
}

/* 四个动作排两行两列：一行四颗在 260px 侧栏里会被挤成换行的残块，
   按钮上的图标还会和文字错开 */
.actions {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 6px;
  flex: none;
  padding-top: 8px;
  border-top: 1px solid var(--line);
}
</style>
