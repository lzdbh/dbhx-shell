import { ref } from 'vue'

import type { Component } from 'vue'

/**
 * 一处声明的右键菜单。
 *
 * 为什么不用 `a-dropdown :trigger="['contextmenu']"`：它要求把目标元素包在自己的
 * 默认插槽里，而我们要挂右键的地方有两类包不进去——终端格子的 DOM 是登记处
 * `createElement` 造的（见 `lib/terminal/registry.ts`），文件列表的行是 `v-for` 出来
 * 的几百个节点。包一层就多一层 DOM，还得给每行造一个 dropdown 实例。
 *
 * 所以这里只有一个模块级的浮层 + 一份状态：谁要菜单就 `openMenu(坐标, 项)`，
 * 全窗口同时最多一个。渲染由 `components/ContextMenu.vue` 负责，本模块只管
 * 「哪些项、哪一项高亮、点完跑谁」。
 */
export interface MenuItem {
  key: string
  label?: string
  /** 右侧灰字提示，用来说明快捷键或作用对象（例如「/root」）。只是文案，不真的绑键。 */
  hint?: string
  icon?: Component
  /** 危险动作：文字涂成错误色，删除这类不可撤销的操作用它 */
  danger?: boolean
  disabled?: boolean
  /** 分隔线。占一行但不可选中、不可点击。 */
  separator?: boolean
  onClick?: () => void
}

export interface MenuPoint {
  x: number
  y: number
}

export interface MenuSize {
  width: number
  height: number
}

export interface Viewport {
  width: number
  height: number
}

/** 菜单与窗口边缘之间留的缝：贴着边缘看起来像没渲染完。 */
export const EDGE_GAP = 6

/**
 * 算出菜单该画在哪。
 *
 * 默认左上角对齐光标（资源管理器的口径：第一个条目正好在指尖下面）。放不下就
 * 朝反方向翻，翻完还溢出才夹到边缘——先翻再夹，是因为「整块挪到可视区」会让
 * 菜单离光标很远，鼠标反而要跑回去找。
 */
export function placeMenu(
  at: MenuPoint,
  size: MenuSize,
  view: Viewport,
  gap = EDGE_GAP,
): { left: number, top: number } {
  let left = at.x
  let top = at.y
  if (left + size.width > view.width - gap) left = at.x - size.width
  if (top + size.height > view.height - gap) top = at.y - size.height
  if (left < gap) left = gap
  if (top < gap) top = gap
  return { left, top }
}

/** 这一项能不能被键盘选中。 */
function pickable(item: MenuItem | undefined): boolean {
  return item !== undefined && item.separator !== true && item.disabled !== true
}

/** 从 `from` 起走 `step` 步，跳过分隔线与禁用项；一圈都没有可选项时给 -1。 */
export function nextActive(items: MenuItem[], from: number, step: number): number {
  const count = items.length
  if (count === 0) return -1
  // 起点无效时按「从头部进入」处理，这样第一次按方向键不会落在分隔线上
  let at = from
  for (let hop = 0; hop < count; hop += 1) {
    at = (at + step + count) % count
    if (pickable(items[at])) return at
  }
  return -1
}

/** 第一个可选项，没有则 -1。菜单刚弹出时高亮它。 */
export function firstActive(items: MenuItem[]): number {
  return nextActive(items, items.length - 1, 1)
}

/** 最后一个可选项：`End` 键用。从 0 往回走一步就等于绕到尾部。 */
export function lastActive(items: MenuItem[]): number {
  return items.length === 0 ? -1 : nextActive(items, 0, -1)
}

const items = ref<MenuItem[]>([])
const point = ref<MenuPoint>({ x: 0, y: 0 })
const active = ref(-1)
const visible = ref(false)

/**
 * 打开（或就地替换）菜单。
 *
 * 空列表直接忽略：右键点了个没有任何可用动作的地方，不该弹一个空框，
 * 也不该把上一个菜单关掉——关掉会让用户以为是自己点没了。
 */
export function openMenu(at: MenuPoint, list: MenuItem[]): void {
  if (list.length === 0) return
  items.value = list
  point.value = at
  active.value = firstActive(list)
  visible.value = true
}

export function closeMenu(): void {
  visible.value = false
  active.value = -1
}

/**
 * 执行某一项。
 *
 * 先关再跑：动作里可能有 `message.success`、开新窗口、甚至把承载菜单的那个 tab
 * 关掉，菜单留着就会指向已经不存在的上下文。
 */
export function trigger(key: string): void {
  const item = items.value.find(i => i.key === key)
  if (item === undefined || item.separator === true || item.disabled === true) return
  closeMenu()
  item.onClick?.()
}

/** 键盘走位。`step` 为 ±1，环绕。 */
export function moveActive(step: number): void {
  const next = nextActive(items.value, active.value, step)
  if (next >= 0) active.value = next
}

/** 直接把高亮放到某一项（鼠标悬停用）。不可选项一律不理。 */
export function setActive(index: number): void {
  if (pickable(items.value[index])) active.value = index
}

export function triggerActive(): void {
  const item = items.value[active.value]
  if (item !== undefined) trigger(item.key)
}

export const menuState = { items, point, active, visible }
