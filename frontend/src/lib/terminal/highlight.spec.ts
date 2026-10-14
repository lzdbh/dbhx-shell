import { describe, expect, it } from 'vitest'

import { compile, hasBell, scanLine, type HighlightRule } from './highlight'

function rule(over: Partial<HighlightRule> = {}): HighlightRule {
  return {
    id: over.id ?? 'r',
    pattern: '',
    regex: false,
    caseSensitive: false,
    background: '#ff0000',
    bell: false,
    enabled: true,
    ...over,
  }
}

describe('关键字规则编译', () => {
  it('字面量规则不解释元字符', () => {
    const compiled = compile([rule({ pattern: 'err(or' })])
    expect(scanLine('a err(or b', compiled)).toEqual([{ start: 2, end: 8, rule: 0 }])
  })

  it('正则规则按正则解释', () => {
    const compiled = compile([rule({ pattern: '\\d{3}', regex: true })])
    expect(scanLine('code 4211', compiled).map(h => h.end - h.start)).toEqual([3])
  })

  it('非法正则不抛出去，只当这条没启用', () => {
    const compiled = compile([rule({ pattern: 'ite(m', regex: true })])
    expect(compiled[0].re).toBeNull()
    expect(scanLine('item item', compiled)).toEqual([])
  })

  it('空串规则不参与：它会把整屏刷成一块颜色', () => {
    expect(scanLine('abc', compile([rule({ pattern: '' })]))).toEqual([])
  })

  it('关掉的规则一律不命中', () => {
    expect(scanLine('error', compile([rule({ pattern: 'error', enabled: false })]))).toEqual([])
  })

  it('底色不合法的规则不画：宁可不上色也别画花', () => {
    expect(scanLine('error', compile([rule({ pattern: 'error', background: 'red' })]))).toEqual([])
    expect(scanLine('error', compile([rule({ pattern: 'error', background: '#ff0' })]))).toEqual([])
  })
})

describe('关键字命中扫描', () => {
  it('默认不区分大小写，勾上之后严格匹配', () => {
    const loose = compile([rule({ pattern: 'error' })])
    expect(scanLine('Error ERROR error', loose)).toHaveLength(3)
    const strict = compile([rule({ pattern: 'error', caseSensitive: true })])
    expect(scanLine('Error ERROR error', strict)).toEqual([{ start: 12, end: 17, rule: 0 }])
  })

  it('重叠时排在前面的规则赢', () => {
    const compiled = compile([rule({ id: 'a', pattern: 'runtime', background: '#111111' }), rule({ id: 'b', pattern: 'time', background: '#222222' })])
    expect(scanLine('runtime error', compiled)).toEqual([{ start: 0, end: 7, rule: 0 }])
  })

  it('多条命中按起点排序，跨规则也排', () => {
    const compiled = compile([rule({ id: 'a', pattern: 'bb' }), rule({ id: 'b', pattern: 'aa' })])
    expect(scanLine('aa bb aa bb', compiled).map(h => h.start)).toEqual([0, 3, 6, 9])
  })

  it('零宽匹配自己前进，不死循环也不画空命中', () => {
    const compiled = compile([rule({ pattern: 'x*', regex: true })])
    expect(scanLine('yxy', compiled)).toEqual([{ start: 1, end: 2, rule: 0 }])
  })

  it('响铃只在真有规则要响时才算数', () => {
    expect(hasBell(compile([rule({ pattern: 'error' })]))).toBe(false)
    expect(hasBell(compile([rule({ pattern: 'error', bell: true })]))).toBe(true)
    // 关掉的、不上色的都不该在半夜把人吵醒
    expect(hasBell(compile([rule({ pattern: 'error', bell: true, enabled: false })]))).toBe(false)
    // 不画的规则也不该响：看不见却响一声，用户只会以为终端坏了
    expect(hasBell(compile([rule({ pattern: 'error', bell: true, background: 'red' })]))).toBe(false)
  })
})
