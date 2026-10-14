<script setup lang="ts">
import { computed } from 'vue'

import type { IconKey } from '../lib/fileType'
import { ACCENTS } from '../lib/fileType'

/**
 * 列表条目的类型图标。
 *
 * 自己画而不是引图标库：`@ant-design/icons-vue` 是独立依赖，为十几个 16px 图标
 * 加一条 npm/Cargo 链不值得。描边用 `currentColor`，所以深浅主题下都跟正文同色；
 * 只有主色（文件夹的黄、图片的绿……）固定，理由是用户在资源管理器里就靠颜色认类型，
 * 跟着主题变会把「图片」和「文档」洗成同一种灰。
 */
const props = defineProps<{ icon: IconKey }>()

const accent = computed(() => ACCENTS[props.icon])
const folderish = computed(() => props.icon === 'folder' || props.icon === 'folder-link')

/** 页身 + 折角。折角画成独立一条，读起来才像「一张纸」。 */
const PAGE = 'M3.4 1.6h5.1l4.1 4.1v8.7H3.4Z'
const FOLD = 'M8.5 1.6l4.1 4.1H8.5Z'
const FOLDER = 'M1.6 3.4h4.2l1.3 1.7h7.3v8.6H1.6Z'

/**
 * 需要几笔才能认出来的那些。其余类型只留底部那条主色带——
 * 16px 上再细的字形只会糊成一团，不如把辨识交给「类型」列。
 */
const MARKS: Partial<Record<IconKey, string>> = {
  code: 'M6.7 8.4 5.1 10.3l1.6 1.9M9.3 8.4l1.6 1.9-1.6 1.9',
  config: 'M8 7.9v4.8M5.8 9.1l4.4 2.4M10.2 9.1l-4.4 2.4',
  image: 'M4.4 12.6 6.9 9.4l1.9 2.3 1.3-1.5 1.5 2.4ZM7 7.6a.9.9 0 1 1-1.8 0 .9.9 0 0 1 1.8 0',
  archive: 'M4.3 8.3h7.4M7.2 8.3v2.6h1.6V8.3',
  audio: 'M8.6 12.4V7.1l2.8-1v3.4M8.6 12.4a1.3 1.1 0 1 1-2.6 0 1.3 1.1 0 0 1 2.6 0',
  video: 'M6.7 7.9v5l4.3-2.5Z',
  key: 'M10.6 7.9a2.4 2.4 0 1 0-2.1 2.4l-.7.7-.9-.9-.9.9.6.6-.6.6.9.9.9-.9.7-.7',
  exe: 'M5 8.2h6v5.2H5ZM6.6 8.2v5.2M9.4 8.2v5.2M5 10.8h6',
  text: 'M5.4 8.2h5.2M5.4 10.2h5.2M5.4 12.2h3',
  doc: 'M5.4 8.2h5.2M5.4 10.2h5.2M5.4 12.2h3',
  sheet: 'M5.2 8.2h5.6M5.2 10.6h5.6M7 7.4v5M9 7.4v5',
  slide: 'M4.8 7.9h6.4v4.6H4.8ZM6.6 13.4h2.8',
  pdf: 'M5.4 12.6 10.6 7.8M6.1 8.4h.9M9.4 12h.9',
}

const mark = computed(() => MARKS[props.icon] ?? '')
</script>

<template>
  <svg class="fi" viewBox="0 0 16 16" width="16" height="16" aria-hidden="true" :style="{ '--accent': accent }">
    <template v-if="folderish">
      <path class="solid" :d="FOLDER" />
      <path v-if="props.icon === 'folder-link'" class="link" d="M11.6 13.6 13 15M13 12.2 11.6 13.6 13 15" />
    </template>
    <template v-else>
      <path class="page" :d="PAGE" />
      <path class="fold" :d="FOLD" />
      <path class="band" d="M4.6 13.5h6.8" />
      <path v-if="mark" class="mark" :d="mark" />
    </template>
  </svg>
</template>

<style scoped>
.fi {
  flex: none;
  display: block;
}

.page {
  fill: currentColor;
  fill-opacity: 0.07;
  stroke: currentColor;
  stroke-opacity: 0.45;
  stroke-width: 0.9;
}

.fold {
  fill: none;
  stroke: currentColor;
  stroke-opacity: 0.45;
  stroke-width: 0.9;
}

.solid {
  fill: var(--accent);
  fill-opacity: 0.85;
  stroke: var(--accent);
  stroke-width: 0.8;
}

.link {
  fill: none;
  stroke: currentColor;
  stroke-width: 1.1;
}

.band {
  stroke: var(--accent);
  stroke-width: 2.2;
  stroke-linecap: round;
}

.mark {
  fill: none;
  stroke: var(--accent);
  stroke-width: 0.9;
  stroke-linejoin: round;
  stroke-linecap: round;
}
</style>
