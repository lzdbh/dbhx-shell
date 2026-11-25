<script setup lang="ts">
import { computed, ref, watch } from 'vue'

import type { Ask } from '@/stores/hostkeys'
import type { LiveEndpoint } from '@/lib/ipc/bindings'
import { sshFingerprint } from '@/lib/fingerprint'
import { useHostkeysStore } from '@/stores/hostkeys'

const hostkeys = useHostkeysStore()

// 三个分支各自收窄成一个可空 computed：模板里靠 v-if 收窄联合类型不可靠，
// 而 `keyAsk.endpoint` 这类访问在类型上就是确定的。
const keyAsk = computed(() => { const a = hostkeys.current; return a?.kind === 'host_key' ? a : null })
const conflictAsk = computed(() => { const a = hostkeys.current; return a?.kind === 'conflict' ? a : null })
const passphraseAsk = computed(() => { const a = hostkeys.current; return a?.kind === 'passphrase' ? a : null })
const transferAsk = computed(() => { const a = hostkeys.current; return a?.kind === 'transfer' ? a : null })

const presentedFp = ref('')
const storedFp = ref('')
const passphrase = ref('')

/** 指纹是算出来的，异步；算出来之前先显示 base64 原文，别让对话框空着。 */
watch(() => hostkeys.current, async (next: Ask | null) => {
  presentedFp.value = ''
  storedFp.value = ''
  passphrase.value = ''
  if (!next || next.kind === 'passphrase' || next.kind === 'transfer') return
  presentedFp.value = await sshFingerprint(next.presented.base64_key)
  if (next.kind === 'conflict') storedFp.value = await sshFingerprint(next.stored.base64_key)
}, { immediate: true })

function endpointText(endpoint: LiveEndpoint): string {
  return `${endpoint.username}@${endpoint.host}:${endpoint.port}`
}

async function submitPassphrase(): Promise<void> {
  const text = passphrase.value
  passphrase.value = ''
  await hostkeys.answerPassphrase(text)
}

async function skipPassphrase(): Promise<void> {
  passphrase.value = ''
  await hostkeys.skipPassphrase()
}
</script>

<template>
  <!-- 主机密钥确认 -->
  <a-modal
    v-if="keyAsk"
    :open="true"
    title="首次连接该主机"
    ok-text="信任并保存"
    cancel-text="拒绝"
    :mask-closable="false"
    @ok="void hostkeys.confirmKey()"
    @cancel="void hostkeys.rejectKey()"
  >
    <a-alert
      message="请核对指纹"
      description="把它和运维提供的记录对一遍。信任之后这台主机的密钥写进 known_hosts，之后不再询问；拒绝则本次连接终止。"
      type="info"
      show-icon
    />
    <a-descriptions :column="1" size="small" bordered style="margin-top: 12px">
      <a-descriptions-item label="目标">{{ endpointText(keyAsk.endpoint) }}</a-descriptions-item>
      <a-descriptions-item label="算法">{{ keyAsk.presented.algorithm }}</a-descriptions-item>
      <a-descriptions-item label="指纹">
        <span class="mono">{{ presentedFp || keyAsk.presented.base64_key }}</span>
      </a-descriptions-item>
    </a-descriptions>
  </a-modal>

  <!-- 中间人告警：只有关闭，不存在「仍然连接」 -->
  <a-modal
    v-else-if="conflictAsk"
    :open="true"
    title="主机密钥变了"
    :mask-closable="false"
    @cancel="hostkeys.dismissConflict()"
  >
    <template #footer>
      <a-button type="primary" @click="hostkeys.dismissConflict()">
        关闭
      </a-button>
    </template>
    <a-alert
      message="连接已被阻断"
      description="这台主机现在出示的密钥与本机保存的不是同一把。最常见的原因是服务器重装过，也可能是中间人。请先与运维核对指纹，确认后在设置里忘记旧密钥再重连。"
      type="error"
      show-icon
    />
    <a-descriptions :column="1" size="small" bordered style="margin-top: 12px">
      <a-descriptions-item label="目标">{{ endpointText(conflictAsk.endpoint) }}</a-descriptions-item>
      <a-descriptions-item label="已保存">
        <span class="mono">{{ storedFp || conflictAsk.stored.base64_key }}</span>
      </a-descriptions-item>
      <a-descriptions-item label="本次出示">
        <span class="mono">{{ presentedFp || conflictAsk.presented.base64_key }}</span>
      </a-descriptions-item>
    </a-descriptions>
  </a-modal>

  <!-- 传输冲突：选择对整批生效 -->
  <a-modal
    v-else-if="transferAsk"
    :open="true"
    title="目标位置有同名文件"
    :mask-closable="false"
    @cancel="void hostkeys.cancelTransferConflict()"
  >
    <a-alert
      :message="`${transferAsk.count} 个文件已存在`"
      description="选一种处理方式，对整批生效；重命名会在扩展名前加 (1)、(2)…"
      type="warning"
      show-icon
    />
    <ul class="samples">
      <li v-for="item in transferAsk.samples" :key="item" class="mono">
        {{ item }}
      </li>
      <li v-if="transferAsk.count > transferAsk.samples.length" class="tip">
        ……还有 {{ transferAsk.count - transferAsk.samples.length }} 个
      </li>
    </ul>
    <template #footer>
      <a-button @click="void hostkeys.cancelTransferConflict()">
        取消传输
      </a-button>
      <a-button @click="void hostkeys.answerTransferConflict('skip')">
        全部跳过
      </a-button>
      <a-button @click="void hostkeys.answerTransferConflict('rename')">
        全部重命名
      </a-button>
      <a-button type="primary" danger @click="void hostkeys.answerTransferConflict('overwrite')">
        全部覆盖
      </a-button>
    </template>
  </a-modal>

  <!-- 私钥口令 -->
  <a-modal
    v-else-if="passphraseAsk"
    :open="true"
    :title="`解锁私钥：${passphraseAsk.identityLabel}`"
    ok-text="确定"
    cancel-text="跳过"
    :mask-closable="false"
    @ok="void submitPassphrase()"
    @cancel="void skipPassphrase()"
  >
    <a-input-password
      v-model:value="passphrase"
      placeholder="私钥口令"
      autocomplete="off"
      @press-enter="void submitPassphrase()"
    />
    <p class="tip">口令只用于本次连接，不会写进日志，也不会回传给渲染进程的其它部分。</p>
  </a-modal>
</template>

<style scoped>
.mono {
  font-family: ui-monospace, Menlo, Consolas, monospace;
  word-break: break-all;
}

.tip {
  margin: 8px 0 0;
  font-size: 12px;
  color: var(--text-2);
}

.samples {
  margin: 10px 0 0;
  padding-left: 18px;
  max-height: 180px;
  overflow: auto;
}
</style>
