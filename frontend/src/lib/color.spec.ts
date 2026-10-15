import { describe, expect, it } from 'vitest'

import { withAlpha } from './color'

describe('withAlpha', () => {
  it('三位与六位 hex 走同一条路', () => {
    expect(withAlpha('#fff', 0.5)).toBe('rgba(255, 255, 255, 0.5)')
    expect(withAlpha('#1677ff', 0.16)).toBe('rgba(22, 119, 255, 0.16)')
  })

  it('透明度超出范围时夹紧', () => {
    expect(withAlpha('#000', -1)).toBe('rgba(0, 0, 0, 0)')
    expect(withAlpha('#000', 9)).toBe('rgba(0, 0, 0, 1)')
  })

  it('非法 hex 回中性灰，不放无效值进 CSS', () => {
    // 浏览器遇到非法的变量值会整条声明丢掉，那处不是变灰而是没底色
    for (const bad of ['', 'red', '#12345', '#1234567', '1677ff']) {
      expect(withAlpha(bad, 0.2)).toBe('rgba(128, 128, 128, 0.2)')
    }
  })
})
