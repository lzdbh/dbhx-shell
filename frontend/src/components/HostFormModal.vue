<script setup lang="ts">
import { computed, reactive, ref, watch } from 'vue'
import { message } from 'ant-design-vue'

import type { AuthMode, HostDraft, HostId, HostInfo, Protocol, TerminalEncoding } from '@/lib/ipc/bindings'
import { vaultApi } from '@/lib/ipc/client'
import { UNGROUPED, useHostsStore } from '@/stores/hosts'

const props = defineProps<{ open: boolean, host: HostInfo | null, presetGroup?: string | null, seed?: HostInfo | null }>()
const emit = defineEmits<{ 'update:open': [boolean], saved: [HostId] }>()

const hosts = useHostsStore()
const [messageApi, holder] = message.useMessage()

const AUTH_OPTIONS: { value: AuthMode, label: string }[] = [
  { value: 'password', label: '密码' },
  { value: 'public_key', label: '私钥' },
  { value: 'agent', label: 'ssh-agent' },
  { value: 'keyboard_interactive', label: '交互问答（堡垒机）' },
]

const form = reactive({
  label: '',
  group: '',
  favorite: false,
  protocol: 'ssh' as Protocol,
  host: '',
  port: 22,
  username: '',
  authMode: 'password' as AuthMode,
  /** a-select 不接受 null，用空串表示「还没选」，提交时再翻回 null。 */
  identityId: '',
  keepaliveSecs: 30,
  encoding: 'utf8' as TerminalEncoding,
  /** 明文只在提交这一瞬间存在；为空表示「不改已存口令」。 */
  password: '',
})

/** 各协议的默认端口；存回记录时等于默认值就归一成 null，让后端 `default_port` 兜底。 */
const PROTOCOL_DEFAULT_PORT: Record<Protocol, number | null> = { ssh: 22, telnet: 23, local: null }

const submitting = ref(false)
const revealing = ref(false)
const needsIdentity = computed(() => form.authMode === 'public_key')
const needsPassword = computed(() => form.authMode === 'password' || form.authMode === 'keyboard_interactive')
/** 需要远端地址的协议：SSH 与 telnet 都要 host/port，本机终端不要。 */
const isRemote = computed(() => form.protocol === 'ssh' || form.protocol === 'telnet')

/** 分组的可选项：已有主机的分组 + 手工建的空分组；从分组节点新建时，那个名字也保证在列。 */
const groupOptions = computed(() => {
  const names = hosts.groups.map(group => group.name).filter(name => name !== UNGROUPED)
  if (props.presetGroup) names.push(props.presetGroup)
  return [...new Set(names)].sort((a, b) => a.localeCompare(b, 'zh-Hans-CN')).map(name => ({ value: name, label: name }))
})

// tags 模式能顺手敲个新组名，但表单只留最后一个：保存的是主机身上的一个字段，不是一串标签
watch(() => form.group, (next) => {
  if (Array.isArray(next)) form.group = String(next[next.length - 1] ?? '').trim()
})

/**
 * 表单照着哪台机器灌。
 *
 * 三种来源共用这一条通道：编辑（`host`）、克隆（`seed`）、新建（都没有，
 * 只有 `presetGroup` 带一个分组名）。克隆走 `seed` 而不是直接把记录塞进 `host`，
 * 是为了让「保存时要不要带 id」这一个判断把编辑和克隆彻底分开——带 id 是改，
 * 不带才是新增，混用会当场把源主机覆盖掉。
 */
const source = computed(() => props.host ?? props.seed ?? null)

// 每次打开都从选中的主机重新灌一遍，避免上一次编辑的残留
watch(() => [props.open, props.host, props.seed] as const, ([open]) => {
  if (!open) return
  const host = source.value
  // 克隆出来的名字加「副本」后缀：同名的两台机器在树里根本分不出哪个是哪个
  form.label = host ? (props.host ? host.label : `${host.label} 副本`) : ''
  // 从分组节点「在这里新建主机」进来时，分组名先填好；编辑已有主机仍以记录为准
  form.group = host?.group ?? props.presetGroup ?? ''
  form.favorite = host?.favorite ?? false
  form.protocol = host?.protocol ?? 'ssh'
  form.host = host?.endpoint.host ?? ''
  form.port = host?.endpoint.port ?? PROTOCOL_DEFAULT_PORT[host?.protocol ?? 'ssh'] ?? 22
  form.username = host?.endpoint.username ?? ''
  form.authMode = host?.auth_mode ?? 'password'
  // 身份引用是 auth 带的，不是 auth_mode 能推出来的：PublicKey 一定配 Identity
  form.identityId = host?.auth.kind === 'identity' ? host.auth.identity_id : ''
  form.keepaliveSecs = host?.keepalive_secs ?? 30
  form.encoding = host?.encoding ?? 'utf8'
  form.password = ''
}, { immediate: true })

// 切协议时，若用户没改过端口（还停在旧协议的默认值），就跟着换成新协议的默认端口，
// 免得存一个 telnet 主机却带着 22 这种明显的串味。
watch(() => form.protocol, (next, prev) => {
  const prevDefault = PROTOCOL_DEFAULT_PORT[prev]
  const nextDefault = PROTOCOL_DEFAULT_PORT[next]
  if (nextDefault !== null && prevDefault !== null && form.port === prevDefault) form.port = nextDefault
})

function valid(): string | null {
  if (!form.label.trim()) return '请填写名称'
  if (isRemote.value && !form.host.trim()) return '请填写主机地址'
  // telnet 在流里登录，压根没有「用户名」这一说，别拿 SSH 的必填去卡它
  if (form.protocol === 'ssh' && !form.username.trim()) return '请填写用户名'
  if (isRemote.value && (form.port < 1 || form.port > 65535)) return '端口要在 1-65535 之间'
  if (needsIdentity.value && !form.identityId) return '私钥认证要选一把密钥'
  return null
}

