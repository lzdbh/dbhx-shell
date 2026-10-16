<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'

import { AppstoreOutlined, BulbFilled, BulbOutlined, CodeOutlined, ColumnWidthOutlined, LockOutlined, SettingOutlined, SyncOutlined } from '@ant-design/icons-vue'
import { Modal, message } from 'ant-design-vue'

import { startEventBridge } from '@/lib/ipc/bus'
import { lastSyncAgo, summarize } from '@/lib/syncSummary'
import { applyFontSizeAll, applyTheme, focusTerminal, setHighlightRules } from '@/lib/terminal/registry'
import { installShortcuts, type ShortcutHandlers } from '@/lib/shortcuts'
import { clearSaved, resolve, save, savedSessions } from '@/lib/session-restore'
import { antdTheme } from '@/styles/antd'
import AskModal from '@/components/AskModal.vue'
import ContextMenu from '@/components/ContextMenu.vue'
import FindBar from '@/components/FindBar.vue'
import GridBoard from '@/components/GridBoard.vue'
import HostTree from '@/components/HostTree.vue'
import LockOverlay from '@/components/LockOverlay.vue'
import SettingsDrawer from '@/components/SettingsDrawer.vue'
import TabBar from '@/components/TabBar.vue'
import TerminalPane from '@/components/TerminalPane.vue'
import WindowControls from '@/components/WindowControls.vue'
import { useHostsStore } from '@/stores/hosts'
import { GRID_SIZES, useLayoutStore, type GridSize } from '@/stores/layout'
import { useLockStore } from '@/stores/lock'
import { useQuickStore } from '@/stores/quick'
import { useSessionsStore } from '@/stores/sessions'
import { useSettingsStore } from '@/stores/settings'
import { useSyncStore } from '@/stores/sync'
import { useThemeStore } from '@/stores/theme'
import { useTransfersStore } from '@/stores/transfers'

const hosts = useHostsStore()
const layout = useLayoutStore()
const lock = useLockStore()
const quick = useQuickStore()
const sessions = useSessionsStore()
const settings = useSettingsStore()
const sync = useSyncStore()
const themeStore = useThemeStore()
const transfers = useTransfersStore()

const drawerOpen = ref(false)
const findOpen = ref(false)
// 和抽屉里那套一样的用法：从当前主题上下文里拿 api，深色模式下浮层不会突然变成一块白
const [messageApi, msgHolder] = message.useMessage()
const [modalApi, modalHolder] = Modal.useModal()

let stopBridge: (() => void) | null = null
let stopShortcuts: (() => void) | null = null
let stopActivity: (() => void) | null = null

const antTheme = computed(() => antdTheme(themeStore.dark, themeStore.colorPrimary))

/**
 * 钉住的会话恰好是当前会话时不分屏。
 *
 * 同一份输出画两遍没有意义，而且两个格子会互相把同一个终端实例抢来抢去
 * （DOM 只有一个，appendChild 到哪里哪里就有画面），看上去就是随机闪烁。
 */
const isGrid = computed(() => layout.mode === 'grid')

const splitActive = computed(() => !isGrid.value && layout.pinned !== null && layout.pinned !== sessions.activeId)
const mainSession = computed(() => sessions.activeId)
const pinnedSession = computed(() => (splitActive.value ? layout.pinned : null))
/** 空格子时（关会话、文件面板跟谁）动的就是它：焦点格优先，没焦点才跟着 tab 走 */
const focusedSession = computed(() => layout.focusTarget(sessions.activeId))

/** 顶条上那颗「几格」下拉的选项。 */
const gridOptions = GRID_SIZES.map(size => ({ value: size, label: `${size} 格` }))

/**
 * 切视图。
 *
 * 从标签切到宫格时把当前会话带进第一格：切过去看见九个空框，用户会以为会话被关掉了，
 * 而实际上它们都还在标签栏里。带一个过去，这一眼就看懂了两种模式说的是同一批会话。
 */
function switchMode(next: 'tabs' | 'grid'): void {
  if (next === layout.mode) return
  layout.setMode(next)
  if (next === 'grid' && layout.cells.every(item => item === null)) {
    const current = focusedSession.value ?? sessions.activeId
    if (current !== null) layout.placeCell(0, current)
  }
}

/**
 * 顶栏的「立即同步」。
 *
 * 只在配齐（同步串 + 令牌）时才有这颗按钮：摆一颗点了只会失败的按钮比不摆更糟——
 * 用户会以为同步坏了，而实际只是还没配。配置本身仍然只在设置抽屉里做。
 */
