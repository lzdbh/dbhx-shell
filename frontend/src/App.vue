<script setup lang="ts">
import { computed, ref } from 'vue'
import { message, theme } from 'ant-design-vue'

import { ping, pingFailure } from '@/lib/ipc/client'
import { describeIpcError, normalizeIpcError } from "@/lib/ipc/errors"
import type { ErrorCode, Pong } from '@/lib/ipc/bindings'
import { useThemeStore } from '@/stores/theme'

// AntDV 的静态 message.* 不读 ConfigProvider 的主题，必须用 useMessage + contextHolder
const [messageApi, contextHolder] = message.useMessage()
const themeStore = useThemeStore()

const antTheme = computed(() => ({
  algorithm: themeStore.dark ? theme.darkAlgorithm : theme.defaultAlgorithm,
  token: { colorPrimary: themeStore.colorPrimary },
}))

const seq = ref(0)
const pong = ref<Pong | null>(null)
const errorText = ref('')
const busy = ref(false)

async function runPing(): Promise<void> {
  busy.value = true
  errorText.value = ''
  try {
    seq.value += 1
    pong.value = await ping(seq.value, `d1-${Date.now()}`)
    messageApi.success(`pong #${pong.value.seq}`)
  } catch (err) {
    errorText.value = describeIpcError(normalizeIpcError(err))
  } finally {
    busy.value = false
  }
}

// 错误通道探针：host_key_changed 是必须硬阻断的那一类，UI 侧要能原样拿到 code
async function runFailureProbe(code: ErrorCode): Promise<void> {
  busy.value = true
  pong.value = null
  try {
    await pingFailure(code, 'probe from D1 smoke page')
    errorText.value = '预期返回 Err，实际返回了 Ok —— 契约有变'
  } catch (err) {
    errorText.value = describeIpcError(normalizeIpcError(err))
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <a-config-provider :theme="antTheme">
    <context-holder />
    <a-layout style="min-height: 100vh">
      <a-layout-header style="display: flex; align-items: center; gap: 16px">
        <a-typography-title :level="4" style="margin: 0; color: inherit">
          dbhx shell · D1 冒烟页
        </a-typography-title>
        <a-switch
          :checked="themeStore.dark"
          checked-children="暗"
          un-checked-children="亮"
          @change="themeStore.toggleDark()"
        />
        <a-button :loading="busy" type="primary" @click="runPing">
          ping
        </a-button>
        <a-button :disabled="busy" danger @click="runFailureProbe('host_key_changed')">
          注入 host_key_changed
        </a-button>
      </a-layout-header>
      <a-layout-content style="padding: 24px">
        <a-space direction="vertical" size="middle" style="width: 100%">
          <a-alert
            v-if="errorText"
            :message="errorText"
            type="error"
            show-icon
          />
          <a-descriptions
            v-if="pong"
            :column="1"
            bordered
            size="small"
            title="来自 Rust 的 Pong"
          >
            <a-descriptions-item label="seq">{{ pong.seq }}</a-descriptions-item>
            <a-descriptions-item label="echo">{{ pong.echo }}</a-descriptions-item>
            <a-descriptions-item label="server_time_ms">
              {{ pong.server_time_ms }}
            </a-descriptions-item>
            <a-descriptions-item label="target">
              {{ pong.target.username }}@{{ pong.target.host }}:{{ pong.target.port }}
            </a-descriptions-item>
          </a-descriptions>
          <!-- 终端调色板的可见性检查：M1 要把同一份 ansi 喂给 xterm.js -->
          <a-typography-paragraph type="secondary">
            当前终端底色：{{ themeStore.terminalTheme.background }}（{{ themeStore.terminalTheme.id }}）
          </a-typography-paragraph>
          <div class="ansi-row">
            <span
              v-for="(color, index) in themeStore.terminalTheme.ansi"
              :key="index"
              class="ansi-cell"
              :style="{ background: color }"
            />
          </div>
        </a-space>
      </a-layout-content>
    </a-layout>
  </a-config-provider>
</template>

<style scoped>
.ansi-row {
  display: flex;
  gap: 4px;
}

.ansi-cell {
  width: 28px;
  height: 18px;
  border-radius: 2px;
  border: 1px solid rgba(128, 128, 128, 0.35);
}
</style>
