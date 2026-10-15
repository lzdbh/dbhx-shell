/**
 * 从主机树拖到宫格里的这一趟。
 *
 * 和 `fileDrag.ts` 同样的理由：浏览器在 `dragover` 阶段读不到 `dataTransfer` 的内容，
 * 只能读 `types`，而「这一格该不该亮起来」恰好要在 dragover 时决定。树和格子在同一个
 * webview 里，所以模块里留一份就够了。
 *
 * 为什么不复用 `fileDrag.ts`：那条载荷是「哪个栏 + 一串路径」，这里是「哪台主机」，
 * 两者放一起只会让每个消费点都先做一次类型判断。
 */
export const HOST_DRAG_MIME = 'application/x-dbhx-host'

let dragging: string | null = null

export function beginHostDrag(hostId: string): void {
  dragging = hostId
}

export function endHostDrag(): void {
  dragging = null
}

/** 此刻正在拖的主机 id；没在拖就是 null。 */
export function currentHostDrag(): string | null {
  return dragging
}

/**
 * 把载荷写进事件。
 *
 * `setData` 依然要调：浏览器靠它决定光标的样式，而且没有它整趟拖拽在有些内核里
 * 根本不会开始。内容本身用模块里那份读。
 */
export function writeHostDrag(event: { dataTransfer: DataTransfer | null }, hostId: string): void {
  beginHostDrag(hostId)
  event.dataTransfer?.setData(HOST_DRAG_MIME, hostId)
  if (event.dataTransfer !== null) event.dataTransfer.effectAllowed = 'copy'
}