const syncReady = computed(() => Boolean(sync.status?.has_sync_str && sync.status?.has_token))

async function runSync(recreate = false): Promise<void> {
  const report = await sync.syncNow(recreate)
  if (!report) {
    messageApi.error(sync.error ?? '同步失败')
    return
  }
  if (report.outcome === 'recreate_required') {
    // 云上那个片段没了（多半是在网页上被删）。换坐标并把本机这份推上去是会改动云上的
    // 动作，不能替用户决定，所以问一句再走。
    void modalApi.confirm({
      title: '云上那个片段已经不在了',
      content: '重建会在云上开一个新片段，并把本机这份主机列表推上去。本机数据不受影响。',
      okText: '重建并同步',
      cancelText: '先不动',
      onOk: () => { void runSync(true) },
    })
    return
  }
  messageApi.success(summarize(report))
  // 合并会改本机库，主机列表那边得重读一次
  await hosts.refresh()
}

/**
 * 悬浮文案在展开那一刻现算。
 *
 * 「上次同步 3 分钟前」要是渲染时算好的，应用闲置半小时它还写着「刚刚」——
 * 一句关于时间的话，宁可晚半秒也不要说错。
 */
const syncHint = ref('同步主机列表到云端')

function fillSyncHint(open: boolean): void {
  if (!open) return
  const parts = ['同步主机列表到云端']
  const ago = lastSyncAgo(sync.lastSyncAt)
  if (ago) parts.push(ago)
  if (sync.status) parts.push(`本机 ${sync.status.hosts} 台`)
  syncHint.value = parts.join(' · ')
}

const restoreList = computed(() => resolve(savedSessions(), hosts.hosts))
/** 要等主机列表拉回来才知道哪些能自动重开，所以初值一律 false */
const restoreOpen = ref(false)
/** 能直接重拨的那些（本机终端 + 还能在 vault 里对上号的主机） */
const restorableCount = computed(() => restoreList.value.filter(entry => entry.kind !== 'unknown').length)
/** 对不上号的只报数量：地址是用户自己的数据，没必要在横幅里替他列一遍 */
const unrestorableCount = computed(() => restoreList.value.length - restorableCount.value)

function pickPane(pane: 'main' | 'pinned'): void {
  layout.setFocused(paneSession(pane))
}

function paneSession(pane: 'main' | 'pinned'): number | null {
  return pane === 'main' ? mainSession.value : pinnedSession.value
}

/**
 * 焦点跳到下一个有会话的格子。
 *
 * 空格子直接跳过：按 Ctrl+Tab 的人要找的是下一台机器，不是「哪一格是空的」。
 * 一整圈都是空格就原地不动。
 */
function cycleCell(): void {
  const count = layout.cells.length
  for (let hop = 1; hop <= count; hop += 1) {
    const index = (layout.focusCell + hop) % count
    const id = layout.cells[index]
    if (id === null) continue
    layout.focusOnCell(index)
    focusTerminal(id)
    sessions.activate(id)
    return
  }
}

/** 两格之间跳：不分屏时按 tab 顺序循环，等于 Ctrl+Tab 的左手替代。 */
function cycleFocus(): void {
  if (isGrid.value) {
    cycleCell()
    return
  }
  if (splitActive.value) {
    const next = focusedSession.value === pinnedSession.value ? mainSession.value : pinnedSession.value
    layout.setFocused(next)
    focusTerminal(next)
    return
  }
  const ids = sessions.tabs.map(t => t.id)
  if (ids.length < 2) return
  const at = ids.indexOf(sessions.activeId ?? -1)
  sessions.activate(ids[(at + 1) % ids.length])
}

function nextTab(step: number): void {
  const ids = sessions.tabs.map(t => t.id)
  if (ids.length === 0) return
  const at = ids.indexOf(sessions.activeId ?? -1)
  const next = (at + step + ids.length) % ids.length
  sessions.activate(ids[next])
}

function selectTab(index: number): void {
  const id = sessions.tabs[index]?.id
  if (id !== undefined) sessions.activate(id)
}

/** 把当前会话钉到右格；再按一次取消。钉的就是当前会话时等于收回单屏。 */
function toggleSplit(): void {
  const current = sessions.activeId
  if (current === null) return
  if (layout.pinned === current) {
    layout.unpin()
    return
  }
  layout.pin(current)
  // 新钉住的格子只是「给我看」，键盘还是留在主格
  layout.setFocused(current)
}