async function submit(): Promise<void> {
  const problem = valid()
  if (problem) {
    messageApi.warning(problem)
    return
  }
  const defaultPort = PROTOCOL_DEFAULT_PORT[form.protocol]
  const draft: HostDraft = {
    id: props.host?.id ?? null,
    label: form.label.trim(),
    group: form.group.trim() ? form.group.trim() : null,
    tags: source.value?.tags ?? [],
    favorite: form.favorite,
    protocol: form.protocol,
    // 系统字段已不在表单里编辑：编辑/克隆沿用原记录，新建归为未知
    os: source.value?.os ?? 'unknown',
    endpoint: {
      host: form.host.trim(),
      // 等于该协议默认端口就存 null，让后端的 default_port 兜底，记录更干净
      port: defaultPort !== null && form.port === defaultPort ? null : form.port,
      username: form.protocol === 'ssh' ? form.username.trim() : '',
    },
    auth_mode: form.authMode,
    identity_id: needsIdentity.value ? form.identityId : null,
    keepalive_secs: form.keepaliveSecs,
    encoding: form.encoding,
  }
  submitting.value = true
  const id = await hosts.save(draft, form.password || null)
  submitting.value = false
  if (!id) {
    messageApi.error(hosts.error ?? '保存失败')
    return
  }
  messageApi.success('已保存')
  emit('saved', id)
  emit('update:open', false)
}

/**
 * 编辑或克隆时按需取回源记录的明文口令填进输入框。
 *
 * 这是唯一一条明文回读入口，而且必须用户显式点击：克隆不会自动把口令复制过去，
 * 因为「复制一条记录」不等于「授权把一把密钥的明文从保险库里搬到另一条记录上」。
 * 点了这个按钮，就是用户自己决定要让新记录带上这份口令。
 */
async function revealStored(): Promise<void> {
  if (!source.value) return
  revealing.value = true
  try {
    form.password = await vaultApi.revealPassword(source.value.id)
  }
  catch (e) {
    messageApi.error(e instanceof Error ? e.message : '读取口令失败')
  }
  finally {
    revealing.value = false
  }
}
</script>

<template>
  <a-modal
    :open="open"
    :title="host ? `编辑 ${host.label}` : seed ? `克隆 ${seed.label}` : '新建主机'"
    :confirm-loading="submitting"
    ok-text="保存"
    cancel-text="取消"
    @update:open="emit('update:open', $event)"
    @ok="void submit()"
  >
    <holder />
    <a-form layout="vertical" @submit.prevent>
      <a-form-item label="名称">
        <a-input v-model:value="form.label" placeholder="生产数据库" />
      </a-form-item>
      <a-form-item label="分组">
        <a-select
          v-model:value="form.group"
          mode="tags"
          :options="groupOptions"
          placeholder="选择分组，可输入新建；留空表示未分组"
          allow-clear
        />
      </a-form-item>
      <a-form-item label="协议">
        <a-select v-model:value="form.protocol">
          <a-select-option value="ssh">SSH</a-select-option>
          <a-select-option value="telnet">Telnet</a-select-option>
          <a-select-option value="local">本机终端</a-select-option>
        </a-select>
      </a-form-item>
      <template v-if="isRemote">
        <a-form-item label="主机">
          <a-input v-model:value="form.host" placeholder="10.0.0.12" />
        </a-form-item>
        <a-form-item label="端口">
          <a-input-number v-model:value="form.port" :min="1" :max="65535" style="width: 120px" />
        </a-form-item>
        <!-- SSH 才有登录用户与认证；telnet 在流里登录，用户名/口令这一套不存在 -->
        <template v-if="form.protocol === 'ssh'">
          <a-form-item label="用户名">
            <a-input v-model:value="form.username" placeholder="root" />
          </a-form-item>
          <a-form-item label="认证方式">
            <a-select v-model:value="form.authMode" :options="AUTH_OPTIONS" />
          </a-form-item>
          <a-form-item v-if="needsIdentity" label="密钥">
            <a-select v-model:value="form.identityId" placeholder="先在设置里导入私钥" allow-clear>
              <a-select-option v-for="item in hosts.identities" :key="item.id" :value="item.id">
                {{ item.label }}
              </a-select-option>
            </a-select>
          </a-form-item>
          <a-form-item v-if="needsPassword" :label="host ? '口令（留空则不修改）' : '口令'">
            <a-input-password v-model:value="form.password" autocomplete="new-password" />
            <!-- 编辑且已存口令时，按需把明文取回填入：口令默认只进不出，这是唯一显式回读入口 -->
            <a-button v-if="source?.has_credential" size="small" type="link" :loading="revealing" @click="void revealStored()">
              取回已存口令
            </a-button>
          </a-form-item>
          <a-form-item label="保活间隔（秒）">
            <a-input-number v-model:value="form.keepaliveSecs" :min="0" :max="600" style="width: 120px" />
          </a-form-item>
        </template>
        <a-form-item label="远端编码">
          <a-select v-model:value="form.encoding">
            <a-select-option value="utf8">UTF-8</a-select-option>
            <a-select-option value="gb18030">GB18030（内网老机器）</a-select-option>
          </a-select>
        </a-form-item>
      </template>
      <a-form-item label="收藏">
        <a-switch v-model:checked="form.favorite" />
      </a-form-item>
    </a-form>
  </a-modal>
</template>
