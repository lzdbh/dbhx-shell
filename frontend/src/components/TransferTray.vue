<script setup lang="ts">
import { computed } from 'vue'

import type { TransferState } from '@/lib/ipc/bindings'
import { formatSize } from '@/lib/format'
import { useTransfersStore } from '@/stores/transfers'

const transfers = useTransfersStore()

const STATE_TEXT: Record<TransferState, string> = {
  queued: '排队中',
  running: '传输中',
  completed: '已完成',
  failed: '失败',
  cancelled: '已取消',
}

const rows = computed(() => transfers.queue)

/** 百分比：总量未知时给 0，由 `indeterminate` 的进度条表达「还在动」。 */
function percentOf(transferred: number, total: number | null): number {
  if (!total) return 0
  return Math.min(100, Math.round((transferred / total) * 100))
}

function failedBatches(): string[] {
  return [...transfers.batches.values()]
    .filter(row => row.ended?.error)
    .map(row => `批次 ${row.batch}：${row.ended?.error ?? ''}`)
}

const batchErrors = computed(failedBatches)
</script>

<template>
  <div v-if="rows.length > 0" class="tray">
    <div class="head">
      <span>传输 {{ transfers.active.length }} / {{ rows.length }}</span>
      <a-button size="small" type="text" @click="transfers.clearFinished()">
        清掉已结束的
      </a-button>
    </div>
    <div v-for="row in rows" :key="row.id" class="row">
      <div class="line">
        <span class="name" :title="row.path">{{ row.kind === 'upload' ? '↑' : '↓' }} {{ row.path.split('/').pop() }}</span>
        <span class="state">{{ STATE_TEXT[row.state] }}</span>
      </div>
      <a-progress
        :percent="percentOf(row.transferred, row.total)"
        :status="row.state === 'failed' ? 'exception' : row.state === 'completed' ? 'success' : 'active'"
        :show-info="false"
        size="small"
      />
      <div class="meta">
        <span>{{ formatSize(row.transferred) }} / {{ row.total === null ? '未知' : formatSize(row.total) }}</span>
        <span v-if="row.error" class="error">{{ row.error }}</span>
      </div>
    </div>
    <a-alert v-for="message in batchErrors" :key="message" :message="message" type="error" show-icon />
  </div>
</template>

<style scoped>
.tray {
  border-top: 1px solid rgba(128, 128, 128, 0.25);
  padding: 6px 8px;
  max-height: 240px;
  overflow: auto;
  flex: none;
}

.head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  font-size: 12px;
  margin-bottom: 4px;
}

.row {
  margin-bottom: 6px;
}

.line {
  display: flex;
  justify-content: space-between;
  gap: 8px;
  font-size: 12px;
}

.name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.state {
  flex: none;
  opacity: 0.7;
}

.meta {
  display: flex;
  justify-content: space-between;
  font-size: 11px;
  opacity: 0.7;
}

.error {
  color: #ff4d4f;
}
</style>
