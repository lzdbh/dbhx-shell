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

describe('分屏里的焦点格', () => {
  it('没有焦点格时回落到当前 tab', () => {
    const layout = useLayoutStore()
    expect(layout.focusTarget(4)).toBe(4)
    layout.setFocused(9)
    // 焦点一旦确立就压过 tab：分屏时用户在右格打字，切 tab 不代表切了格子
    expect(layout.focusTarget(4)).toBe(9)
    layout.setFocused(null)
    expect(layout.focusTarget(null)).toBeNull()
  })

  it('取消钉住时焦点正落在那格上就一起清掉', () => {
    const layout = useLayoutStore()
    layout.pin(3)
    layout.setFocused(3)
    layout.unpin()
    expect(layout.focused).toBeNull()
    expect(layout.focusTarget(8)).toBe(8)
  })

  it('换钉住的会话时，旧会话的焦点不能悬空', () => {
    const layout = useLayoutStore()
    layout.pin(3)
    layout.setFocused(3)
    layout.pin(4)
    // 3 已经从右格消失，焦点留着它 Ctrl+W 就打在看不见的会话上
    expect(layout.focused).toBeNull()
    expect(layout.focusTarget(8)).toBe(8)
  })

  it('焦点所在的会话被关掉时一起收回', () => {
    const layout = useLayoutStore()
    layout.pin(3)
    layout.setFocused(3)
    layout.prune([1, 2])
    expect(layout.pinned).toBeNull()
    expect(layout.focused).toBeNull()
  })
})

describe('宫格视图', () => {
  it('默认标签视图、4 格全空', () => {
    const layout = useLayoutStore()
    expect(layout.mode).toBe('tabs')
    expect(layout.gridSize).toBe(4)
    expect(layout.cells).toEqual([null, null, null, null])
  })

  it('换档数只截尾巴、补空格，已经钉好的格子位置不动', () => {
    const layout = useLayoutStore()
    layout.placeCell(1, 11)
    layout.setGridSize(9)
    expect(layout.cells).toEqual([null, 11, null, null, null, null, null, null, null])
    layout.setGridSize(2)
    expect(layout.cells).toEqual([null, 11])
    // 缩小到 1 格时焦点格下标不能还指着已经不存在的第三格
    layout.setGridSize(1)
    expect(layout.focusCell).toBe(0)
  })

  it('同一个会话不会占两格：放进去会把原来那一格腾出来', () => {
    const layout = useLayoutStore()
    layout.placeCell(0, 7)
    layout.placeCell(2, 7)
    expect(layout.cells[0]).toBeNull()
    expect(layout.cells[2]).toBe(7)
    expect(layout.cells.filter(item => item === 7)).toHaveLength(1)
  })

  it('放会话进一格会把键盘也给这一格', () => {
    const layout = useLayoutStore()
    layout.placeCell(3, 21)
    expect(layout.focusCell).toBe(3)
    expect(layout.focused).toBe(21)
    expect(layout.focusTarget(1)).toBe(21)
  })

  it('腾空格子不关会话，只是这一格不再画它', () => {
    const layout = useLayoutStore()
    layout.placeCell(0, 5)
    layout.placeCell(1, 6)
    layout.clearCell(1)
    expect(layout.cells).toEqual([5, null, null, null])
    // 被腾掉的那个会话还活着，剪枝不该把它抹了
    layout.prune([5, 6])
    expect(layout.cells[0]).toBe(5)
  })

  it('会话关掉后它那一格变空，焦点挪到下一个还有东西的格子', () => {
    const layout = useLayoutStore()
    layout.placeCell(0, 5)
    layout.placeCell(2, 6)
    layout.focusOnCell(0)
    layout.prune([6])
    expect(layout.cells).toEqual([null, null, 6, null])
    expect(layout.focusCell).toBe(2)
    expect(layout.focused).toBe(6)
  })

  it('焦点停在空格子上时，别处的会话增删不能把焦点抢走', () => {
    const layout = useLayoutStore()
    layout.placeCell(0, 5)
    // 用户点了空格子准备往里拖主机：这一刻焦点格是空的，但它是用户选的
    layout.focusOnCell(2)
    // 新会话建好、列表刷新，剪枝顺带跑一次
    layout.prune([5, 6])
    expect(layout.focusCell).toBe(2)
    expect(layout.cells[0]).toBe(5)
  })

  it('视图、档数、格子表三样都落盘，重启后回到同一副画面', async () => {
    const layout = useLayoutStore()
    layout.setMode('grid')
    layout.setGridSize(6)
    layout.placeCell(2, 8)
    await nextTick()
    expect(localStorage.getItem('dbhx:layout:mode')).toBe('grid')
    expect(localStorage.getItem('dbhx:layout:grid')).toBe('6')
    expect(localStorage.getItem('dbhx:layout:cells')).toBe('[null,null,8,null,null,null]')

    setActivePinia(createPinia())
    const again = useLayoutStore()
    expect(again.mode).toBe('grid')
    expect(again.gridSize).toBe(6)
    expect(again.cells).toEqual([null, null, 8, null, null, null])
  })

  it('盘上写坏了就回到默认，而不是画出一个指向不存在会话的格子', () => {
    localStorage.setItem('dbhx:layout:grid', '7')
    localStorage.setItem('dbhx:layout:cells', '["x", 3, -1, 0, 9]')
    setActivePinia(createPinia())
    const layout = useLayoutStore()
    // 7 不是合法档数 → 回落 4 格
    expect(layout.gridSize).toBe(4)
    // 非正整数的一律当空格；多出来的按档数截掉
    expect(layout.cells).toEqual([null, 3, null, null])
  })
})
