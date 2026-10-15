<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, ref, watch } from 'vue'

import type { CSSProperties } from 'vue'

import { closeMenu, firstActive, lastActive, menuState, moveActive, placeMenu, setActive, trigger, triggerActive } from '@/lib/contextMenu'

/**
 * 右键菜单的浮层。全窗口只有一个实例，挂在 `App.vue` / `FileManager.vue` 上。
 *
 * 定位要等实测：菜单宽度由最长的标签 + 提示决定，事先不知道，所以第一帧按光标
 * 位置画、量完再挪。为了不让这一挪被看见，量到之前整块保持透明。
 */
const panel = ref<HTMLElement | null>(null)
const placed = ref<{ left: number, top: number } | null>(null)

const style = computed<CSSProperties>(() => ({
  left: `${placed.value?.left ?? menuState.point.value.x}px`,
  top: `${placed.value?.top ?? menuState.point.value.y}px`,
  visibility: placed.value === null ? 'hidden' : 'visible',
}))

async function reposition(): Promise<void> {
  placed.value = null
  await nextTick()
  const el = panel.value
  if (el === null) return
  placed.value = placeMenu(
    menuState.point.value,
    { width: el.offsetWidth, height: el.offsetHeight },
    { width: window.innerWidth, height: window.innerHeight },
  )
}

watch(() => [menuState.visible.value, menuState.point.value], ([vis]) => {
  if (vis) void reposition()
})

function onKeydown(event: KeyboardEvent): void {
  if (!menuState.visible.value) return
  switch (event.key) {
    case 'Escape':
      // 吃掉：Esc 在锁屏之外的地方没有别的用途，但查找条也听 Esc，别让它俩同时收
      closeMenu()
      break
    case 'ArrowDown':
      moveActive(1)
      break
    case 'ArrowUp':
      moveActive(-1)
      break
    case 'Home':
      setActive(firstActive(menuState.items.value))
      break
    case 'End':
      setActive(lastActive(menuState.items.value))
      break
    case 'Enter':
      triggerActive()
      break
    case 'Tab':
      // 菜单不该参与 Tab 序列：按 Tab 等于放弃菜单
      closeMenu()
      break
    default:
      return
  }
  // 键盘事件一旦被菜单认领，就不能再往下传到目标控件。
  // 少了这一句，菜单开着时按 Enter 会同时触发「高亮的那一项」和输入框自己的
  // Enter（快捷命令栏会当场把框里的内容再发一遍）；↑ / ↓ 同理会翻到别的历史。
  event.preventDefault()
  event.stopPropagation()
}

function onPointerDown(event: PointerEvent): void {
  if (!menuState.visible.value) return
  const el = panel.value
  // 捕获阶段先于目标自己的处理器：点菜单里就交给条目自己的 click，点外面即关
  if (el !== null && event.target instanceof Node && el.contains(event.target)) return
  closeMenu()
}

function onBlur(): void {
  closeMenu()
}

function install(): void {
  document.addEventListener('keydown', onKeydown, true)
  document.addEventListener('pointerdown', onPointerDown, true)
  window.addEventListener('blur', onBlur)
}

function uninstall(): void {
  document.removeEventListener('keydown', onKeydown, true)
  document.removeEventListener('pointerdown', onPointerDown, true)
  window.removeEventListener('blur', onBlur)
}

watch(menuState.visible, vis => { if (vis) install(); else uninstall() })

onBeforeUnmount(uninstall)
</script>

<template>
  <Teleport to="body">
    <div
      v-if="menuState.visible.value"
      ref="panel"
      class="ctx-menu"
      :style="style"
      role="menu"
      @contextmenu.prevent
    >
      <template v-for="(entry, index) in menuState.items.value" :key="entry.key">
        <div v-if="entry.separator" class="sep" />
        <div
          v-else
          class="entry"
          :class="{ on: menuState.active.value === index, off: entry.disabled, danger: entry.danger }"
          role="menuitem"
          @pointerenter="setActive(index)"
          @click="trigger(entry.key)"
        >
          <span class="mark">
            <component :is="entry.icon" v-if="entry.icon" />
          </span>
          <span class="label">{{ entry.label }}</span>
          <span v-if="entry.hint" class="hint">{{ entry.hint }}</span>
        </div>
      </template>
    </div>
  </Teleport>
</template>

<style scoped>
.ctx-menu {
  position: fixed;
  z-index: 1080;
  min-width: 168px;
  padding: 4px;
  background: var(--bg-raised);
  border: 1px solid var(--line-strong);
  border-radius: var(--radius);
  box-shadow: var(--shadow);
  font-size: 12px;
  user-select: none;
}

.entry {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 4px 8px;
  border-radius: var(--radius-sm);
  color: var(--text-1);
  cursor: default;
  transition: background var(--tap) ease;
}

.entry.on {
  background: var(--bg-hover);
}

.entry.off {
  color: var(--text-3);
  cursor: not-allowed;
}

.entry.danger {
  color: var(--err);
}

/* 图标列固定宽度：没有图标的项要和有的对齐，否则标签左沿参差不齐 */
.mark {
  display: inline-flex;
  width: 14px;
  flex: none;
  justify-content: center;
  opacity: 0.8;
}

.label {
  flex: 1;
  white-space: nowrap;
}

.hint {
  margin-left: 18px;
  color: var(--text-3);
  white-space: nowrap;
}

.sep {
  height: 1px;
  margin: 4px 6px;
  background: var(--line);
}
</style>
