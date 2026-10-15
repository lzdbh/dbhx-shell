<script setup lang="ts">
import { computed } from 'vue'
import { ClusterOutlined, GlobalOutlined } from '@ant-design/icons-vue'

import type { OsType } from '@/lib/ipc/bindings'
import { OS_GLYPH } from '@/lib/osGlyph'

/**
 * 主机行最左边那颗系统图标。
 *
 * 只画用户自己填的那个字段，不去猜：SSH 横幅里的 `OpenSSH_8.x` 既可能来自路由器，
 * 也可能来自装了第三方 sshd 的 Windows，画错一颗比不画更误导。所以「其他」和「未知」
 * 一律用中性图形——没有依据时，不该给用户一个看起来像依据的东西。
 *
 * 有标志可画的一律走实心品牌图形（Ubuntu 画 Ubuntu、CentOS 画 CentOS），路径数据
 * 都在 `osGlyph.ts`；这里只负责挂上去、统一尺寸。
 */
const props = defineProps<{ os: OsType }>()

const glyph = computed<string | null>(() => OS_GLYPH[props.os] ?? null)
</script>

<template>
  <ClusterOutlined v-if="props.os === 'other'" class="osi" />
  <svg v-else-if="glyph" class="osi brand" viewBox="0 0 24 24" aria-hidden="true">
    <path :d="glyph" />
  </svg>
  <GlobalOutlined v-else class="osi" />
</template>

<style scoped>
.osi {
  flex: none;
}

/* `.anticon` 那套靠 `1em` 定尺寸，手挂的这批 svg 要对齐同一口径，
   否则它们会比「未知」那几行矮一截或者顶出基线。 */
.brand {
  width: 1em;
  height: 1em;
  fill: currentColor;
  vertical-align: -0.125em;
}
</style>
