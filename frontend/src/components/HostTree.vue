<script setup lang="ts">
import { computed, onMounted, reactive, ref } from 'vue'
import { Modal, message } from 'ant-design-vue'

import { CopyOutlined, DeleteOutlined, DownOutlined, EditOutlined, FolderAddOutlined, FolderOpenOutlined, FormOutlined, LinkOutlined, PlusOutlined, ReloadOutlined } from '@ant-design/icons-vue'

import type { HostInfo } from '@/lib/ipc/bindings'
import { fileWindowApi } from '@/lib/ipc/client'
import { openMenu } from '@/lib/contextMenu'
import type { MenuPoint, MenuItem } from '@/lib/contextMenu'
import { endHostDrag, writeHostDrag } from '@/lib/hostDrag'
import { UNGROUPED, useHostsStore } from '@/stores/hosts'
import { useSessionsStore } from '@/stores/sessions'
import HostFormModal from './HostFormModal.vue'
import OsIcon from './OsIcon.vue'

const hosts = useHostsStore()
const sessions = useSessionsStore()
const [messageApi, holder] = message.useMessage()
const [modalApi, modalHolder] = Modal.useModal()

const keyword = ref('')
const selectedKey = ref<string | null>(null)
const formOpen = ref(false)
const editing = ref<HostInfo | null>(null)
/** 克隆的来源记录；只在 `editing` 为空时起作用。 */
const seeding = ref<HostInfo | null>(null)
/** 从分组节点新建时带上的分组名；编辑已有主机时忽略。 */
const presetGroup = ref<string | null>(null)

const HOST_PREFIX = 'h:'
const GROUP_PREFIX = 'g:'

const filtered = computed(() => {
  const text = keyword.value.trim().toLowerCase()
  if (!text) return hosts.hosts
  return hosts.hosts.filter(h =>
    [h.label, h.group ?? '', h.endpoint.host, h.endpoint.username].join(' ').toLowerCase().includes(text),
  )
})

const treeData = computed(() =>
  groupsOfShown.value.map(group => ({
    title: `${group.name}（${group.hosts.length}）`,
    key: `${GROUP_PREFIX}${group.name}`,
    selectable: false,
    children: group.hosts.map(host => ({
      // 标题只留名字：地址那半截在 260px 的侧栏里几乎总被省略号切掉，反而最没用的
      // `:22` 每次都露着。完整身份挪进 tooltip 和右键菜单的提示里，同名主机照样分得开。
      title: host.label,
      tip: `${host.label} · ${hosts.subtitle(host)}`,
      os: host.os,
      key: `${HOST_PREFIX}${host.id}`,
    })),
  })),
)

/**
 * 搜索时把空分组滤掉。
 *
 * 空分组是「事先建好的坑」，只有在浏览全部主机时才有意义；用户已经在搜东西了，
 * 底下挂一排 `（0）` 的空节点只会让结果看起来像没搜到。
 */
const groupsOfShown = computed(() => {
  const list = hosts.groupsOf(filtered.value)
  if (keyword.value.trim() === '') return list
  return list.filter(group => group.hosts.length > 0)
})

async function connect(host: HostInfo): Promise<void> {
  const result = await sessions.openRemote(host.id)
  if (!result.ok) messageApi.error(result.error ?? '连接失败')
  else messageApi.success(`正在连接 ${host.label}`)
}

/** 手上只有 id 的地方（双击、拖放）先把记录捞回来再拨。找不到就是记录被删了。 */
function connectById(id: string): void {
  const host = hosts.byId(id)
  if (host) void connect(host)
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
  seeding.value = null
  presetGroup.value = null
  formOpen.value = true
}

/**
 * 克隆：开一个新增表单，字段照抄点的那台。
 *
 * 走 `seed` 而不是把记录塞进 `host`，是因为表单里「带 id 就是改、不带就是增」这一个
 * 判断决定了保存落到哪条记录上；克隆必须落在新记录上。
 * 口令不跟着复制——要复制得在表单里显式点「取回已存口令」。
 */
