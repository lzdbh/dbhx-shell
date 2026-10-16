<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import type { Component } from 'vue'
import { message } from 'ant-design-vue'
import {
  BgColorsOutlined,
  CloudSyncOutlined,
  CodeOutlined,
  FolderOpenOutlined,
  KeyOutlined,
  LockOutlined,
} from '@ant-design/icons-vue'

import { formatTime } from '@/lib/format'
import { clipboardApi } from '@/lib/ipc/client'
import type { ProviderKind } from '@/lib/ipc/bindings'
import { lastSyncAgo, summarize } from '@/lib/syncSummary'
import { useHostkeysStore } from '@/stores/hostkeys'
import { useHostsStore } from '@/stores/hosts'
import { useLockStore } from '@/stores/lock'
import { DEFAULT_FONT_SIZE, MAX_FONT_SIZE, MIN_FONT_SIZE, useSettingsStore } from '@/stores/settings'
import { useSyncStore } from '@/stores/sync'
import { useThemeStore } from '@/stores/theme'

const settings = useSettingsStore()
const themeStore = useThemeStore()
const hosts = useHostsStore()
const hostkeys = useHostkeysStore()
const lock = useLockStore()
const sync = useSyncStore()
const [messageApi, holder] = message.useMessage()

/** 同步目的地两选一。值必须是后端契约里的小写串，写错了会被当成非法枚举拒掉。 */
const PROVIDERS: { value: ProviderKind, label: string }[] = [
  { value: 'github', label: 'GitHub' },
  { value: 'gitee', label: 'Gitee' },
]

function providerLabel(provider: ProviderKind): string {
  return PROVIDERS.find(p => p.value === provider)?.label ?? provider
}

/** 自动上锁的候选时长（分钟）。0 是「关闭」，放在选择器里而不是开关，好让「多久」和「开不开」是一件事。 */
const IDLE_OPTIONS = [
  { value: 0, label: '不自动锁定' },
  { value: 1, label: '空闲 1 分钟' },
  { value: 5, label: '空闲 5 分钟' },
  { value: 15, label: '空闲 15 分钟' },
  { value: 30, label: '空闲 30 分钟' },
]

const PRIMARY_COLORS = ['#1677ff', '#13a8a8', '#722ed1', '#d4380d', '#389e0d']

type SectionKey = 'appearance' | 'terminal' | 'files' | 'security' | 'sync' | 'credentials'

/**
 * 左侧导航的六节。
 *
 * 分节按「用户来这儿要办的事」切，不按代码里的 store 切：`settings` 一个 store 里的东西
 * 被拆进了外观和终端两节，因为改字号和改高亮规则是两趟不同的心智。
 */
const SECTIONS: { key: SectionKey, label: string, icon: Component }[] = [
  { key: 'appearance', label: '外观', icon: BgColorsOutlined },
  { key: 'terminal', label: '终端', icon: CodeOutlined },
  { key: 'files', label: '文件', icon: FolderOpenOutlined },
  { key: 'security', label: '安全', icon: LockOutlined },
  { key: 'sync', label: '同步', icon: CloudSyncOutlined },
  { key: 'credentials', label: '凭据', icon: KeyOutlined },
]

const active = ref<SectionKey>('appearance')

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

/**
 * 同步串与令牌只在这一次提交里活着。保存成功就清空输入框，
 * 界面之后只显示「已配置」——想改只能重新贴一遍，这是有意的。
 */
const syncStr = ref('')
const syncToken = ref('')
const syncFragment = ref('')

/* 二级面板的开关：快捷键表、同步的安全说明，以及三个凭据的编辑器、PIN 表单、私钥导入
   表单，都是「配一次就用很久」的东西，常驻会把每一节撑回原来的三屏长。 */
const shortcutsOpen = ref(false)
const noteOpen = ref(false)
const editStr = ref(false)
const editToken = ref(false)
const editFragment = ref(false)
const pinOpen = ref(false)
const importOpen = ref(false)

const lastText = computed(() => (sync.last ? summarize(sync.last) : '这台机器还没同步过'))
const lastAgo = computed(() => lastSyncAgo(sync.lastSyncAt))

onMounted(() => {
  void hostkeys.refresh()
  void hosts.refresh()
  void lock.refresh()
  void sync.refresh()
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
    pinOpen.value = false
  } catch (e) {
    messageApi.error(describe(e))
  } finally {
    savingPin.value = false
  }
}

