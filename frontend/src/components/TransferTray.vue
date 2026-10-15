<script setup lang="ts">
import { computed } from 'vue'

import { ArrowDownOutlined, ArrowUpOutlined } from '@ant-design/icons-vue'

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
        <span class="name" :title="row.path">
          <ArrowUpOutlined v-if="row.kind === 'upload'" class="dir" />
          <ArrowDownOutlined v-else class="dir" />
          {{ row.path.split('/').pop() }}
        </span>
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
  border-top: 1px solid var(--line);
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
  display: inline-flex;
  align-items: center;
  gap: 5px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* 上传/下载靠箭头方向区分，颜色只是把这一列点亮一点，不承担含义 */
.dir {
  font-size: 11px;
  color: var(--text-3);
  flex: none;
}

.state {
  flex: none;
  color: var(--text-3);
}

.meta {
  display: flex;
  justify-content: space-between;
  gap: 8px;
  font-size: 12px;
  color: var(--text-3);
  /* 字节数是数字，等宽才不会每刷新一次就左右跳 */
  font-variant-numeric: tabular-nums;
}

.error {
  color: var(--err);
}
</style>
