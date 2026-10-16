<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { message } from 'ant-design-vue'

import { formatTime } from '@/lib/format'
import { clipboardApi } from '@/lib/ipc/client'
import type { ProviderKind } from '@/lib/ipc/bindings'
import { summarize } from '@/lib/syncSummary'
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
const PROVIDER_OPTIONS: { value: ProviderKind, label: string }[] = [
  { value: 'github', label: 'GitHub Gist' },
  { value: 'gitee', label: 'Gitee 代码片段' },
]

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

/**
 * 同步串与令牌只在这一次提交里活着。保存成功就清空输入框，
 * 界面之后只显示「已配置」——想改只能重新贴一遍，这是有意的。
 */
const syncStr = ref('')
const syncToken = ref('')
const syncFragment = ref('')

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

async function saveSyncStr(): Promise<void> {
  if (await sync.setStr(syncStr.value)) {
    // 清掉明文：界面上之后只剩「已配置」三个字，值不在渲染层留副本
    syncStr.value = ''
    messageApi.success('已保存同步串')
  } else {
    messageApi.error(sync.error ?? '保存失败')
  }
}

async function forgetSyncStr(): Promise<void> {
  if (await sync.setStr(null)) messageApi.success('已忘记本机存的同步串，云上那一份没动')
  else messageApi.error(sync.error ?? '操作失败')
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
    messageApi.success('已保存云端令牌')
  } else {
    messageApi.error(sync.error ?? '保存失败')
  }
}

async function forgetSyncToken(): Promise<void> {
  if (await sync.setToken(null)) messageApi.success('已忘记令牌')
  else messageApi.error(sync.error ?? '操作失败')
}

async function saveSyncFragment(): Promise<void> {
  if (await sync.setFragment(syncFragment.value)) {
    syncFragment.value = ''
    messageApi.success('已记下片段 id')
  } else {
    messageApi.error(sync.error ?? '保存失败')
  }
}

async function forgetSyncFragment(): Promise<void> {
  if (await sync.setFragment(null)) messageApi.success('本机不再跟着任何片段')
  else messageApi.error(sync.error ?? '操作失败')
}

/**
 * 换目的地。切之前先把「有没有跟着片段」记下来：后端在换家那一刻会连同基线
 * 一起清掉坐标，界面必须把这件事说出来，否则用户以为切回去 id 还在。
 */
