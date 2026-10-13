<script setup lang="ts">
import { computed, reactive, ref, watch } from 'vue'
import { message } from 'ant-design-vue'

import type { AuthMode, HostDraft, HostId, HostInfo, Protocol, TerminalEncoding } from '@/lib/ipc/bindings'
import { useHostsStore } from '@/stores/hosts'

const props = defineProps<{ open: boolean, host: HostInfo | null }>()
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

const submitting = ref(false)
const needsIdentity = computed(() => form.authMode === 'public_key')
const needsPassword = computed(() => form.authMode === 'password' || form.authMode === 'keyboard_interactive')

// 每次打开都从选中的主机重新灌一遍，避免上一次编辑的残留
watch(() => [props.open, props.host] as const, ([open]) => {
  if (!open) return
  const host = props.host
  form.label = host?.label ?? ''
  form.group = host?.group ?? ''
  form.favorite = host?.favorite ?? false
  form.protocol = host?.protocol ?? 'ssh'
  form.host = host?.endpoint.host ?? ''
  form.port = host?.endpoint.port ?? 22
  form.username = host?.endpoint.username ?? ''
  form.authMode = host?.auth_mode ?? 'password'
  // 身份引用是 auth 带的，不是 auth_mode 能推出来的：PublicKey 一定配 Identity
  form.identityId = host?.auth.kind === 'identity' ? host.auth.identity_id : ''
  form.keepaliveSecs = host?.keepalive_secs ?? 30
  form.encoding = host?.encoding ?? 'utf8'
  form.password = ''
}, { immediate: true })

function valid(): string | null {
  if (!form.label.trim()) return '请填写名称'
  if (form.protocol === 'ssh' && !form.host.trim()) return '请填写主机地址'
  if (form.protocol === 'ssh' && !form.username.trim()) return '请填写用户名'
  if (form.port < 1 || form.port > 65535) return '端口要在 1-65535 之间'
  if (needsIdentity.value && !form.identityId) return '私钥认证要选一把密钥'
  return null
}

async function submit(): Promise<void> {
  const problem = valid()
  if (problem) {
    messageApi.warning(problem)
    return
  }
  const draft: HostDraft = {
    id: props.host?.id ?? null,
    label: form.label.trim(),
    group: form.group.trim() ? form.group.trim() : null,
    tags: props.host?.tags ?? [],
    favorite: form.favorite,
    protocol: form.protocol,
    endpoint: {
      host: form.host.trim(),
      port: form.port === 22 ? null : form.port,
      username: form.username.trim(),
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
</script>

<template>
  <a-modal
    :open="open"
    :title="host ? `编辑 ${host.label}` : '新建主机'"
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
        <a-input v-model:value="form.group" placeholder="留空表示未分组" />
      </a-form-item>
      <a-form-item label="协议">
        <a-select v-model:value="form.protocol">
          <a-select-option value="ssh">SSH</a-select-option>
          <a-select-option value="local">本机终端</a-select-option>
        </a-select>
      </a-form-item>
      <template v-if="form.protocol === 'ssh'">
        <a-form-item label="主机">
          <a-input v-model:value="form.host" placeholder="10.0.0.12" />
        </a-form-item>
        <a-form-item label="端口">
          <a-input-number v-model:value="form.port" :min="1" :max="65535" style="width: 120px" />
        </a-form-item>
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
          <!-- 口令永不出现在读取方向上，所以这里没有回填，只有覆盖 -->
          <a-input-password v-model:value="form.password" autocomplete="new-password" />
        </a-form-item>
        <a-form-item label="保活间隔（秒）">
          <a-input-number v-model:value="form.keepaliveSecs" :min="0" :max="600" style="width: 120px" />
        </a-form-item>
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