/**
 * 用户活动跟踪：任何指针/键盘/窗口聚焦动作都回调一次，用来把自动上锁往后推。
 *
 * 监听挂在 `document` 冒泡阶段即可——我们只关心「发生过没有」，不需要抢在任何
 * 处理器之前看到事件；节流由 store 侧的 setTimeout 复位天然完成。
 * 返回卸载函数，交给 onBeforeUnmount。
 */
function watchUserActivity(onActivity: () => void): () => void {
  const events: (keyof DocumentEventMap)[] = ['pointerdown', 'keydown', 'wheel', 'visibilitychange']
  for (const name of events) document.addEventListener(name, onActivity)
  return () => {
    for (const name of events) document.removeEventListener(name, onActivity)
  }
}

const shortcuts: ShortcutHandlers = {
  newLocal: () => { void sessions.openLocal('本机终端') },
  closeActive: () => {
    const target = focusedSession.value
    if (target !== null) void sessions.close(target)
  },
  nextTab: () => { nextTab(1) },
  prevTab: () => { nextTab(-1) },
  selectTab,
  toggleSplit,
  focusNext: cycleFocus,
  openSettings: () => { drawerOpen.value = true },
  find: () => { findOpen.value = true },
  // 缩放写进 settings：字号是全局偏好，格子上的 Ctrl+滚轮和这里的 Ctrl+= 必须改同一个值
  zoom: step => { settings.zoom(step) },
  resetZoom: () => { settings.resetZoom() },
  toggleQuickBar: () => { quick.toggle() },
}

/**
 * 逐条重开。
 *
 * `unknown` 那档（主机记录已被删掉）跳过：我们手里只有地址，没有凭据，
 * 硬要连就得让用户当场输密码——那是「新建连接」的活，不是恢复的活。
 */
async function restoreAll(): Promise<void> {
  // 一条一条连着拨，别一次性把整份列表甩给后端：SSH 拨号是重活，
  // 十几个并发握手会让先起来的终端明显卡
  for (const entry of restoreList.value) {
    if (entry.kind === 'host') await sessions.openRemote(entry.hostId)
    else if (entry.kind === 'local') await sessions.openLocal(entry.label)
  }
  restoreOpen.value = false
  clearSaved()
}

onMounted(async () => {
  // 快捷键先装：它是纯 DOM 的事，万一事件桥那边出问题也不该把键盘一起带走。
  // 锁屏期间整体禁用（第四个入参）：capture 监听比覆盖层先看到按键，不拦的话
  // Ctrl+T 会在锁后偷偷开一个终端。
  stopShortcuts = installShortcuts(document, () => shortcuts, () => splitActive.value, () => !lock.locked)
  // 活动跟踪：任何指针/键盘动作都把自动上锁往后推
  stopActivity = watchUserActivity(() => lock.activity())
  // 再先接事件后拉列表：拉列表期间到达的 term-data 否则会没人认领
  stopBridge = await startEventBridge()
  await Promise.all([sessions.refresh(), transfers.refresh(), hosts.refresh(), lock.refresh(), sync.refresh()])
  // 恢复**不自动拨号**：无人值守地往生产机连不是启动该做的事。
  // 只在「后端确实没有会话」时给一条提示，由用户决定重开哪些。
  // 锁屏时不弹这条：底层界面都被盖住了，先解锁再谈恢复。
  restoreOpen.value = !lock.locked && sessions.tabs.length === 0 && restorableCount.value > 0
})

onBeforeUnmount(() => {
  stopShortcuts?.()
  stopActivity?.()
  stopBridge?.()
})

/* 会话与布局的联动：这些都是「别让格子指向不存在的东西」的记账 */

watch(() => sessions.tabs.map(t => t.id), ids => {
  layout.prune(ids)
  save(sessions.tabs)
})

/**
 * 宫格模式下点标签：把这个会话搬到当前格，而不是让整块画面跟着 tab 走。
 *
 * 标签栏在宫格模式下仍然是「有哪些会话」的清单，但用户点它的时候想看的显然是那一个。
 * 如果它已经钉在某一格里，就跳到那一格（看见≠重新安排）；否则占用当前格——
 * 被挤掉的那个会话不关，只是这一格不再显示它。
 */
watch(() => sessions.activeId, (id) => {
  if (!isGrid.value || id === null) return
  const already = layout.cells.findIndex(item => item === id)
  if (already >= 0) {
    if (already !== layout.focusCell) layout.focusOnCell(already)
    return
  }
  layout.placeCell(layout.focusCell, id)
})