function clone(host: HostInfo): void {
  editing.value = null
  seeding.value = host
  presetGroup.value = null
  formOpen.value = true
}

function create(groupName: string | null = null): void {
  editing.value = null
  seeding.value = null
  presetGroup.value = groupName === UNGROUPED ? null : groupName
  formOpen.value = true
}

/**
 * 展开状态自己攥着。
 *
 * `default-expand-all` 只在挂载那一刻算一遍 key，之后新建或改名的分组不在这份名单里，
 * 于是「建组 → 往里加主机 → 主机看不见」：组一直是收起的。改成默认全开，只记用户手动收起的那些。
 */
const collapsed = ref<string[]>([])

const expandedKeys = computed(() =>
  treeData.value
    .filter(node => node.children.length > 0)
    .map(node => String(node.key))
    .filter(key => !collapsed.value.includes(key)),
)

function onExpand(_keys: unknown, info: { node?: { key?: unknown }, expanded?: boolean }): void {
  const key = String(info?.node?.key ?? '')
  if (!key.startsWith(GROUP_PREFIX)) return
  if (info.expanded) collapsed.value = collapsed.value.filter(item => item !== key)
  else if (!collapsed.value.includes(key)) collapsed.value = [...collapsed.value, key]
}

/**
 * 双击树上的节点。
 *
 * 「选中再点连接」在天天用这个程序的人手里是两次多余动作，Xshell 也是双击即拨号。
 * 分组节点双击什么都不做：折叠有箭头管，双击再管一份就成两个入口做一件事了。
 */
function onNodeDoubleClick(key: unknown): void {
  const id = hostIdOf(key)
  if (id !== null) void connectById(id)
}

/** 只有主机节点能拖：分组不是一个连得上的东西。 */
function hostIdOf(key: unknown): string | null {
  const raw = String(key ?? '')
  return raw.startsWith(HOST_PREFIX) ? raw.slice(HOST_PREFIX.length) : null
}

function onNodeDragstart(key: unknown, event: DragEvent): void {
  const id = hostIdOf(key)
  if (id === null) {
    event.preventDefault()
    return
  }
  writeHostDrag(event, id)
}

/** 新建/重命名分组的小对话框。`from` 非空表示改名，空表示新建。 */
const groupDialog = reactive<{ open: boolean, from: string | null, name: string }>({ open: false, from: null, name: '' })
const groupError = ref<string | null>(null)

function openGroupDialog(from: string | null = null): void {
  groupDialog.from = from
  groupDialog.name = from ?? ''
  groupError.value = null
  groupDialog.open = true
}

/** 就地校验：名字是用户敲的，`groupNameProblem` 说的话直接顶在输入框下面，比弹 toast 好找。 */
function checkGroupName(): void {
  groupError.value = groupDialog.from === null
    ? hosts.addGroup(groupDialog.name)
    : hosts.renameGroup(groupDialog.from, groupDialog.name)
}

function submitGroupDialog(): void {
  checkGroupName()
  if (groupError.value === null) groupDialog.open = false
}

/**
 * 删除的确认框。
 *
 * 右键菜单点完菜单就没了，没有宿主可挂 `a-popconfirm`，所以走 `useModal` 的确认框——
 * 它和 `messageApi` 一样是从当前主题上下文里出来的，深色模式下不会突然变成一块白。
 */
function askRemove(host: HostInfo): void {
  void modalApi.confirm({
    title: `删除 ${host.label}？`,
    content: '这台主机连同已存口令会一起抹掉，无法恢复。',
    okText: '删除',
    cancelText: '取消',
    okType: 'danger',
    onOk: () => remove(host),
  })
}

