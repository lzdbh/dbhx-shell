import { computed, ref, watch } from 'vue'

import { defineStore } from 'pinia'

import type { SessionId } from '../lib/ipc/bindings'

/**
 * 主窗口的布局：两种看法（跟着 tab 走的「标签」，和一次看几台机器的「宫格」）。
 *
 * 标签模式做的是**「当前会话 + 一个钉住的会话」**，不是任意网格。理由很实际：会话模型的核心
 * 是 `sessions.activeId` 这一个标量，输入焦点、SFTP 面板跟随、传输归属全都挂在它上面；
 * 一旦允许 2x2，就得先回答「键盘输入进哪个格子」，那是另一套状态机。而日常
 * 「一边看日志一边敲命令」只需要两个视图，钉住一个、另一个跟着 tab 走，
 * 恰好把新增状态压到一个字段。
 *
 * 宫格模式就是把那套「另一张状态机」补上：`cells` 是**格子 → 会话**的表，
 * `focusCell` 说明键盘在哪一格。表里的会话 id 和 tab 是同一批会话，不是新对象——
 * 所以一格断开不影响别的格，一个会话也不会同时占两格。
 *
 * 只存会话 id，不存比例：比例是纯视觉的，改一次就要重排两个终端，1fr 1fr 已经够用。
 */

const KEY_PINNED = 'dbhx:layout'
const KEY_FOCUSED = 'dbhx:layout:focus'
const KEY_MODE = 'dbhx:layout:mode'
const KEY_GRID_SIZE = 'dbhx:layout:grid'
const KEY_CELLS = 'dbhx:layout:cells'

/** 主窗口的两种看法：跟着 tab 走的一/两格，或者一次看好几台机器的宫格。 */
export type ViewMode = 'tabs' | 'grid'

/** 宫格档数。`columns` 是排布用的列数，数组长度就是格子数。 */
export const GRID_SIZES = [1, 2, 4, 6, 9] as const
export type GridSize = (typeof GRID_SIZES)[number]

const GRID_COLUMNS: Record<GridSize, number> = { 1: 1, 2: 2, 4: 2, 6: 3, 9: 3 }

/** 这一档该排几列：宫格组件的 `grid-template-columns` 用它算，别让两处各写一张表。 */
export function columnsOf(size: GridSize): number {
  return GRID_COLUMNS[size]
}