watch(() => themeStore.terminalTheme, next => {
  sessions.tabs.forEach(tab => { applyTheme(tab.id, next) })
})

watch(() => settings.fontSize, size => { applyFontSizeAll(size) })

// 关键字规则是全局偏好：改一条，所有已经开着的终端立刻重画
watch(() => settings.highlightRules, rules => { setHighlightRules(rules) }, { deep: true })
</script>

<template>
  <a-config-provider :theme="antTheme">
    <a-layout class="shell">
      <msgHolder />
      <modalHolder />
      <!-- 这条工具栏顶替了原生标题栏：整条是拖拽区（deep：子元素里也能拖），
           但按钮是 clickable 标签、drag.js 自动豁免，下拉没有可点击标签必须显式关 -->
      <a-layout-header class="bar" data-tauri-drag-region="deep">
        <span class="brand">dbhx shell</span>
        <!-- 顶条只放图标 + tooltip：这五个动作天天按，文字标签把条带撑到一半宽，
             而窗口宽度是要留给终端的 -->
        <a-tooltip title="新建本机终端（Ctrl+T）">
          <a-button size="small" type="text" @click="void sessions.openLocal('本机终端')">
            <template #icon>
              <CodeOutlined />
            </template>
          </a-button>
        </a-tooltip>
        <a-tooltip v-if="!isGrid" title="Ctrl+E：把当前会话钉到右格">
          <a-button
            size="small"
            :type="splitActive ? 'primary' : 'text'"
            :disabled="sessions.activeId === null"
            @click="toggleSplit"
          >
            <template #icon>
              <ColumnWidthOutlined />
            </template>
          </a-button>
        </a-tooltip>
        <!-- 宫格档数就在切换按钮旁边：切过去那一刻用户要做的决定就是「我要几格」 -->
        <a-tooltip v-if="isGrid" title="格子数">
          <a-select
            size="small"
            class="grid-size"
            data-tauri-drag-region="false"
            :value="layout.gridSize"
            :options="gridOptions"
            @change="value => layout.setGridSize(value as GridSize)"
          />
        </a-tooltip>
        <a-tooltip :title="isGrid ? '切回标签视图' : '宫格视图：同时看几台机器，从左边拖一台进格子'">
          <a-button
            size="small"
            :type="isGrid ? 'primary' : 'text'"
            @click="switchMode(isGrid ? 'tabs' : 'grid')"
          >
            <template #icon>
              <AppstoreOutlined />
            </template>
          </a-button>
        </a-tooltip>
        <a-tooltip v-if="lock.hasPin" title="立即锁定：锁定后禁止新连接与凭据读写">
          <a-button size="small" type="text" @click="void lock.lock()">
            <template #icon>
              <LockOutlined />
            </template>
          </a-button>
        </a-tooltip>
        <span class="spacer" />
        <!-- 同步只在配齐之后出现：没配齐时点它只会得到一句「还没配好」，
             与其摆一颗注定失败的按钮，不如让人去设置抽屉里把那两样填上 -->
        <a-tooltip v-if="syncReady" :title="syncHint" @open-change="fillSyncHint">
          <a-button size="small" type="text" :loading="sync.busy" @click="void runSync()">
            <template #icon>
              <SyncOutlined />
            </template>
          </a-button>
        </a-tooltip>
        <a-tooltip :title="themeStore.dark ? '切成浅色（配色偏好两个窗口共用）' : '切成深色'">
          <a-button size="small" type="text" @click="themeStore.toggleDark()">
            <template #icon>
              <BulbFilled v-if="themeStore.dark" />
              <BulbOutlined v-else />
            </template>
          </a-button>
        </a-tooltip>
        <a-tooltip title="设置（Ctrl+,）">
          <a-button size="small" type="text" @click="drawerOpen = true">
            <template #icon>
              <SettingOutlined />
            </template>
          </a-button>
        </a-tooltip>
        <WindowControls />
      </a-layout-header>
      <a-layout has-sider>
        <a-layout-sider :width="260" class="sider">
          <HostTree />
        </a-layout-sider>
        <a-layout class="center">
          <TabBar />
          <div v-if="restoreOpen && restorableCount > 0" class="restore">
            <span>
              上次还有 {{ restorableCount }} 个会话可以重开
              <template v-if="unrestorableCount > 0">，另有 {{ unrestorableCount }} 个找不到对应的主机记录</template>
            </span>
            <a-button size="small" @click="void restoreAll()">全部重开</a-button>
            <a-button size="small" type="text" @click="restoreOpen = false; clearSaved()">
              不用了
            </a-button>
          </div>
          <div class="terminal-area" :class="{ split: splitActive, grid: isGrid }">
            <!-- 查找条浮在格子上，不挤布局：一挤就要重新 fit，画面会跳一下 -->
            <FindBar v-model:open="findOpen" :session="focusedSession" />
            <GridBoard v-if="isGrid" @find="findOpen = true" />
            <!-- 焦点格高亮不用 :focus-within：用户点了文件面板之后焦点离开了终端，
                 但键盘下一条输入仍然属于这一格 -->
            <!-- 没有会话时整格不挂：格子自带边框和 pane 底色，空着就是一块占半屏的空白卡片 -->
            <template v-else>
              <TerminalPane
                v-if="mainSession !== null"
                :session="mainSession"
                class="pane-main"
                :class="{ focused: splitActive && focusedSession === mainSession }"
                @click="pickPane('main')"
                @find="findOpen = true"
              />
              <TerminalPane
                v-if="splitActive"
                :session="pinnedSession"
                class="pane-pinned"
                :class="{ focused: focusedSession === pinnedSession }"
                @click="pickPane('pinned')"
                @find="findOpen = true"
              />
              <div v-if="sessions.tabs.length === 0" class="placeholder">
                从左侧选一台主机，或按 Ctrl+T 新建本机终端
              </div>
            </template>
          </div>
        </a-layout>
      </a-layout>
    </a-layout>
    <a-drawer v-model:open="drawerOpen" title="设置" :width="420" placement="right">
      <SettingsDrawer />
    </a-drawer>
    <AskModal />
    <ContextMenu />
    <LockOverlay />
  </a-config-provider>
