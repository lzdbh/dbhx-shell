/**
 * 两栏之间的拖拽。
 *
 * 只认我们自己发起的拖拽：`dragstart` 时把来源栏和路径写进 `dataTransfer`，同时留在
 * 这个模块里一份。留这一份不是冗余——浏览器在 `dragover` 阶段出于安全考虑只让读
 * `types`，读不到内容，而「这一栏该不该亮起来」恰好在 dragover 时就要决定。
 * 两栏在同一个 webview、同一个 document 里，所以这份进程内状态够用，也不必担心
 * 别的窗口拖过来（跨窗口的拖拽本来就没有 dataTransfer 共享）。
 */
export type PaneSide = 'local' | 'remote'

export interface DragPayload {
  side: PaneSide
  paths: string[]
}

/** 自定义 MIME：用 `text/plain` 的话会和别的拖拽源串味。 */
export const DRAG_MIME = 'application/x-dbhx-files'

let dragging: DragPayload | null = null

export function beginDrag(payload: DragPayload): void {
  dragging = payload
}

export function endDrag(): void {
  dragging = null
}

/** 此刻正在拖的东西，`dragover` 时用。 */
export function currentDrag(): DragPayload | null {
  return dragging
}

/**
 * 这次拖拽能不能落到 `paneSide` 这一栏。
 *
 * 同侧不算传输（在本栏里拖到本栏什么也不该发生），空路径也不接。返回去侧的载荷，
 * 交给调用方发起真正的传输。
 */
export function dropTargetOf(payload: DragPayload | null, paneSide: PaneSide): DragPayload | null {
  if (payload === null || payload.paths.length === 0) return null
  return payload.side === paneSide ? null : payload
}

/**
 * 这一栏此刻值不值得高亮成「放开就传」。
 *
 * `dragover` 里 `getData()` 拿到的是空串，所以判据只能是「我们自己发起的拖拽还在」
 * 加上「来源不是本栏」。
 */
export function isDropCandidate(payload: DragPayload | null, paneSide: PaneSide): boolean {
  return dropTargetOf(payload, paneSide) !== null
}

/** 把载荷写进事件；`dropEffect` 设成 copy，鼠标上才会是「加号」而不是移动箭头。 */
export function writeDrag(event: { dataTransfer: DataTransfer | null }, payload: DragPayload): void {
  beginDrag(payload)
  event.dataTransfer?.setData(DRAG_MIME, JSON.stringify(payload))
  if (event.dataTransfer !== null) event.dataTransfer.effectAllowed = 'copy'
}
