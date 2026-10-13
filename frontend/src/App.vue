<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'

import { theme } from 'ant-design-vue'

import { startEventBridge } from '@/lib/ipc/bus'
import { applyFontSizeAll, applyTheme, focusTerminal } from '@/lib/terminal/registry'
import { installShortcuts, type ShortcutHandlers } from '@/lib/shortcuts'
import { clearSaved, resolve, save, savedSessions } from '@/lib/session-restore'
import AskModal from '@/components/AskModal.vue'
import HostTree from '@/components/HostTree.vue'
import LockOverlay from '@/components/LockOverlay.vue'
import SettingsDrawer from '@/components/SettingsDrawer.vue'
import SftpPanel from '@/components/SftpPanel.vue'
import TabBar from '@/components/TabBar.vue'
import TerminalPane from '@/components/TerminalPane.vue'
import TransferTray from '@/components/TransferTray.vue'
import { useHostsStore } from '@/stores/hosts'
import { useLayoutStore } from '@/stores/layout'
import { useLockStore } from '@/stores/lock'
import { useSessionsStore } from '@/stores/sessions'
import { useSettingsStore } from '@/stores/settings'
import { useThemeStore } from '@/stores/theme'
import { useTransfersStore } from '@/stores/transfers'

const hosts = useHostsStore()
const layout = useLayoutStore()
const lock = useLockStore()
const sessions = useSessionsStore()
const settings = useSettingsStore()
const themeStore = useThemeStore()
const transfers = useTransfersStore()

const drawerOpen = ref(false)

let stopBridge: (() => void) | null = null
let stopShortcuts: (() => void) | null = null
let stopActivity: (() => void) | null = null

const antTheme = computed(() => ({
  algorithm: themeStore.dark ? theme.darkAlgorithm : theme.defaultAlgorithm,
  token: { colorPrimary: themeStore.colorPrimary },
}))

/**
 * 钉住的会话恰好是当前会话时不分屏。
 *
 * 同一份输出画两遍没有意义，而且两个格子会互相把同一个终端实例抢来抢去
 * （DOM 只有一个，appendChild 到哪里哪里就有画面），看上去就是随机闪烁。
 */
const splitActive = computed(() => layout.pinned !== null && layout.pinned !== sessions.activeId)
const mainSession = computed(() => sessions.activeId)
const pinnedSession = computed(() => (splitActive.value ? layout.pinned : null))
/** 空格子时（关会话、文件面板跟谁）动的就是它：焦点格优先，没焦点才跟着 tab 走 */
const focusedSession = computed(() => layout.focusTarget(sessions.activeId))

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

/** 两格之间跳：不分屏时按 tab 顺序循环，等于 Ctrl+Tab 的左手替代。 */
function cycleFocus(): void {
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
  await Promise.all([sessions.refresh(), transfers.refresh(), hosts.refresh(), lock.refresh()])
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

watch(() => themeStore.terminalTheme, next => {
  sessions.tabs.forEach(tab => { applyTheme(tab.id, next) })
})

watch(() => settings.fontSize, size => { applyFontSizeAll(size) })
</script>

<template>
  <a-config-provider :theme="antTheme">
    <a-layout class="shell">
      <a-layout-header class="bar">
        <span class="brand">dbhx shell</span>
        <a-button size="small" @click="void sessions.openLocal('本机终端')">
          本机终端
        </a-button>
        <a-tooltip title="Ctrl+E：把当前会话钉到右格">
          <a-button size="small" :disabled="sessions.activeId === null" @click="toggleSplit">
            {{ splitActive ? '取消分屏' : '分屏' }}
          </a-button>
        </a-tooltip>
        <a-tooltip v-if="lock.hasPin" title="立即锁定：锁定后禁止新连接与凭据读写">
          <a-button size="small" @click="void lock.lock()">
            锁定
          </a-button>
        </a-tooltip>
        <a-switch
          :checked="themeStore.dark"
          checked-children="暗"
          un-checked-children="亮"
          size="small"
          @change="themeStore.toggleDark()"
        />
        <a-button size="small" @click="drawerOpen = true">
          设置
        </a-button>
      </a-layout-header>
      <a-layout has-sider>
        <a-layout-sider :width="260" theme="light" class="sider">
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
          <div class="terminal-area" :class="{ split: splitActive }">
            <!-- 焦点格高亮不用 :focus-within：用户点了文件面板之后焦点离开了终端，
                 但键盘下一条输入仍然属于这一格 -->
            <TerminalPane
              :session="mainSession"
              class="pane-main"
              :class="{ focused: splitActive && focusedSession === mainSession }"
              @click="pickPane('main')"
            />
            <TerminalPane
              v-if="splitActive"
              :session="pinnedSession"
              class="pane-pinned"
              :class="{ focused: focusedSession === pinnedSession }"
              @click="pickPane('pinned')"
            />
            <a-empty
              v-if="sessions.tabs.length === 0"
              description="从左侧选一台主机，或新建本机终端"
              class="placeholder"
            />
          </div>
        </a-layout>
        <a-layout-sider :width="settings.panelWidth" theme="light" class="sider">
          <div class="panel-host">
            <SftpPanel />
          </div>
          <TransferTray />
        </a-layout-sider>
      </a-layout>
    </a-layout>
    <a-drawer v-model:open="drawerOpen" title="设置" :width="420" placement="right">
      <SettingsDrawer />
    </a-drawer>
    <AskModal />
    <LockOverlay />
  </a-config-provider>
</template>

<style scoped>
.shell {
  height: 100vh;
}

.bar {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 0 16px;
  height: 44px;
  line-height: 44px;
  background: rgba(128, 128, 128, 0.08);
}

.brand {
  font-weight: 600;
  margin-right: 8px;
}

.sider {
  background: rgba(128, 128, 128, 0.04);
  border-right: 1px solid rgba(128, 128, 128, 0.2);
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
  background: rgba(128, 128, 128, 0.08);
  border-bottom: 1px solid rgba(128, 128, 128, 0.16);
}

.terminal-area {
  display: flex;
  flex: 1;
  min-height: 0;
  gap: 4px;
  padding: 4px 6px 6px;
}

/* 等分：拖拽调比例要改的是格子的 flex-basis，而终端实例只有在我们主动 resize
   时才重画，先不做这个复杂度 */
.terminal-area.split > * {
  flex-basis: 0;
}

.pane-main,
.pane-pinned {
  border: 1px solid transparent;
}

/* 当前吃键盘的那格给一圈高亮，否则两格长一样，用户不知道键去了哪 */
.terminal-area.split .pane-main.focused,
.terminal-area.split .pane-pinned.focused {
  border-color: rgba(128, 128, 128, 0.35);
}

.placeholder {
  flex: 1;
  display: flex;
  align-items: center;
  justify-content: center;
}

.panel-host {
  flex: 1;
  min-height: 0;
  overflow: hidden;
}
</style>