</template>

<style scoped>
.shell {
  height: 100vh;
}

/* 底色由 Layout 的 colorBgHeader token 给；高度与内边距在 4.2 里没有对应 token，
   只能在这里写。组件库默认是 64px 的网页页头，对终端窗口来说太高。
   右边不留内边距：窗控按钮要通到窗口右上角，和系统窗口一致。 */
.bar {
  display: flex;
  align-items: center;
  gap: 10px;
  height: 44px;
  line-height: 44px;
  padding: 0 0 0 14px;
  border-bottom: 1px solid var(--line);
}

.brand {
  font-weight: 600;
  letter-spacing: 0.02em;
  margin-right: 6px;
  color: var(--text-1);
}

/* 左侧是「干活的动作」，右侧是主题与设置：把它们推到两头，鼠标移动距离就有了规律 */
.spacer {
  flex: 1;
}

/* 侧栏和顶条同属「外围」，用比窗口底亮一档的表面；终端那块才是内容。
   底色写在这里而不是靠 Layout token：4.2 的 Layout 只有 header/body/trigger 三个色，
   侧栏跟着 body 走会和窗口底一个色，分不出层次。 */
.sider {
  background: var(--bg-surface);
  border-right: 1px solid var(--line);
  overflow: hidden;
}

.center {
  min-width: 0;
}

.restore {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 12px;
  background: var(--bg-raised);
  border-bottom: 1px solid var(--line);
}

.terminal-area {
  /* 查找条以它为定位基准，改成 absolute 的 containing block 就靠这一句 */
  position: relative;
  display: flex;
  flex: 1;
  min-height: 0;
  gap: 6px;
  padding: 6px 8px 8px;
  background: var(--bg-app);
}

/* 等分：拖拽调比例要改的是格子的 flex-basis，而终端实例只有在我们主动 resize
   时才重画，先不做这个复杂度 */
.terminal-area.split > * {
  flex-basis: 0;
}

/* 当前吃键盘的那格给一圈高亮，否则两格长一样，用户不知道键去了哪。
   用主色的淡描边而不是灰色：灰色和「已结束」那种中性边框分不开。 */
.terminal-area.split .pane-main.focused,
.terminal-area.split .pane-pinned.focused {
  border-color: var(--accent-line);
}

/* 档数下拉只有五个值，给 88px 刚好放下「9 格」再加一颗箭头；
   再宽就把顶条右侧那组按钮挤得贴边了 */
.grid-size {
  width: 88px;
}

.placeholder {
  flex: 1;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 12px;
  color: var(--text-3);
}
</style>
