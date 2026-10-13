import { beforeEach, describe, expect, it } from 'vitest'

import { createPinia, setActivePinia } from 'pinia'
import { nextTick } from 'vue'

import { useLayoutStore } from './layout'

const KEY = 'dbhx:layout'

beforeEach(() => {
  localStorage.clear()
  setActivePinia(createPinia())
})

describe('分屏布局', () => {
  it('默认不分屏', () => {
    const layout = useLayoutStore()
    expect(layout.pinned).toBeNull()
    expect(layout.split).toBe(false)
  })

  it('钉住与取消钉住会写穿 localStorage', async () => {
    const layout = useLayoutStore()
    layout.pin(7)
    // 写穿在 watch 里做，要等一次 flush
    await nextTick()
    expect(localStorage.getItem(KEY)).toBe('7')
    layout.unpin()
    await nextTick()
    expect(localStorage.getItem(KEY)).toBeNull()
  })

  it('重启后从存储里捡回上次的钉住会话', async () => {
    useLayoutStore().pin(12)
    await nextTick()
    setActivePinia(createPinia())
    expect(useLayoutStore().pinned).toBe(12)
  })

  it('存储被改坏时当作没分屏，而不是把 NaN 当会话 id', () => {
    localStorage.setItem(KEY, 'not-a-number')
    setActivePinia(createPinia())
    expect(useLayoutStore().pinned).toBeNull()
    localStorage.setItem(KEY, '-3')
    setActivePinia(createPinia())
    expect(useLayoutStore().pinned).toBeNull()
  })

  it('钉住的会话被关掉后格子自动收回', () => {
    const layout = useLayoutStore()
    layout.pin(3)
    layout.prune([1, 2])
    expect(layout.pinned).toBeNull()
    // 还活着的会话不能被误伤
    layout.pin(2)
    layout.prune([1, 2])
    expect(layout.pinned).toBe(2)
  })

  it('toggle 对同一个会话按两次等于没按', () => {
    const layout = useLayoutStore()
    layout.toggle(5)
    expect(layout.pinned).toBe(5)
    layout.toggle(5)
    expect(layout.pinned).toBeNull()
    // 钉着 A 时 toggle B 是换人，不是取消
    layout.pin(5)
    layout.toggle(6)
    expect(layout.pinned).toBe(6)
  })
})
