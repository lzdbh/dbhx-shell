import { computed, ref, watch } from 'vue'

import { defineStore } from 'pinia'

import type { SessionId } from '../lib/ipc/bindings'

/**
 * 分屏布局。
 *
 * 做的是**「当前会话 + 一个钉住的会话」**，不是任意网格。理由很实际：会话模型的核心
 * 是 `sessions.activeId` 这一个标量，输入焦点、SFTP 面板跟随、传输归属全都挂在它上面；
 * 一旦允许 2x2，就得先回答「键盘输入进哪个格子」，那是另一套状态机。而日常
 * 「一边看日志一边敲命令」只需要两个视图，钉住一个、另一个跟着 tab 走，
 * 恰好把新增状态压到一个字段。
 *
 * 只存会话 id，不存比例：比例是纯视觉的，改一次就要重排两个终端，1fr 1fr 已经够用。
 */

const KEY_PINNED = 'dbhx:layout'
const KEY_FOCUSED = 'dbhx:layout:focus'

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
   * 会话被关掉之后把钉住与焦点一起收掉。
   *
   * 不这么做的话，关掉钉住的 tab 会留下一个指向不存在会话的格子：终端实例已经
   * dispose 了，再挂上去只剩一片空白，用户以为程序坏了。焦点同理，否则 Ctrl+W
   * 会一直打在一个不存在的会话上。
   */
  function prune(liveIds: SessionId[]): void {
    if (pinned.value !== null && !liveIds.includes(pinned.value)) pinned.value = null
    if (focused.value !== null && !liveIds.includes(focused.value)) focused.value = null
  }

  watch(pinned, value => { persist(KEY_PINNED, value) })
  watch(focused, value => { persist(KEY_FOCUSED, value) })

  return { pinned, focused, split, pin, unpin, toggle, setFocused, focusTarget, prune }
})

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
