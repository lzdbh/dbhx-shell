<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { message } from 'ant-design-vue'

import { formatTime } from '@/lib/format'
import { useHostkeysStore } from '@/stores/hostkeys'
import { useHostsStore } from '@/stores/hosts'
import { useLockStore } from '@/stores/lock'
import { DEFAULT_FONT_SIZE, MAX_FONT_SIZE, MIN_FONT_SIZE, useSettingsStore } from '@/stores/settings'
import { useThemeStore } from '@/stores/theme'

const settings = useSettingsStore()
const themeStore = useThemeStore()
const hosts = useHostsStore()
const hostkeys = useHostkeysStore()
const lock = useLockStore()
const [messageApi, holder] = message.useMessage()

/** 自动上锁的候选时长（分钟）。0 是「关闭」，放在选择器里而不是开关，好让「多久」和「开不开」是一件事。 */
const IDLE_OPTIONS = [
  { value: 0, label: '不自动锁定' },
  { value: 1, label: '空闲 1 分钟' },
  { value: 5, label: '空闲 5 分钟' },
  { value: 15, label: '空闲 15 分钟' },
  { value: 30, label: '空闲 30 分钟' },
]

const PRIMARY_COLORS = ['#1677ff', '#13a8a8', '#722ed1', '#d4380d', '#389e0d']

const SHORTCUT_COLUMNS = [
  { title: '按键', dataIndex: 'keys', width: 130 },
  { title: '作用', dataIndex: 'what' },
]

/** `Cmd` 在 macOS 上与 `Ctrl` 等价，判定里就是同一个修饰键，这里照实写。 */
const SHORTCUTS = [
  { keys: 'Ctrl/Cmd + T', what: '新建本机终端' },
  { keys: 'Ctrl/Cmd + W', what: '关闭当前格子的会话' },
  { keys: 'Ctrl/Cmd + E', what: '把当前会话钉到右格（再按取消）' },
  { keys: 'Ctrl/Cmd + Tab', what: '切到下一个标签，Shift 反向' },
  { keys: 'Ctrl/Cmd + 1…9', what: '跳到第 N 个标签' },
  { keys: 'Ctrl/Cmd + ,', what: '打开设置' },
  { keys: 'Ctrl/Cmd + F', what: '在当前终端查找，Enter 下一条、Shift+Enter 上一条，Esc 关闭' },
  { keys: 'Ctrl/Cmd + = / -', what: '放大 / 缩小终端字号（也可以 Ctrl + 滚轮）' },
  { keys: 'Ctrl/Cmd + 0', what: '字号回到默认' },
  { keys: 'Ctrl/Cmd + Shift + K', what: '开/收快捷命令栏（不带 Shift 的 Ctrl+K 仍归终端）' },
  { keys: 'Alt + ←/→', what: '分屏时在两格之间移焦点' },
]

const identityLabel = ref('')
/** 私钥文本只活在这一次提交里：不写 store、不进 localStorage。 */
const privateKey = ref('')
const identityPassphrase = ref('')

const addingIdentity = ref(false)

const newPin = ref('')
const confirmPin = ref('')
const savingPin = ref(false)

onMounted(() => {
  void hostkeys.refresh()
  void hosts.refresh()
  void lock.refresh()
})

/** 前端预校验只是少跑一趟后端；真正的长度校验与「锁着不许改锁」都在 Rust 侧。 */
async function savePin(): Promise<void> {
  if (newPin.value.length < 4) {
    messageApi.warning('PIN 至少 4 位')
    return
  }
  if (newPin.value !== confirmPin.value) {
    messageApi.warning('两次输入的 PIN 不一致')
    return
  }
  savingPin.value = true
  try {
    await lock.setPin(newPin.value)
    messageApi.success(lock.hasPin ? '已更新解锁 PIN' : '已启用应用锁')
    newPin.value = ''
    confirmPin.value = ''
  } catch (e) {
    messageApi.error(describe(e))
  } finally {
    savingPin.value = false
  }
}

async function removePin(): Promise<void> {
  try {
    await lock.clearPin()
    messageApi.success('已关闭应用锁')
  } catch (e) {
    messageApi.error(describe(e))
  }
}

