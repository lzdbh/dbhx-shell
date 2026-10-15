<script setup lang="ts">
import { nextTick, ref, watch } from 'vue'

import { LockOutlined } from '@ant-design/icons-vue'

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
      <div class="title">
        <LockOutlined />应用已锁定
      </div>
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
  /* 不透出底下内容，也不做毛玻璃：半透明加模糊仍然能辨出地址、用户名，
     锁屏这一层要的是「什么都看不见」，所以用最深的那一档表面整块盖住 */
  background: var(--bg-app);
  color: var(--text-1);
}

/* 卡片靠「浮起的一档表面 + 一道边 + 投影」和背景分开。
   之前写的是 rgba(255, 255, 255, 0.06)：浅色主题下白压白，卡片等于消失。 */
.card {
  width: 320px;
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 28px;
  border-radius: var(--radius-lg);
  background: var(--bg-raised);
  border: 1px solid var(--line);
  box-shadow: var(--shadow);
}

.title {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 18px;
  font-weight: 600;
}

.sub {
  margin: 0;
  font-size: 13px;
  color: var(--text-2);
}

.err {
  color: var(--err);
  font-size: 13px;
}
</style>