function hostMenu(at: MenuPoint, host: HostInfo): void {
  openMenu(at, [
    { key: 'connect', label: '连接', hint: '双击', icon: LinkOutlined, onClick: () => { void connect(host) } },
    // 本机/telnet 记录没有 SFTP：给个灰掉的条目比点了弹一句英文报错好
    { key: 'files', label: '文件面板', icon: FolderOpenOutlined, disabled: host.protocol !== 'ssh', onClick: () => { void openPanel(host) } },
    { key: 'edit', label: '编辑', icon: EditOutlined, onClick: () => { edit(host) } },
    { key: 'clone', label: '克隆', hint: `${host.label} 副本`, icon: CopyOutlined, onClick: () => { clone(host) } },
    { key: 'sep', separator: true },
    { key: 'remove', label: '删除', icon: DeleteOutlined, danger: true, onClick: () => { askRemove(host) } },
  ])
}

/**
 * 分组节点上的右键。
 *
 * 「未分组」是虚拟节点（后端存的是 `None`），它没有名字可以改、也没有名单可以删，
 * 所以那两个动作对它直接不出现——出现在菜单里再点失败，比不出现更让人以为程序坏了。
 */
function groupMenu(at: MenuPoint, groupName: string): void {
  const virtual = groupName === UNGROUPED
  const empty = hosts.isEmptyGroup(groupName)
  const list: MenuItem[] = [
    {
      key: 'new',
      // 「未分组」这个节点新建出来就该还是未分组，所以给它的是不带分组的新建
      label: virtual ? '新建主机' : `在「${groupName}」新建主机`,
      icon: PlusOutlined,
      onClick: () => { create(virtual ? null : groupName) },
    },
  ]
  if (!virtual) {
    list.push({ key: 'sep', separator: true })
    list.push({
      key: 'rename',
      label: '重命名分组',
      icon: FormOutlined,
      disabled: !empty,
      hint: empty ? undefined : '组里还有主机',
      onClick: () => { openGroupDialog(groupName) },
    })
    list.push({
      key: 'remove',
      label: '删除分组',
      icon: DeleteOutlined,
      danger: true,
      disabled: !empty,
      hint: empty ? undefined : '组里还有主机',
      onClick: () => {
        const problem = hosts.removeGroup(groupName)
        if (problem) messageApi.error(problem)
        else messageApi.success(`已删除分组「${groupName}」`)
      },
    })
  }
  openMenu(at, list)
}

/**
 * 树上的右键。
 *
 * 不用 `a-tree` 的 `@right-click`：那个事件在 ant-design-vue 4.2 里只是类型声明
 * （从 React 版抄下来的 props），`vc-tree` 的实现里压根没有 contextmenu 处理器，
 * 绑上去永远不触发——「主机树右键没反应」报的就是这个。所以改成节点自己接
 * 原生 contextmenu，空白处由外层那层 div 接。
 *
 * 分组的 key 是名字不是 id，所以按前缀分流后各自取自己那一支。
 */
function onNodeContext(key: unknown, event: MouseEvent): void {
  const at: MenuPoint = { x: event.clientX, y: event.clientY }
  const hostId = hostIdOf(key)
  if (hostId !== null) {
    const host = hosts.byId(hostId)
    // 右键顺手把它选上：菜单一收就什么都没留下，高亮是「刚才动的是这台」唯一的痕迹
    if (host) selectedKey.value = `${HOST_PREFIX}${hostId}`
    if (host) hostMenu(at, host)
    return
  }
  const raw = String(key ?? '')
  if (raw.startsWith(GROUP_PREFIX)) groupMenu(at, raw.slice(GROUP_PREFIX.length))
}

/**
 * 「新建」的两种东西。
 *
 * 工具栏那颗箭头和树背景的右键共用这一份：两个入口给两份名单，
 * 早晚会出现一边有、一边没有，用户就以为程序行为不一致。
 */
function newItems(): MenuItem[] {
  return [
    { key: 'new', label: '新建主机', icon: PlusOutlined, onClick: () => { create() } },
    { key: 'group', label: '新建分组', icon: FolderAddOutlined, onClick: () => { openGroupDialog() } },
  ]
}