async function removePin(): Promise<void> {
  try {
    await lock.clearPin()
    pinOpen.value = false
    messageApi.success('已关闭应用锁')
  } catch (e) {
    messageApi.error(describe(e))
  }
}

async function saveSyncStr(): Promise<void> {
  if (await sync.setStr(syncStr.value)) {
    // 清掉明文：界面上之后只剩「已配置」三个字，值不在渲染层留副本
    syncStr.value = ''
    editStr.value = false
    messageApi.success('已保存同步串')
  } else {
    messageApi.error(sync.error ?? '保存失败')
  }
}

async function forgetSyncStr(): Promise<void> {
  if (await sync.setStr(null)) {
    editStr.value = false
    messageApi.success('已忘记本机存的同步串，云上那一份没动')
  } else {
    messageApi.error(sync.error ?? '操作失败')
  }
}

/** 生成只填进输入框，不落盘。存下去才算换锁，而换锁让云上那一份立刻解不开。 */
async function generateSyncStr(): Promise<void> {
  syncStr.value = await sync.newStr()
  messageApi.info('已生成一串。先抄到别处存好，再点保存')
}

async function copySyncStr(): Promise<void> {
  await clipboardApi.write(syncStr.value)
  messageApi.success('已复制到剪贴板')
}

async function saveSyncToken(): Promise<void> {
  if (await sync.setToken(syncToken.value)) {
    syncToken.value = ''
    editToken.value = false
    messageApi.success('已保存云端令牌')
  } else {
    messageApi.error(sync.error ?? '保存失败')
  }
}

async function forgetSyncToken(): Promise<void> {
  if (await sync.setToken(null)) {
    editToken.value = false
    messageApi.success('已忘记令牌')
  } else {
    messageApi.error(sync.error ?? '操作失败')
  }
}

async function saveSyncFragment(): Promise<void> {
  if (await sync.setFragment(syncFragment.value)) {
    syncFragment.value = ''
    editFragment.value = false
    messageApi.success('已记下片段 id')
  } else {
    messageApi.error(sync.error ?? '保存失败')
  }
}

async function forgetSyncFragment(): Promise<void> {
  if (await sync.setFragment(null)) {
    editFragment.value = false
    messageApi.success('本机不再跟着任何片段')
  } else {
    messageApi.error(sync.error ?? '操作失败')
  }
}

/**
 * 换到另一家。后端把四样配置（同步串、令牌、片段 id、基线）按家各存一份，
 * 所以这里只是换了个槽位读：切走不清任何东西，切回来原样还在。
 *
 * 展开着的输入行要收掉 —— 那串东西是为上一家准备的，留在屏幕上等于下一次
 * 点「保存」就把它贴进了另一家的槽位。
 */
async function switchProvider(provider: ProviderKind): Promise<void> {
  const wasConfigured = sync.status?.has_sync_str || sync.status?.has_token || sync.status?.fragment_id
  if (!(await sync.setProvider(provider))) {
    messageApi.error(sync.error ?? '切换失败')
    return
  }
  editStr.value = false
  editToken.value = false
  editFragment.value = false
  syncStr.value = ''
  syncToken.value = ''
  syncFragment.value = ''
  const after = sync.status
  if (after && wasConfigured && !after.has_sync_str && !after.has_token && !after.fragment_id) {
    messageApi.info(`已切到${providerLabel(provider)}：这一家在这台设备上还没配过，切回另一家时那一份原样还在`)
  }
}

/** 勾不勾都当场落盘并回读：它决定推上去的那份里到底有没有私钥，不能只改界面。 */
async function savePrivateKeys(enabled: boolean): Promise<void> {
  if (await sync.setPrivateKeys(enabled))
    messageApi.success(enabled ? '已开启：私钥会一起封进云上那份' : '已关闭：私钥只留在本机')
  else messageApi.error(sync.error ?? '操作失败')
}

