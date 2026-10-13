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
 * 只存会话 id，不存比例：比例是纯视觉的，改一次就要重排两个终端，
 * 而 1fr 1fr 已经够用。
 */

const KEY = 'dbhx:layout'

export const useLayoutStore = defineStore('layout', () => {
  /** 钉在右格的会话；null 表示没分屏 */
  const pinned = ref<SessionId | null>(read())

  const split = computed(() => pinned.value !== null)

  function pin(id: SessionId): void {
    pinned.value = id
  }

  function unpin(): void {
    pinned.value = null
  }

  /** 对同一个会话按两次就是取消——工具栏按钮与 Ctrl+E 都走这里。 */
  function toggle(id: SessionId): void {
    pinned.value = pinned.value === id ? null : id
  }

  /**
   * 会话被关掉之后把钉住状态清掉。
   *
   * 不这么做的话，关掉钉住的 tab 会留下一个指向不存在会话的格子：终端实例已经
   * dispose 了，再挂上去只剩一片空白，用户以为程序坏了。
   */
  function prune(liveIds: SessionId[]): void {
    if (pinned.value !== null && !liveIds.includes(pinned.value)) pinned.value = null
  }

  watch(pinned, value => {
    try {
      if (value === null) localStorage.removeItem(KEY)
      else localStorage.setItem(KEY, String(value))
    } catch {
      /* 存不下就算了，分屏不是必须跨重启的东西 */
    }
  })

  return { pinned, split, pin, unpin, toggle, prune }
})

function read(): SessionId | null {
  try {
    const raw = localStorage.getItem(KEY)
    if (raw === null) return null
    const parsed = Number(raw)
    // id 由 Rust 侧分配，是正整数；NaN、0、负数说明存储被改坏了，当作没分屏
    return Number.isInteger(parsed) && parsed > 0 ? parsed : null
  } catch {
    return null
  }
}