function describe(e: unknown): string {
  return e && typeof e === 'object' && 'message' in e ? String((e as { message: unknown }).message) : '操作失败'
}

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
        <!-- 上下限与缩放共用一套常量：设置页能填到 9 而 Ctrl+滚轮 缩不到 9 会很怪 -->
        <a-input-number v-model:value="settings.fontSize" :min="MIN_FONT_SIZE" :max="MAX_FONT_SIZE" />
        <a-button
          v-if="settings.fontSize !== DEFAULT_FONT_SIZE"
          size="small"
          type="link"
          @click="settings.resetZoom()"
        >
          恢复默认
        </a-button>
      </a-form-item>

      <a-divider orientation="left">
        关键字高亮
      </a-divider>
      <!-- 命中判定在 lib/terminal/highlight.ts，这里只填规则表。顺序即优先级：一行里
           两个词撞上时，排在前面的那条赢，所以列表自上而下读就是「越靠前越重要」 -->
      <p class="hint-line">
        命中的关键字在终端里涂上底色；只涂底色不改字色，DOM 渲染器画不出后者。
      </p>
      <div v-for="rule in settings.highlightRules" :key="rule.id" class="hl-row">
        <a-input v-model:value="rule.pattern" size="small" class="hl-pattern" placeholder="关键字，勾上正则后按正则写" />
        <!-- AntDV 4.2 没有 ColorPicker，用原生取色框：它给出的永远是 `#rrggbb`，
             正好是编译环节认的格式，省得再让用户手敲十六进制 -->
        <input v-model="rule.background" type="color" class="hl-color" title="命中底色" aria-label="命中底色">
        <a-checkbox v-model:checked="rule.regex">正则</a-checkbox>
        <a-checkbox v-model:checked="rule.caseSensitive">区分大小写</a-checkbox>
        <a-checkbox v-model:checked="rule.bell">响铃</a-checkbox>
        <a-checkbox v-model:checked="rule.enabled">启用</a-checkbox>
        <a-button size="small" type="text" danger @click="settings.removeHighlightRule(rule.id)">
          删除
        </a-button>
      </div>
      <a-button size="small" @click="settings.addHighlightRule()">
        新增规则
      </a-button>

      <a-divider orientation="left">
        快捷键
      </a-divider>
      <!-- 只做展示：判定逻辑在 lib/shortcuts.ts，这里写错了不会影响行为，但会误导用户，
           所以两边都改的时候记得对齐 -->
      <a-table
        size="small"
        :columns="SHORTCUT_COLUMNS"
        :data-source="SHORTCUTS"
        :pagination="false"
        row-key="keys"
      />

      <a-divider orientation="left">
        文件
      </a-divider>
      <a-form-item label="下载目录">
        <!-- 没接系统保存对话框：目录由用户写死，下载按钮才知道往哪放 -->
        <a-input v-model:value="settings.downloadDir" placeholder="D:\Downloads 或 /home/dong/Downloads" />
      </a-form-item>
      <a-form-item label="日志目录">
        <!-- 留空 = 应用数据目录下的 logs。目录由后端创建，这里只存一个绝对路径 -->
        <a-input v-model:value="settings.logDir" placeholder="留空则用应用数据目录下的 logs" />
      </a-form-item>
      <p class="hint-line">
        日志抄的是终端收到的原始字节，包括被回显出来的口令与密钥片段。只写在本机，
        要发给别人之前先自己看一遍。
      </p>

      <a-divider orientation="left">
        应用锁
      </a-divider>
      <a-form-item label="自动锁定">
        <a-select
          :value="lock.idleMinutes"
          style="width: 200px"
          :options="IDLE_OPTIONS"
          :disabled="!lock.hasPin"
          @update:value="(v) => lock.setIdleMinutes(Number(v))"
        />
      </a-form-item>
      <a-form-item label="解锁 PIN">
        <a-space direction="vertical" style="width: 100%">
          <a-input-password
            v-model:value="newPin"
            :maxlength="32"
            autocomplete="new-password"
            :placeholder="lock.hasPin ? '设置新 PIN 以更换' : '设置一个 PIN 以启用应用锁'"
          />
          <a-input-password
            v-model:value="confirmPin"
            :maxlength="32"
            autocomplete="new-password"
            placeholder="再输入一次确认"
          />
          <a-space>
            <a-button type="primary" size="small" :loading="savingPin" @click="void savePin()">
              {{ lock.hasPin ? '更换 PIN' : '启用并设置 PIN' }}
            </a-button>
            <a-popconfirm
              v-if="lock.hasPin"
              title="关闭应用锁会删除已存的 PIN，确定吗？"
              ok-text="关闭"
              cancel-text="取消"
              @confirm="void removePin()"
            >
              <a-button size="small" danger>
                关闭应用锁
              </a-button>
            </a-popconfirm>
          </a-space>
          <span class="dim">锁定后禁止新建连接与凭据读写，已建立的会话不受影响。</span>
        </a-space>
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

.hint-line {
  margin: 0 0 8px;
  font-size: 12px;
  opacity: 0.65;
}

.hl-row {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
  margin-bottom: 6px;
}

.hl-pattern {
  width: 220px;
}

/* 原生取色框默认带一圈内边距，压成和旁边小控件同高 */
.hl-color {
  width: 34px;
  height: 24px;
  padding: 0 2px;
  border: 1px solid rgba(128, 128, 128, 0.45);
  border-radius: 4px;
  background: none;
  cursor: pointer;
}
</style>