async function runSync(recreate = false): Promise<void> {
  const report = await sync.syncNow(recreate)
  if (!report) {
    messageApi.error(sync.error ?? '同步失败')
    return
  }
  const text = summarize(report)
  if (report.outcome === 'recreate_required') messageApi.warning(text)
  else messageApi.success(text)
  // 合并会改本机库，主机列表那边得重读一次
  await hosts.refresh()
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
  importOpen.value = false
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
  <div class="settings">
    <holder />
    <nav class="rail">
      <!-- 用原生 button 而不是 a-menu：这一列只有六项、没有层级、不做键盘漫游，
           换 a-menu 要多背一套 token 和折叠动画，收益是零 -->
      <button
        v-for="section in SECTIONS"
        :key="section.key"
        type="button"
        class="rail-item"
        :class="{ on: active === section.key }"
        @click="active = section.key"
      >
        <component :is="section.icon" />
        <span>{{ section.label }}</span>
      </button>
    </nav>

    <!-- 各节都留在 DOM 里（v-show）：同步串、令牌这些是用户贴进来还没点保存的明文，
         切一节就把输入框卸载掉等于悄悄把他贴的东西扔了 -->
    <div class="pane">
      <section v-show="active === 'appearance'" class="group">
        <h3 class="group-title">
          外观
        </h3>
        <p class="group-sub">
          改完即生效，不需要保存
        </p>
        <div class="card">
          <div class="card-body">
            <div class="row">
              <div class="row-main">
                <div class="row-label">
                  深色模式
                </div>
                <div class="row-hint">
                  界面与终端配色一起切
                </div>
              </div>
              <div class="row-ctl">
                <a-switch :checked="themeStore.dark" @change="themeStore.toggleDark()" />
              </div>
            </div>
            <div class="row">
              <div class="row-main">
                <div class="row-label">
                  主色
                </div>
                <div class="row-hint">
                  选中行、焦点描边和主按钮都从这里取
                </div>
              </div>
              <div class="row-ctl">
                <button
                  v-for="color in PRIMARY_COLORS"
                  :key="color"
                  type="button"
                  class="swatch"
                  :class="{ on: themeStore.colorPrimary === color }"
                  :style="{ background: color }"
                  :aria-label="`主色 ${color}`"
                  :aria-pressed="themeStore.colorPrimary === color"
                  @click="themeStore.colorPrimary = color"
                />
              </div>
            </div>
            <div class="row">
              <div class="row-main">
                <div class="row-label">
                  终端字号
                </div>
                <div class="row-hint">
                  {{ MIN_FONT_SIZE }}–{{ MAX_FONT_SIZE }}；Ctrl + 滚轮 改的就是这个值
                </div>
              </div>
              <div class="row-ctl">
                <!-- 上下限与缩放共用一套常量：设置页能填到 9 而 Ctrl+滚轮 缩不到 9 会很怪 -->
                <a-input-number
                  v-model:value="settings.fontSize"
                  size="small"
                  :min="MIN_FONT_SIZE"
                  :max="MAX_FONT_SIZE"
                  class="font-size"
                />
                <a-button
                  v-if="settings.fontSize !== DEFAULT_FONT_SIZE"
                  size="small"
                  type="link"
                  class="plain-link"
                  @click="settings.resetZoom()"
                >
                  默认 {{ DEFAULT_FONT_SIZE }}
                </a-button>
              </div>
            </div>
          </div>
        </div>
      </section>

      <section v-show="active === 'terminal'" class="group">
        <h3 class="group-title">
          终端
        </h3>
        <p class="group-sub">
          对所有标签页和分屏生效
        </p>
        <div class="card">
          <div class="card-head">
            关键字高亮
            <div class="head-ctl">
              <a-button size="small" type="text" @click="settings.addHighlightRule()">
                新增规则
              </a-button>
            </div>
          </div>
          <div class="card-body">
            <!-- 命中判定在 lib/terminal/highlight.ts，这里只填规则表。顺序即优先级：一行里
                 两个词撞上时，排在前面的那条赢，所以列表自上而下读就是「越靠前越重要」 -->
            <p v-if="settings.highlightRules.length === 0" class="empty">
              还没有规则。加一条关键字，命中的那一行会涂上底色
            </p>
            <div v-for="rule in settings.highlightRules" :key="rule.id" class="row hl-row">
              <a-input v-model:value="rule.pattern" size="small" class="hl-pattern" placeholder="关键字，勾上正则后按正则写" />
              <!-- AntDV 4.2 没有 ColorPicker，用原生取色框：它给出的永远是 `#rrggbb`，
                   正好是编译环节认的格式，省得再让用户手敲十六进制 -->
              <input v-model="rule.background" type="color" class="hl-color" title="命中底色" aria-label="命中底色">
              <a-checkbox v-model:checked="rule.regex">
                正则
              </a-checkbox>
              <a-checkbox v-model:checked="rule.caseSensitive">
                区分大小写
              </a-checkbox>
              <a-checkbox v-model:checked="rule.bell">
                响铃
              </a-checkbox>
              <a-checkbox v-model:checked="rule.enabled">
                启用
              </a-checkbox>
              <a-button size="small" type="text" danger @click="settings.removeHighlightRule(rule.id)">
                删除
              </a-button>
            </div>
            <!-- 空表时这条说明和上面那句灰字会挤成两行相邻的注释，中间还压一道分隔线；
                 等真有规则了它才解释「为什么我这条没生效」 -->
            <div v-if="settings.highlightRules.length > 0" class="row">
              <div class="row-main">
                <div class="row-hint">
                  只涂底色不改字色：DOM 渲染器画不出后者。多条同时命中时，排在前面的那条赢
                </div>
              </div>
            </div>
          </div>
        </div>
        <div class="card">
          <div class="card-body">
            <div class="row">
              <div class="row-main">
                <div class="row-label">
                  快捷键
                </div>
                <div class="row-hint">
                  共 {{ SHORTCUTS.length }} 个，写死在程序里，暂不支持改键
                </div>
              </div>
              <div class="row-ctl">
                <a-button size="small" type="text" @click="shortcutsOpen = true">
                  查看清单
                </a-button>
              </div>
            </div>
          </div>
        </div>
        <a-modal v-model:open="shortcutsOpen" title="快捷键" :footer="null" :width="620">
          <!-- 只做展示：判定逻辑在 lib/shortcuts.ts，这里写错了不会影响行为，但会误导用户，
               所以两边都改的时候记得对齐 -->
          <a-table
            size="small"
            :columns="SHORTCUT_COLUMNS"
            :data-source="SHORTCUTS"
            :pagination="false"
            row-key="keys"
          />
        </a-modal>
      </section>

      <section v-show="active === 'files'" class="group">
        <h3 class="group-title">
          文件
        </h3>
        <p class="group-sub">
          两个目录都只写在本机，不进云端那份
        </p>
        <div class="card">
          <div class="card-body">
            <div class="row">
              <div class="row-main">
                <div class="row-label">
                  下载目录
                </div>
                <div class="row-hint">
                  文件窗口的下载落到它本机栏正显示的那个目录，不读这里
                </div>
              </div>
              <div class="row-ctl">
                <!-- 没接系统保存对话框：目录由用户写死，下载按钮才知道往哪放 -->
                <a-input
                  v-model:value="settings.downloadDir"
                  size="small"
                  class="ctl-wide"
                  placeholder="D:\Downloads 或 /home/dong/Downloads"
                />
              </div>
            </div>
            <div class="row">
              <div class="row-main">
                <div class="row-label">
                  日志目录
                </div>
                <div class="row-hint">
                  留空用应用数据目录下的 logs。抄的是原始字节，含被回显的口令
                </div>
              </div>
              <div class="row-ctl">
                <!-- 留空 = 应用数据目录下的 logs。目录由后端创建，这里只存一个绝对路径 -->
                <a-input
                  v-model:value="settings.logDir"
                  size="small"
                  class="ctl-wide"
                  placeholder="留空则用应用数据目录下的 logs"
                />
              </div>
            </div>
          </div>
        </div>
      </section>

      <section v-show="active === 'security'" class="group">
        <h3 class="group-title">
          安全
        </h3>
        <p class="group-sub">
          锁定后禁止新建连接与凭据读写，已建立的会话不受影响
        </p>
        <div class="card">
          <div class="card-body">
            <div class="row">
              <div class="row-main">
                <div class="row-label">
                  自动锁定
                </div>
                <div class="row-hint">
                  {{ lock.hasPin ? '空闲够久就把界面盖住，解锁要输 PIN' : '得先设置解锁 PIN，这一项才可选' }}
                </div>
              </div>
              <div class="row-ctl">
                <a-select
                  :value="lock.idleMinutes"
                  size="small"
                  class="ctl-mid"
                  :options="IDLE_OPTIONS"
                  :disabled="!lock.hasPin"
                  @update:value="(v) => lock.setIdleMinutes(Number(v))"
                />
              </div>
            </div>
            <div class="row">
              <div class="row-main">
                <div class="row-label">
                  解锁 PIN
                </div>
                <div class="row-hint">
                  至少 4 位；长度与「锁着不许改锁」都在 Rust 侧再判一遍
                </div>
              </div>
              <div class="row-ctl">
                <span class="state">
                  <i class="dot" :class="{ ok: lock.hasPin }" />
                  {{ lock.hasPin ? '已启用' : '未启用' }}
                </span>
                <a-button size="small" type="text" @click="pinOpen = !pinOpen">
                  {{ pinOpen ? '收起' : (lock.hasPin ? '更换' : '设置') }}
                </a-button>
              </div>
            </div>
            <div v-if="pinOpen" class="row-edit">
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
                class="edit-second"
              />
              <div class="edit-actions">
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
                  <a-button size="small" type="text" danger>
                    关闭应用锁
                  </a-button>
                </a-popconfirm>
              </div>
            </div>
          </div>
        </div>
      </section>

      <section v-show="active === 'sync'" class="group">
        <h3 class="group-title">
          同步
        </h3>
        <p class="group-sub">
          主机列表（含账号口令）整个加密成一份，存进云上那家的一个片段里
        </p>
        <div class="note-toggle">
          <a-button size="small" type="link" class="plain-link" @click="noteOpen = !noteOpen">
            {{ noteOpen ? '收起说明' : '这份东西有多敏感？' }}
          </a-button>
        </div>
        <p v-if="noteOpen" class="note">
          云上躺着的是密文，没有同步串谁也解不开；但<strong>片段地址有可能被别人翻到</strong>，
          所以同步串泄露等于整本主机库连口令一起交出去，别把它和地址存在同一个地方。
          第二台设备要把「同步串」和「片段 id」两样都填进去：id 是云端随机生成的，
          从同步串推不出来，少一样就找不到云上那一份。
        </p>
        <div class="card">
          <div class="card-body">
            <div class="row">
              <div class="row-main">
                <div class="row-label">
                  存到哪一家
                </div>
                <div class="row-hint">
                  两家各存一份配置，切走再切回来都原样还在
                </div>
              </div>
              <div class="row-ctl">
                <a-segmented
                  :value="sync.status?.provider ?? 'github'"
                  size="small"
                  :options="PROVIDERS"
                  @change="(v) => void switchProvider(v as ProviderKind)"
                />
              </div>
            </div>
          </div>
        </div>
        <div class="card">
          <div class="card-head">
            凭据
          </div>
          <div class="card-body">
            <div class="row">
              <div class="row-main">
                <div class="row-label">
                  同步串
                </div>
                <div class="row-hint">
                  32 个字符；丢了，云上那一份就永远解不开
                </div>
              </div>
              <div class="row-ctl">
                <span class="state">
                  <i class="dot" :class="{ ok: sync.status?.has_sync_str }" />
                  {{ sync.status?.has_sync_str ? '已配置' : '未配置' }}
                </span>
                <a-button size="small" type="text" @click="editStr = !editStr">
                  {{ editStr ? '收起' : (sync.status?.has_sync_str ? '更换' : '设置') }}
                </a-button>
              </div>
            </div>
            <div v-if="editStr" class="row-edit">
              <a-input-password
                v-model:value="syncStr"
                :maxlength="64"
                autocomplete="new-password"
                placeholder="贴入同步串以保存或更换"
              />
              <div class="edit-actions">
                <a-button type="primary" size="small" :disabled="!syncStr.trim()" @click="void saveSyncStr()">
                  保存
                </a-button>
                <a-button size="small" type="text" :disabled="!syncStr.trim()" @click="void copySyncStr()">
                  复制
                </a-button>
                <a-button size="small" type="text" @click="void generateSyncStr()">
                  生成新的
                </a-button>
                <a-popconfirm
                  v-if="sync.status?.has_sync_str"
                  title="忘记之后本机再也解不开云上那一份，云端的数据不动。确定吗？"
                  ok-text="忘记"
                  cancel-text="取消"
                  @confirm="void forgetSyncStr()"
                >
                  <a-button size="small" type="text" danger>
                    忘记
                  </a-button>
                </a-popconfirm>
              </div>
            </div>
            <div class="row">
              <div class="row-main">
                <div class="row-label">
                  云端令牌
                </div>
                <div class="row-hint">
                  只用来读写云上那个片段，要那个账号有相应的片段权限
                </div>
              </div>
              <div class="row-ctl">
                <span class="state">
                  <i class="dot" :class="{ ok: sync.status?.has_token }" />
                  {{ sync.status?.has_token ? '已配置' : '未配置' }}
                </span>
                <a-button size="small" type="text" @click="editToken = !editToken">
                  {{ editToken ? '收起' : (sync.status?.has_token ? '更换' : '设置') }}
                </a-button>
              </div>
            </div>
            <div v-if="editToken" class="row-edit">
              <a-input-password
                v-model:value="syncToken"
                :maxlength="256"
                autocomplete="off"
                placeholder="贴入令牌以保存或更换"
              />
              <div class="edit-actions">
                <a-button type="primary" size="small" :disabled="!syncToken.trim()" @click="void saveSyncToken()">
                  保存
                </a-button>
                <a-popconfirm
                  v-if="sync.status?.has_token"
                  title="忘记令牌之后就无法再推新数据上云，本机和云端都已存的东西都还在。确定吗？"
                  ok-text="忘记"
                  cancel-text="取消"
                  @confirm="void forgetSyncToken()"
                >
                  <a-button size="small" type="text" danger>
                    忘记
                  </a-button>
                </a-popconfirm>
              </div>
            </div>
            <div class="row">
              <div class="row-main">
                <div class="row-label">
                  云端片段 id
                </div>
                <div class="row-hint">
                  {{ sync.status?.fragment_id ? `当前跟着 ${sync.status.fragment_id}` : '还没有跟着任何片段' }}
                </div>
              </div>
              <div class="row-ctl">
                <a-button size="small" type="text" @click="editFragment = !editFragment">
                  {{ editFragment ? '收起' : (sync.status?.fragment_id ? '更换' : '设置') }}
                </a-button>
              </div>
            </div>
            <div v-if="editFragment" class="row-edit">
              <a-input
                v-model:value="syncFragment"
                :maxlength="64"
                placeholder="网页地址里那一串字母数字"
              />
              <div class="edit-actions">
                <a-button type="primary" size="small" :disabled="!syncFragment.trim()" @click="void saveSyncFragment()">
                  保存
                </a-button>
                <a-popconfirm
                  v-if="sync.status?.fragment_id"
                  title="不再跟着任何片段之后，下一次同步会当成「本机还没上过云」来处理。确定吗？"
                  ok-text="取消跟随"
                  cancel-text="再想想"
                  @confirm="void forgetSyncFragment()"
                >
                  <a-button size="small" type="text" danger>
                    不再跟随
                  </a-button>
                </a-popconfirm>
              </div>
            </div>
            <div class="row">
              <div class="row-main">
                <div class="row-label">
                  把私钥一起同步
                </div>
                <div class="row-hint">
                  默认关。私钥连同它的口令封进同一份密文；不勾的另一面是换设备后私钥认证的主机要手工重导一次
                </div>
              </div>
              <div class="row-ctl">
                <a-switch
                  :checked="sync.status?.include_private_keys ?? false"
                  @change="(v: boolean) => void savePrivateKeys(v)"
                />
              </div>
            </div>
          </div>
        </div>
        <div class="card">
          <div class="card-body">
            <div class="row">
              <div class="row-main">
                <div class="row-label">
                  立即同步
                </div>
                <div class="row-hint">
                  本机共 {{ sync.status?.hosts ?? 0 }} 台；一次大约要等十秒
                </div>
              </div>
              <div class="row-ctl">
                <a-popconfirm
                  v-if="sync.last?.outcome === 'recreate_required'"
                  title="在云上重建一个新片段，用本机这份当起点。另一台设备仍然跟着老片段，得有人把新 id 抄过去。"
                  ok-text="重建"
                  cancel-text="取消"
                  @confirm="void runSync(true)"
                >
                  <a-button size="small" danger :loading="sync.busy">
                    重建片段
                  </a-button>
                </a-popconfirm>
                <a-button type="primary" size="small" :loading="sync.busy" @click="void runSync()">
                  同步
                </a-button>
              </div>
            </div>
            <p v-if="sync.error" class="row-note err-text">
              {{ sync.error }}
            </p>
            <p v-else-if="sync.last" class="row-note">
              最近一次：{{ lastText }}<template v-if="lastAgo"> · {{ lastAgo.replace('上次同步：', '') }}</template>
            </p>
          </div>
        </div>
      </section>

      <section v-show="active === 'credentials'" class="group">
        <h3 class="group-title">
          凭据
        </h3>
        <p class="group-sub">
          私钥与主机密钥都存在本机加密库里
        </p>
        <div class="card">
          <div class="card-head">
            私钥
            <div class="head-ctl">
              <a-button size="small" type="text" @click="importOpen = !importOpen">
                {{ importOpen ? '收起' : '导入私钥' }}
              </a-button>
            </div>
          </div>
          <div class="card-body">
            <p v-if="hosts.identities.length === 0" class="empty">
              还没有导入过私钥
            </p>
            <div v-for="item in hosts.identities" :key="item.id" class="row">
              <div class="row-main">
                <div class="row-label">
                  {{ item.label }}
                </div>
                <div class="row-hint">
                  {{ item.is_ppk ? 'PuTTY .ppk 格式' : 'PEM 文本' }} · {{ item.has_passphrase ? '带口令' : '无口令' }}
                </div>
              </div>
              <div class="row-ctl">
                <a-button size="small" type="text" danger @click="void dropIdentity(item.id, item.label)">
                  删除
                </a-button>
              </div>
            </div>
            <div v-if="importOpen" class="row-edit">
              <a-input v-model:value="identityLabel" size="small" placeholder="名称，例如 bastion-key" />
              <a-textarea
                v-model:value="privateKey"
                :rows="4"
                spellcheck="false"
                class="edit-second"
                placeholder="私钥内容（PEM 或 .ppk 文本）"
              />
              <a-input-password
                v-model:value="identityPassphrase"
                size="small"
                autocomplete="off"
                class="edit-second"
                placeholder="私钥口令（没有就留空）"
              />
              <div class="edit-actions">
                <a-button type="primary" size="small" :loading="addingIdentity" @click="void addIdentity()">
                  导入
                </a-button>
              </div>
            </div>
          </div>
        </div>
        <div class="card">
          <div class="card-head">
            主机密钥
            <div class="head-ctl">
              <a-button size="small" type="text" @click="void hostkeys.refresh()">
                刷新
              </a-button>
              <a-button size="small" type="text" @click="void importSystem()">
                导入系统 known_hosts
              </a-button>
            </div>
          </div>
          <div class="card-body">
            <p v-if="hostkeys.known.length === 0" class="empty">
              还没有任何已知主机
            </p>
            <div v-for="item in hostkeys.known" :key="`${item.pattern}-${item.algorithm}`" class="row">
              <div class="row-main">
                <div class="row-label mono">
                  {{ item.pattern }}
                </div>
                <div class="row-hint">
                  {{ item.algorithm }} · {{ formatTime(item.added_at_ms) }} · {{ item.source === 'imported' ? '系统导入' : '本应用确认' }}
                </div>
              </div>
              <div class="row-ctl">
                <a-popconfirm title="忘记这台主机的密钥？下次连接会重新确认。" ok-text="忘记" cancel-text="取消" @confirm="void forget(item)">
                  <a-button size="small" type="text" danger>
                    忘记
                  </a-button>
                </a-popconfirm>
              </div>
            </div>
          </div>
        </div>
      </section>
    </div>
  </div>
</template>

<style scoped>
/* 左导航 + 右内容两栏。抽屉 body 的内边距已经在 `a-drawer` 上清掉，
   不然导航条贴不到抽屉左边缘，会浮在一圈留白里。 */
.settings {
  display: flex;
  height: 100%;
  min-height: 0;
}

.rail {
  flex: none;
  width: 116px;
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding: 10px 8px;
  border-right: 1px solid var(--line);
  background: var(--bg-surface);
}

.rail-item {
  display: flex;
  align-items: center;
  gap: 8px;
  height: 32px;
  padding: 0 9px;
  border: 0;
  border-radius: var(--radius);
  background: none;
  color: var(--text-2);
  font: inherit;
  font-size: 13px;
  text-align: left;
  cursor: pointer;
  transition: background var(--tap), color var(--tap);
}

.rail-item:hover {
  background: var(--bg-hover);
  color: var(--text-1);
}

.rail-item.on {
  background: var(--bg-selected);
  color: var(--text-1);
  font-weight: 500;
}

.pane {
  flex: 1;
  min-width: 0;
  overflow-y: auto;
  padding: 16px 20px 28px;
}

.group-title {
  margin: 0;
  font-size: 15px;
  font-weight: 600;
  color: var(--text-1);
}

.group-sub {
  margin: 3px 0 0;
  font-size: 12px;
  color: var(--text-3);
}

/* 节标题和内容之间的距离只在这里给一次：卡片和「为什么要注意」那段展开共用同一个起头 */
.group > .card,
.group > .note-toggle,
.group > .note {
  margin-top: 14px;
}

.group > .note-toggle {
  margin-top: 6px;
}

.card + .card {
  margin-top: 14px;
}

/* 一张表面卡装同一类设置，行与行之间一根线：
   老版本靠 a-divider 拉一根通栏线切八段，段内又全是撑满宽度的竖排控件，
   看起来既碎又长。现在一节一屏放得下，卡内一行一件事。 */
.card {
  border: 1px solid var(--line);
  border-radius: var(--radius-lg);
  background: var(--bg-raised);
  overflow: hidden;
}

.card-head {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 10px 8px 14px;
  border-bottom: 1px solid var(--line);
  font-size: 13px;
  font-weight: 600;
  color: var(--text-2);
}

.head-ctl {
  margin-left: auto;
  display: flex;
  align-items: center;
  gap: 2px;
  font-weight: 400;
}

.row {
  display: flex;
  align-items: center;
  gap: 16px;
  padding: 12px 14px;
}

/* 分隔线只由「上一行」给，空态和展开面板也走同一条规则，
   免得四种元素各写一遍 border 再互相叠出双线 */
.card-body > * + * {
  border-top: 1px solid var(--line);
}

.row-main {
  flex: 1;
  min-width: 0;
}

.row-label {
  font-size: 13px;
  color: var(--text-1);
}

/* 说明永远跟在标题下面一行，不挂到控件右边：420 那一档它被挤断过行 */
.row-hint {
  margin-top: 2px;
  font-size: 12px;
  color: var(--text-3);
}

.row-ctl {
  flex: none;
  display: flex;
  align-items: center;
  gap: 8px;
}

/* 展开出来的编辑器：底色压深一档，让它和上面那行「状态」明显是两块 */
.row-edit {
  padding: 12px 14px;
  background: var(--bg-surface);
}

.edit-second {
  margin-top: 8px;
}

.edit-actions {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 10px;
}

.empty {
  margin: 0;
  padding: 14px;
  font-size: 12px;
  color: var(--text-3);
}

.row-note {
  margin: 0;
  padding: 0 14px 12px;
  font-size: 12px;
  color: var(--text-3);
}

.note {
  margin: 6px 0 0;
  padding: 10px 12px;
  border: 1px solid var(--line);
  border-left: 2px solid var(--warn);
  border-radius: var(--radius);
  background: var(--bg-surface);
  font-size: 12px;
  line-height: 1.7;
  color: var(--text-2);
}

/* 状态只有一处用大字：这一行的圆点替掉了原来满屏的 a-tag */
.state {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  color: var(--text-2);
}

.dot {
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: var(--text-3);
}

.dot.ok {
  background: var(--ok);
}

.err-text {
  color: var(--err);
}

.swatch {
  width: 20px;
  height: 20px;
  padding: 0;
  border: 1px solid var(--line-strong);
  border-radius: var(--radius-sm);
  cursor: pointer;
  transition: box-shadow var(--tap), transform var(--tap);
}

.swatch:hover {
  transform: translateY(-1px);
}

/* 选中态用「底色环 + 前景环」两道：只描一圈边的话，深色主题下最亮那颗蓝和
   紫色根本分不出谁被选中了 */
.swatch.on {
  box-shadow: 0 0 0 2px var(--bg-raised), 0 0 0 4px var(--text-1);
}

.font-size {
  width: 64px;
}

.ctl-mid {
  width: 140px;
}

.ctl-wide {
  width: 280px;
}

.plain-link {
  height: auto;
  padding: 0;
  font-size: 12px;
}

.mono {
  font-family: ui-monospace, Menlo, Consolas, monospace;
  font-size: 12px;
}

.hl-row {
  gap: 8px;
}

.hl-pattern {
  flex: 1;
  min-width: 140px;
  max-width: 240px;
}

/* 原生取色框默认带一圈内边距，压成和旁边小控件同高 */
.hl-color {
  width: 34px;
  height: 24px;
  padding: 0 2px;
  border: 1px solid var(--line-strong);
  border-radius: var(--radius-sm);
  background: none;
  cursor: pointer;
}
</style>
