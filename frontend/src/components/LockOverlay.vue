<script setup lang="ts">
import { nextTick, ref, watch } from 'vue'

import { useLockStore } from '@/stores/lock'

/**
 * 锁屏覆盖层。
 *
 * 它只是一扇门：真正挡住拨号与凭据读写的是后端，这里的职责是让「锁着」这件事
 * 在界面上无可忽视——盖满整窗、吃掉键盘、除了输 PIN 无处可去。
 *
 * 不显示任何 hint 或历史输入，也不区分「PIN 错」和「没设 PIN」之外的细节，
 * 免得这层 UI 反过来给旁人递信息。
 */

const lock = useLockStore()

const pin = ref('')
const error = ref('')
const busy = ref(false)
const field = ref<{ focus: () => void } | null>(null)

async function submit(): Promise<void> {
  if (busy.value || pin.value === '') return
  busy.value = true
  error.value = ''
  const ok = await lock.unlock(pin.value)
  busy.value = false
  // 无论成功失败都擦掉输入：留在框里的 PIN 比屏幕上有没有锁更容易被人瞥到
  pin.value = ''
  if (!ok) {
    error.value = 'PIN 不正确'
    void nextTick(() => { field.value?.focus() })
  }
}

// 每次重新锁上都把光标送回输入框，省一次点击
watch(() => lock.locked, locked => {
  if (locked) {
    error.value = ''
    void nextTick(() => { field.value?.focus() })
  }
})
</script>

<template>
  <div v-if="lock.locked" class="overlay" data-testid="lock-overlay">
    <div class="card">
      <div class="title">应用已锁定</div>
      <p class="sub">
        连接与凭据读写在解锁前都会被拒绝。
      </p>
      <a-input-password
        ref="field"
        v-model:value="pin"
        size="large"
        placeholder="输入解锁 PIN"
        :autofocus="true"
        :status="error ? 'error' : ''"
        @press-enter="void submit()"
      />
      <div v-if="error" class="err">{{ error }}</div>
      <a-button
        type="primary"
        size="large"
        block
        :loading="busy"
        :disabled="pin === ''"
        @click="void submit()"
      >
        解锁
      </a-button>
    </div>
  </div>
</template>

<style scoped>
.overlay {
  position: fixed;
  inset: 0;
  z-index: 2000;
  display: flex;
  align-items: center;
  justify-content: center;
  /* 不透出底下内容：半透明会让窗口里的地址、用户名在锁定态仍被看到 */
  background: #1f1f1f;
  color: #eaeaea;
}

.card {
  width: 320px;
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 28px;
  border-radius: 10px;
  background: rgba(255, 255, 255, 0.06);
}

.title {
  font-size: 18px;
  font-weight: 600;
}

.sub {
  margin: 0;
  font-size: 13px;
  opacity: 0.7;
}

.err {
  color: #ff7875;
  font-size: 13px;
}
</style>
