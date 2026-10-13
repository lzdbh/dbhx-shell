<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'

import { theme } from 'ant-design-vue'

import { startEventBridge } from '@/lib/ipc/bus'
import AskModal from '@/components/AskModal.vue'
import HostTree from '@/components/HostTree.vue'
import SettingsDrawer from '@/components/SettingsDrawer.vue'
import SftpPanel from '@/components/SftpPanel.vue'
import TabBar from '@/components/TabBar.vue'
import TerminalView from '@/components/TerminalView.vue'
import TransferTray from '@/components/TransferTray.vue'
import { useSessionsStore } from '@/stores/sessions'
import { useSettingsStore } from '@/stores/settings'
import { useThemeStore } from '@/stores/theme'
import { useTransfersStore } from '@/stores/transfers'

const sessions = useSessionsStore()
const settings = useSettingsStore()
const themeStore = useThemeStore()
const transfers = useTransfersStore()

const drawerOpen = ref(false)
let stopBridge: (() => void) | null = null

const antTheme = computed(() => ({
  algorithm: themeStore.dark ? theme.darkAlgorithm : theme.defaultAlgorithm,
  token: { colorPrimary: themeStore.colorPrimary },
}))

onMounted(async () => {
  // 先接事件再拉列表：拉列表期间到达的 term-data 否则会没人认领
  stopBridge = await startEventBridge()
  await Promise.all([sessions.refresh(), transfers.refresh()])
})

onBeforeUnmount(() => stopBridge?.())
</script>

<template>
  <a-config-provider :theme="antTheme">
    <a-layout class="shell">
      <a-layout-header class="bar">
        <span class="brand">dbhx shell</span>
        <a-button size="small" @click="void sessions.openLocal('本机终端')">
          本机终端
        </a-button>
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
          <div class="terminal-area">
            <!-- 每个 tab 各挂一个终端实例，切换只用 v-show：
                 卸载会销毁 xterm，回滚缓冲区里的日志就没了 -->
            <TerminalView
              v-for="tab in sessions.tabs"
              :key="tab.id"
              :session="tab.id"
              :active="tab.id === sessions.activeId"
              v-show="tab.id === sessions.activeId"
            />
            <a-empty v-if="sessions.tabs.length === 0" description="从左侧选一台主机，或新建本机终端" class="placeholder" />
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

.bar :deep(.ant-layout-header) {
  background: transparent;
}

.sider {
  background: rgba(128, 128, 128, 0.04);
  border-right: 1px solid rgba(128, 128, 128, 0.2);
  overflow: hidden;
}

.center {
  min-width: 0;
}

.terminal-area {
  position: relative;
  flex: 1;
  min-height: 0;
  padding: 6px 8px 8px;
}

.terminal-area > *:not(.placeholder) {
  position: absolute;
  inset: 6px 8px 8px;
}

.placeholder {
  position: absolute;
  inset: 0;
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