/**
 * 分裂按钮的箭头那颗：把上面两条开在按钮下沿。
 *
 * 坐标不用光标位置而用按钮自身的矩形，是因为点箭头时第一个条目正好落在指针下面
 * （和资源管理器的口径一致），而右键整颗按钮也走同一个函数，两个入口看到的是同一个框。
 */
function openNewMenu(event: MouseEvent): void {
  const box = (event.currentTarget as HTMLElement).getBoundingClientRect()
  openMenu({ x: Math.round(box.left), y: Math.round(box.bottom + 2) }, newItems())
}

/** 树背景上的右键：没有节点可指，那就给「凭空加东西」的三条。 */
function onBlankContext(event: MouseEvent): void {
  openMenu({ x: event.clientX, y: event.clientY }, [
    ...newItems(),
    { key: 'sep', separator: true },
    { key: 'refresh', label: '刷新列表', icon: ReloadOutlined, onClick: () => { void hosts.refresh() } },
  ])
}
onMounted(() => void hosts.refresh())
</script>

<template>
  <div class="host-tree">
    <holder />
    <modalHolder />
    <div class="toolbar">
      <a-input v-model:value="keyword" size="small" placeholder="搜索主机" allow-clear />
      <!-- 分裂按钮把「新建主机 / 新建分组」收进一颗：点主体仍旧一键建主机（最常用的事
           不该多一步），只有旁边那颗箭头开二选一。
           原先是两颗并排的按钮，其中分组那颗只剩一个图标、没有文字，在 260px 的侧栏里
           看着像和「新建」互不相干的第三个动作，而不是它的另一件事。
           树背景的右键也留着同样两条，两个入口共用一份名单。 -->
      <a-button-group size="small" class="new-split">
        <a-button type="primary" @click="create()">
          <template #icon>
            <PlusOutlined />
          </template>
          新建
        </a-button>
        <a-tooltip title="新建主机 / 新建分组">
          <a-button type="primary" class="caret" @click="openNewMenu" @contextmenu.prevent="openNewMenu">
            <template #icon>
              <DownOutlined />
            </template>
          </a-button>
        </a-tooltip>
      </a-button-group>
    </div>
    <a-alert v-if="hosts.error" :message="hosts.error" type="error" banner />
    <!-- 右键接在这层自己造的 div 上，而不是 `<a-spin>` 的 attribute 透传：
         ADTV 的 Spin 不吃 fallthrough，挂上去的监听器会被静默丢掉（实测点了没反应）。
         它撑满侧栏剩下的整块高度，所以树底下那片空白也算「树背景」，
         而不是只有几行字能点。节点自己 `.stop`，不会被这里接到。 -->
    <div class="tree-body" @contextmenu.prevent="onBlankContext">
      <a-spin :spinning="hosts.loading">
        <!-- 空状态只给一句话：组件库那台插画在侧栏 260px 里又大又抢，而且「还没有主机」
             这件事上面那颗「新建」按钮已经说清楚了 -->
        <div v-if="treeData.length === 0" class="empty">
          {{ keyword ? '没有匹配的主机' : '还没有主机，先新建一台' }}
        </div>
        <a-tree
          v-else
          :tree-data="treeData"
          :selected-keys="selectedKey ? [selectedKey] : []"
          :expanded-keys="expandedKeys"
          block-node
          @expand="onExpand"
          @select="keys => selectedKey = (keys[0] ?? null) as string | null"
        >
          <!-- 自己画标题只为三件事：`@dblclick` 直接拨号、`draggable` 拖去宫格、
               以及把原生标题挂成 tooltip。`a-tree` 既没有双击事件也没有拖拽属性。 -->
          <template #title="node">
            <span
              class="node"
              :class="{ leaf: hostIdOf(node.key) !== null }"
              :title="String(node.tip ?? node.title ?? '')"
              :draggable="hostIdOf(node.key) !== null ? 'true' : 'false'"
              @dblclick.stop="onNodeDoubleClick(node.key)"
              @contextmenu.prevent.stop="onNodeContext(node.key, $event)"
              @dragstart="onNodeDragstart(node.key, $event)"
              @dragend="endHostDrag()"
            >
              <!-- 只有主机行挂系统图标：分组不是某台机器，给它画个机型是假信息 -->
              <OsIcon v-if="hostIdOf(node.key) !== null" :os="node.os" />
              <span class="label">{{ node.title }}</span>
            </span>
          </template>
        </a-tree>
      </a-spin>
    </div>
    <HostFormModal
      v-model:open="formOpen"
      :host="editing"
      :seed="seeding"
      :preset-group="presetGroup"
      @saved="seeding = null"
    />
    <a-modal
      v-model:open="groupDialog.open"
      :title="groupDialog.from === null ? '新建分组' : `重命名「${groupDialog.from}」`"
      ok-text="保存"
      cancel-text="取消"
      @ok="submitGroupDialog"
    >
      <a-input
        v-model:value="groupDialog.name"
        placeholder="例如 生产环境"
        :status="groupError ? 'error' : undefined"
        @press-enter="submitGroupDialog"
        @input="groupError = null"
      />
      <!-- 校验就地显示，不弹 toast：用户还在框里改，提示应该长在改的那个东西底下 -->
      <div v-if="groupError" class="group-error">{{ groupError }}</div>
    </a-modal>
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