export const useLayoutStore = defineStore('layout', () => {
  /** 钉在右格的会话；null 表示没分屏 */
  const pinned = ref<SessionId | null>(read(KEY_PINNED))

  /**
   * 当前吃键盘的那一格对应的会话；null 表示「跟着当前 tab」。
   *
   * 分屏时右格和主格都是活的，`sessions.activeId` 只说明「哪个 tab 被选中」，
   * 说明不了用户在哪个格子里打字。关会话、文件面板跟哪个目录这两件事必须按后者走，
   * 否则用户在右格里按 Ctrl+W，关掉的却是主格那台机器。
   */
  const focused = ref<SessionId | null>(read(KEY_FOCUSED))

  const split = computed(() => pinned.value !== null)

  /** 主窗口现在的看法。 */
  const mode = ref<ViewMode>(readMode())
  /** 宫格档数：1 / 2 / 4 / 6 / 9。 */
  const gridSize = ref<GridSize>(readGridSize())
  /** 宫格里每一格对应的会话；`null` 就是空格子。长度始终等于 `gridSize`。 */
  const cells = ref<(SessionId | null)[]>(readCells(gridSize.value))
  /** 键盘在哪一格。只在宫格模式有意义。 */
  const focusCell = ref(0)

  /** 把长度对齐到档数：多出来的截掉、不够的补空格。已有的格子位置不动。 */
  function fit(list: (SessionId | null)[], size: GridSize): (SessionId | null)[] {
    const filled = list.slice(0, size)
    while (filled.length < size) filled.push(null)
    return filled
  }

  function cellSession(index: number): SessionId | null {
    return cells.value[index] ?? null
  }

  function setMode(next: ViewMode): void {
    mode.value = next
  }

  /**
   * 换档数。
   *
   * 从小宫格放大：空格子加在末尾，已钉的格子原地不动。
   * 反过来缩小：被截掉那一格上的会话**不关**，只是没有格子画它了——它还在 tab 里，
   * 用户切回标签模式或放大档数都还能看见。当场关掉别人的会话是不可逆的误伤。
   */
  function setGridSize(size: GridSize): void {
    gridSize.value = size
    cells.value = fit(cells.value, size)
    if (focusCell.value >= size) focusCell.value = size - 1
  }

  /**
   * 把会话放进某一格。
   *
   * 一个会话只可能有一个 DOM：终端实例归谁插取决于登记处最后一次认领，
   * 所以这里先把它在别的格子里的那份擦掉，否则两格会抢同一个终端、画面随机闪。
   */
  function placeCell(index: number, id: SessionId | null): void {
    if (id !== null) {
      const already = cells.value.findIndex(item => item === id)
      if (already >= 0 && already !== index) cells.value[already] = null
    }
    cells.value[index] = id
    focusOnCell(index)
  }

  function clearCell(index: number): void {
    cells.value[index] = null
    // 清空的是当前格时，键盘跟着落到这一格（空格子 = 谁都不吃键）
    if (focusCell.value === index) focused.value = null
  }

  /** 点一格：键盘交给它，并且把 tab 也翻到那个会话上，别让标签栏和格子各说各话。 */
  function focusOnCell(index: number): void {
    focusCell.value = index
    focused.value = cells.value[index]
  }

  function pin(id: SessionId): void {
    const previous = pinned.value
    pinned.value = id
    // 原来钉住的会话从右格消失了，焦点不能还留在它身上
    if (focused.value !== null && focused.value === previous) focused.value = null
  }

  function unpin(): void {
    const dropped = pinned.value
    pinned.value = null
    if (focused.value !== null && focused.value === dropped) focused.value = null
  }

  /** 对同一个会话按两次就是取消——工具栏按钮与 Ctrl+E 都走这里。 */
  function toggle(id: SessionId): void {
    if (pinned.value === id) {
      unpin()
      return
    }
    pin(id)
  }

  function setFocused(id: SessionId | null): void {
    focused.value = id
  }

  /** 空格子时该动哪个会话：焦点格优先，没焦点就是当前 tab。 */
  function focusTarget(activeId: SessionId | null): SessionId | null {
    return focused.value ?? activeId
  }

  /**
   * 会话被关掉之后把钉住、焦点、宫格里的那一格一起收掉。
   *
   * 不这么做的话，关掉钉住的 tab 会留下一个指向不存在会话的格子：终端实例已经
   * dispose 了，再挂上去只剩一片空白，用户以为程序坏了。焦点同理，否则 Ctrl+W
   * 会一直打在一个不存在的会话上。
   *
   * 宫格这边还要单独处理焦点：当前格被清成空，就顺手挪到第一个还有会话的格子，
   * 不然用户按半天键没有一格接——空格子吃键盘是「什么都没发生」，最难查的那种。
   */
  function prune(liveIds: SessionId[]): void {
    if (pinned.value !== null && !liveIds.includes(pinned.value)) pinned.value = null
    if (focused.value !== null && !liveIds.includes(focused.value)) focused.value = null
    // 先记下焦点格原来挂着谁：空格子是合法的焦点目标（用户刚点了它、正要把主机拖进去），
    // 只有「原来那位刚好被关掉了」才需要搬家，否则一有会话增删就把焦点从用户手上抢走。
    const was = cells.value[focusCell.value]
    cells.value = cells.value.map(item => (item !== null && !liveIds.includes(item) ? null : item))
    if (was !== null && cells.value[focusCell.value] === null) {
      const next = cells.value.findIndex(item => item !== null)
      if (next >= 0) focusOnCell(next)
    }
  }

  watch(pinned, value => { persist(KEY_PINNED, value) })
  watch(focused, value => { persist(KEY_FOCUSED, value) })
  watch(mode, value => { localStorage.setItem(KEY_MODE, value) })
  watch(gridSize, value => { localStorage.setItem(KEY_GRID_SIZE, String(value)) })
  watch(cells, value => { localStorage.setItem(KEY_CELLS, JSON.stringify(value)) }, { deep: true })

  return {
    pinned,
    focused,
    split,
    mode,
    gridSize,
    cells,
    focusCell,
    pin,
    unpin,
    toggle,
    setFocused,
    focusTarget,
    prune,
    setMode,
    setGridSize,
    cellSession,
    placeCell,
    clearCell,
    focusOnCell,
  }
})

/** 读一个数字并按各自的合法集合兜底：存储是被手工改坏过的重灾区。 */
function readNumber(key: string): number | null {
  try {
    const raw = localStorage.getItem(key)
    if (raw === null) return null
    const parsed = Number(raw)
    return Number.isFinite(parsed) ? parsed : null
  } catch {
    return null
  }
}

function readMode(): ViewMode {
  try {
    return localStorage.getItem(KEY_MODE) === 'grid' ? 'grid' : 'tabs'
  } catch {
    return 'tabs'
  }
}

function readGridSize(): GridSize {
  const value = readNumber(KEY_GRID_SIZE)
  return (GRID_SIZES as readonly number[]).includes(value ?? -1) ? (value as GridSize) : 4
}

/**
 * 格子表。
 *
 * 长度按**当前档数**重排：档数和格子表是两份存储，改档数那一刻崩掉、或者用户手改过
 * 其中一份，两边就会不一致；以档数为准至少能保证每格都有东西可画。
 */
function readCells(size: GridSize): (SessionId | null)[] {
  let list: unknown[] = []
  try {
    const raw = localStorage.getItem(KEY_CELLS)
    if (raw) {
      const parsed: unknown = JSON.parse(raw)
      if (Array.isArray(parsed)) list = parsed
    }
  } catch {
    list = []
  }
  const cleaned = list.map(item => (typeof item === 'number' && Number.isInteger(item) && item > 0 ? item : null))
  while (cleaned.length < size) cleaned.push(null)
  return cleaned.slice(0, size)
}

function persist(key: string, value: SessionId | null): void {
  try {
    if (value === null) localStorage.removeItem(key)
    else localStorage.setItem(key, String(value))
  } catch {
    /* 存不下就算了，分屏不是必须跨重启的东西 */
  }
}

function read(key: string): SessionId | null {
  try {
    const raw = localStorage.getItem(key)
    if (raw === null) return null
    const parsed = Number(raw)
    // id 由 Rust 侧分配，是正整数；NaN、0、负数说明存储被改坏了，当作没有
    return Number.isInteger(parsed) && parsed > 0 ? parsed : null
  } catch {
    return null
  }
}
