<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { message } from 'ant-design-vue'

import { formatTime } from '@/lib/format'
import { useHostkeysStore } from '@/stores/hostkeys'
import { useHostsStore } from '@/stores/hosts'
import { useSettingsStore } from '@/stores/settings'
import { useThemeStore } from '@/stores/theme'

const settings = useSettingsStore()
const themeStore = useThemeStore()
const hosts = useHostsStore()
const hostkeys = useHostkeysStore()
const [messageApi, holder] = message.useMessage()

const PRIMARY_COLORS = ['#1677ff', '#13a8a8', '#722ed1', '#d4380d', '#389e0d']

const identityLabel = ref('')
/** 私钥文本只活在这一次提交里：不写 store、不进 localStorage。 */
const privateKey = ref('')
const identityPassphrase = ref('')

const addingIdentity = ref(false)

onMounted(() => {
  void hostkeys.refresh()
  void hosts.refresh()
})

async function addIdentity(): Promise<void> {
  if (!identityLabel.value.trim() || !privateKey.value.trim()) {
    messageApi.warning('名称和私钥内容都要填')
    return
  }
  addingIdentity.value = true
  const ok = await hosts.addIdentity(
    identityLabel.value.trim(),
    privateKey.value,
    identityPassphrase.value ? identityPassphrase.value : null,
  )
  addingIdentity.value = false
  if (!ok) {
    messageApi.error(hosts.error ?? '导入私钥失败')
    return
  }
  identityLabel.value = ''
  privateKey.value = ''
  identityPassphrase.value = ''
  messageApi.success('已导入')
}

async function dropIdentity(id: string, label: string): Promise<void> {
  if (await hosts.removeIdentity(id)) messageApi.success(`已删除 ${label}`)
  else messageApi.error(hosts.error ?? '删除失败')
}

async function forget(entry: { pattern: string, algorithm: string }): Promise<void> {
  const { host, port } = splitPattern(entry.pattern)
  if (await hostkeys.forget(host, port, entry.algorithm)) messageApi.success(`已忘记 ${entry.pattern}`)
  else messageApi.error(hostkeys.error ?? '删除失败')
}

async function importSystem(): Promise<void> {
  if (await hostkeys.importSystem()) messageApi.success(`导入了 ${hostkeys.importedCount ?? 0} 条`)
  else messageApi.error(hostkeys.error ?? '导入失败')
}

/** known_hosts 的 pattern 对非 22 端口写成 `[host]:port`，删除命令要的是拆开的两截。 */
function splitPattern(pattern: string): { host: string, port: number } {
  const bracketed = /^\[(.*)\]:(\d+)$/.exec(pattern)
  if (bracketed) return { host: bracketed[1], port: Number(bracketed[2]) }
  const plain = /^(.*):(\d+)$/.exec(pattern)
  if (plain) return { host: plain[1], port: Number(plain[2]) }
  return { host: pattern, port: 22 }
}
</script>

<template>
  <div class="drawer-body">
    <holder />
    <a-form layout="vertical">
      <a-divider orientation="left" style="margin-top: 0">
        外观
      </a-divider>
      <a-form-item label="深色模式">
        <a-switch :checked="themeStore.dark" @change="themeStore.toggleDark()" />
      </a-form-item>
      <a-form-item label="主色">
        <a-space>
          <a-button
            v-for="color in PRIMARY_COLORS"
            :key="color"
            class="swatch"
            :style="{ background: color }"
            :type="themeStore.colorPrimary === color ? 'primary' : 'default'"
            @click="themeStore.colorPrimary = color"
          />
        </a-space>
      </a-form-item>
      <a-form-item label="终端字号">
        <a-input-number v-model:value="settings.fontSize" :min="10" :max="28" />
      </a-form-item>

      <a-divider orientation="left">
        文件
      </a-divider>
      <a-form-item label="下载目录">
        <!-- 没接系统保存对话框：目录由用户写死，下载按钮才知道往哪放 -->
        <a-input v-model:value="settings.downloadDir" placeholder="D:\Downloads 或 /home/dong/Downloads" />
      </a-form-item>

      <a-divider orientation="left">
        私钥
      </a-divider>
      <a-list size="small" :data-source="hosts.identities">
        <template #renderItem="{ item }">
          <a-list-item>
            <span>
              {{ item.label }}
              <a-tag v-if="item.has_passphrase">
                有口令
              </a-tag>
              <a-tag v-if="item.is_ppk">
                PPK
              </a-tag>
            </span>
            <template #actions>
              <a-button size="small" type="text" danger @click="void dropIdentity(item.id, item.label)">
                删除
              </a-button>
            </template>
          </a-list-item>
        </template>
      </a-list>
      <a-form-item label="名称">
        <a-input v-model:value="identityLabel" placeholder="bastion-key" />
      </a-form-item>
      <a-form-item label="私钥内容（PEM 或 .ppk 文本）">
        <a-textarea v-model:value="privateKey" :rows="4" spellcheck="false" />
      </a-form-item>
      <a-form-item label="私钥口令（没有就留空）">
        <a-input-password v-model:value="identityPassphrase" autocomplete="off" />
      </a-form-item>
      <a-button type="primary" size="small" :loading="addingIdentity" @click="void addIdentity()">
        导入私钥
      </a-button>

      <a-divider orientation="left">
        主机密钥
      </a-divider>
      <a-space style="margin-bottom: 8px">
        <a-button size="small" @click="void hostkeys.refresh()">
          刷新
        </a-button>
        <a-button size="small" @click="void importSystem()">
          导入系统 known_hosts
        </a-button>
      </a-space>
      <a-list size="small" :data-source="hostkeys.known">
        <template #renderItem="{ item }">
          <a-list-item>
            <span class="mono">
              {{ item.pattern }} {{ item.algorithm }}
              <span class="dim">{{ formatTime(item.added_at_ms) }} · {{ item.source === 'imported' ? '系统导入' : '本应用确认' }}</span>
            </span>
            <template #actions>
              <a-popconfirm title="忘记这台主机的密钥？下次连接会重新确认。" ok-text="忘记" cancel-text="取消" @confirm="void forget(item)">
                <a-button size="small" type="text" danger>
                  忘记
                </a-button>
              </a-popconfirm>
            </template>
          </a-list-item>
        </template>
      </a-list>
      <a-empty v-if="hostkeys.known.length === 0" description="还没有任何已知主机" />
    </a-form>
  </div>
</template>

<style scoped>
.drawer-body {
  padding: 4px 8px 24px;
}

.swatch {
  width: 26px;
  height: 26px;
  padding: 0;
}

.mono {
  font-family: ui-monospace, Menlo, Consolas, monospace;
  font-size: 12px;
}

.dim {
  opacity: 0.6;
  margin-left: 6px;
}
</style>