/* 搜索框吃掉剩下的宽度，这颗分裂按钮只占自己那么宽 */
.new-split {
  flex: none;
}

/* AntD 对「纯图标按钮」里的图标统一乘了 1.143（`.ant-btn-icon-only > span`），
   于是箭头被画成 16px，比左边「新建」那颗 14px 的加号还大一号，看着像它才是主角。
   钉平再收一号：它只是「这里还有菜单」的记号，不该和主图标争。 */
.new-split :deep(.caret .anticon) {
  transform: none;
  font-size: 12px;
}

.tree-body {
  flex: 1 1 auto;
  min-height: 0;
  overflow-y: auto;
  /* 横向不出滚动条：侧栏就 260px，长标题由 `.node` 的省略号收掉，
     能横着拖反而会把整条侧栏撑出一条没用的轨道 */
  overflow-x: hidden;
}

.empty {
  padding: 24px 8px;
  text-align: center;
  font-size: 12px;
  color: var(--text-3);
}

/* 标题自己占一整行：双击和拖拽要落在标题上，而 `block-node` 只保证行是整块可点、
   标题本身还是内联的——不撑满的话文字右侧那块空白双击没反应。
   行里是「图标 + 名字」两段，所以省略号交给 `.label`：图标不能被挤掉，
   而完整身份在原生 tooltip 里（`:title` 挂在这个 span 上）。 */
.node {
  display: flex;
  align-items: center;
  gap: 6px;
  overflow: hidden;
}

.node .label {
  min-width: 0;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}

/* 图标比名字淡一档：扫列表时找的是名字，系统只是旁边的一个记号。 */
.node .osi {
  color: var(--text-3);
  font-size: 13px;
}

/* 主机是叶子，永远不会有展开箭头，可 AntD 照样给它留一整个 26px 的 switcher 空槽：
   侧栏只有 260px，名字前面先被吃掉 52px，看着就是「莫名往右缩了一大截」。
   空槽压到 10px，层级缩进那 26px 原样留着，所以分组名和主机名之间仍有一档看得见的差。
   只挑带 `.leaf` 的行——空分组也是 `switcher-noop`，跟着缩会让它和别的分组名错开。 */
.host-tree :deep(.ant-tree-treenode:has(.node.leaf) > .ant-tree-switcher) {
  width: 10px;
  min-width: 10px;
}

.group-error {
  margin-top: 6px;
  font-size: 12px;
  color: var(--err);
}
</style>