async function switchProvider(provider: ProviderKind): Promise<void> {
  const dropped = sync.status?.fragment_id
  if (!(await sync.setProvider(provider))) {
    messageApi.error(sync.error ?? '切换失败')
    return
  }
  if (dropped) messageApi.warning(`已切到另一家，原来跟着的片段 ${dropped} 已经清空，要重新填 id`)
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
        云端同步
      </a-divider>
      <p class="hint-line">
        把主机列表（含账号口令）整个加密成一份，存进所选那家的一个片段里。
        云上躺着的是密文，没有同步串谁也解不开；但<strong>片段地址有可能被别人翻到</strong>，
        所以同步串泄露等于整本主机库连口令一起交出去，别把它和地址存在同一个地方。
        第二台设备要把「同步串」和「片段 id」两样都填进去：
        id 是云端随机生成的，从同步串推不出来，少一样就找不到云上那一份。
      </p>
      <a-form-item label="存到哪一家">
        <a-select
          :value="sync.status?.provider ?? 'github'"
          style="width: 200px"
          :options="PROVIDER_OPTIONS"
          @update:value="(v) => void switchProvider(v as ProviderKind)"
        />
        <span class="dim">换一家会清空当前跟着的片段 id。</span>
      </a-form-item>

      <a-form-item label="同步串">
        <a-space direction="vertical" style="width: 100%">
          <a-space>
            <a-tag :color="sync.status?.has_sync_str ? 'green' : 'default'">
              {{ sync.status?.has_sync_str ? '已配置' : '未配置' }}
            </a-tag>
            <span class="dim">32 个字符；丢了，云上那一份就永远解不开。</span>
          </a-space>
          <a-space wrap>
            <a-input-password
              v-model:value="syncStr"
              :maxlength="64"
              autocomplete="new-password"
              class="sync-input"
              placeholder="贴入同步串以保存或更换"
            />
            <a-button size="small" :disabled="!syncStr.trim()" @click="void saveSyncStr()">
              保存
            </a-button>
            <a-button size="small" :disabled="!syncStr.trim()" @click="void copySyncStr()">
              复制
            </a-button>
            <a-button size="small" @click="void generateSyncStr()">
              生成新的
            </a-button>
            <a-popconfirm
              v-if="sync.status?.has_sync_str"
              title="忘记之后本机再也解不开云上那一份，云端的数据不动。确定吗？"
              ok-text="忘记"
              cancel-text="取消"
              @confirm="void forgetSyncStr()"
            >
              <a-button size="small" danger>
                忘记
              </a-button>
            </a-popconfirm>
          </a-space>
        </a-space>
      </a-form-item>

      <a-form-item label="云端令牌">
        <a-space direction="vertical" style="width: 100%">
          <a-space>
            <a-tag :color="sync.status?.has_token ? 'green' : 'default'">
              {{ sync.status?.has_token ? '已配置' : '未配置' }}
            </a-tag>
            <span class="dim">只用来读写云上那个片段，要那个账号有相应的片段权限。</span>
          </a-space>
          <a-space wrap>
            <a-input-password
              v-model:value="syncToken"
              :maxlength="256"
              autocomplete="off"
              class="sync-input"
              placeholder="贴入令牌以保存或更换"
            />
            <a-button size="small" :disabled="!syncToken.trim()" @click="void saveSyncToken()">
              保存
            </a-button>
            <a-popconfirm
              v-if="sync.status?.has_token"
              title="忘记令牌之后就无法再推新数据上云，本机和云端都已存的东西都还在。确定吗？"
              ok-text="忘记"
              cancel-text="取消"
              @confirm="void forgetSyncToken()"
            >
              <a-button size="small" danger>
                忘记
              </a-button>
            </a-popconfirm>
          </a-space>
        </a-space>
      </a-form-item>

      <a-form-item label="云端片段 id">
        <a-space direction="vertical" style="width: 100%">
          <a-space>
            <span class="mono">{{ sync.status?.fragment_id ? `当前跟着 ${sync.status.fragment_id}` : '还没有跟着任何片段' }}</span>
          </a-space>
          <a-space wrap>
            <a-input
              v-model:value="syncFragment"
              :maxlength="64"
              class="sync-input"
              placeholder="网页地址里那一串字母数字"
            />
            <a-button size="small" :disabled="!syncFragment.trim()" @click="void saveSyncFragment()">
              保存
            </a-button>
            <a-popconfirm
              v-if="sync.status?.fragment_id"
              title="不再跟着任何片段之后，下一次同步会当成「本机还没上过云」来处理。确定吗？"
              ok-text="取消跟随"
              cancel-text="再想想"
              @confirm="void forgetSyncFragment()"
            >
              <a-button size="small" danger>
                不再跟随
              </a-button>
            </a-popconfirm>
          </a-space>
        </a-space>
      </a-form-item>

      <a-form-item label="私钥">
        <a-space>
          <a-checkbox
            :checked="sync.status?.include_private_keys ?? false"
            @update:checked="(v: boolean) => void savePrivateKeys(v)"
          >
            把私钥一起同步
          </a-checkbox>
          <a-tooltip>
            <template #title>
              默认关。私钥连同它的口令一起封进同一份密文里，只有拿着同步串的人解得开。
              不勾的另一面是：换设备之后认证方式选「私钥」的主机要手工重新导入一次。
            </template>
            <span class="dim">这个开关存在本机库里，重装也不会悄悄回到默认值。</span>
          </a-tooltip>
        </a-space>
      </a-form-item>

      <a-form-item label="同步">
        <a-space wrap>
          <a-button type="primary" :loading="sync.busy" @click="void runSync()">
            立即同步
          </a-button>
          <a-popconfirm
            v-if="sync.last?.outcome === 'recreate_required'"
            title="在云上重建一个新片段，用本机这份当起点。另一台设备仍然跟着老片段，得有人把新 id 抄过去。"
            ok-text="重建"
            cancel-text="取消"
            @confirm="void runSync(true)"
          >
            <a-button danger :loading="sync.busy">
              重建片段
            </a-button>
          </a-popconfirm>
          <span class="dim">本机共 {{ sync.status?.hosts ?? 0 }} 台；一次大约要等十秒。</span>
        </a-space>
      </a-form-item>
      <p v-if="sync.error" class="hint-line sync-error">
        {{ sync.error }}
      </p>

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
  color: var(--text-3);
  margin-left: 6px;
}

.hint-line {
  margin: 0 0 8px;
  font-size: 12px;
  color: var(--text-3);
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

/* 抽屉只有 420 宽：输入框给到一行能放下「输入框 + 保存」为止，其余按钮让它自己换行 */
.sync-input {
  width: 200px;
}

.sync-error {
  color: var(--err);
}

/* 原生取色框默认带一圈内边距，压成和旁边小控件同高 */
.hl-color {
  width: 34px;
  height: 24px;
  padding: 0 2px;
  border: 1px solid var(--line-strong);
  border-radius: 4px;
  background: none;
  cursor: pointer;
}
</style>
