import { createPinia, setActivePinia } from 'pinia'
import { beforeEach, describe, expect, it } from 'vitest'

import { useQuickStore } from './quick'

beforeEach(() => {
  localStorage.clear()
  setActivePinia(createPinia())
})

describe('快捷命令栏 store', () => {
  it('开关只是翻一个布尔，两格共用同一个显示状态', () => {
    const quick = useQuickStore()
    expect(quick.visible).toBe(false)
    quick.toggle()
    expect(quick.visible).toBe(true)
    quick.toggle()
    expect(quick.visible).toBe(false)
  })

  it('空的、只有空白的命令不记', () => {
    const quick = useQuickStore()
    quick.remember('local', '')
    quick.remember('local', '   ')
    expect(quick.entries('local')).toEqual([])
  })

  it('记的时候去掉首尾空白：发出去的就是这一行，历史要和发出去的一致', () => {
    const quick = useQuickStore()
    quick.remember('local', '  ls -l\t')
    expect(quick.entries('local')).toEqual(['ls -l'])
  })

  it('重复命令挪到最前，而不是留下两份', () => {
    const quick = useQuickStore()
    quick.remember('local', 'git status')
    quick.remember('local', 'git log')
    quick.remember('local', 'git status')
    expect(quick.entries('local')).toEqual(['git status', 'git log'])
  })

  it('历史按归属分开：本机那份不该出现在服务器上', () => {
    const quick = useQuickStore()
    quick.remember('local', 'dir')
    quick.remember('host:h1', 'ls')
    expect(quick.entries('local')).toEqual(['dir'])
    expect(quick.entries('host:h1')).toEqual(['ls'])
    expect(quick.entries('host:h2')).toEqual([])
  })

  it('超出上限时丢最旧的：翻历史翻到第 50 条已经够远了', () => {
    const quick = useQuickStore()
    for (let i = 0; i < 60; i++) quick.remember('local', `cmd ${i}`)
    const list = quick.entries('local')
    expect(list).toHaveLength(50)
    expect(list[0]).toBe('cmd 59')
    expect(list).not.toContain('cmd 0')
  })

  it('历史不落盘：命令里可能带着一次性口令', () => {
    const quick = useQuickStore()
    quick.remember('local', 'mysql -u root -psecret')
    expect(localStorage.length).toBe(0)

    setActivePinia(createPinia())
    expect(useQuickStore().entries('local')).toEqual([])
  })
})
