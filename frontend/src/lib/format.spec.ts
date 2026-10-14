import { describe, expect, it } from 'vitest'

import { localJoin, localParent, localSep } from './format'

describe('localSep', () => {
  it('盘符开头用反斜杠，其余用正斜杠', () => {
    expect(localSep('C:\\Users\\dong')).toBe('\\')
    expect(localSep('/home/dong')).toBe('/')
  })
})

describe('localParent', () => {
  it('Windows 逐级上溯，到盘符根停住', () => {
    expect(localParent('C:\\Users\\dong')).toBe('C:\\Users')
    expect(localParent('C:\\Users')).toBe('C:\\')
    expect(localParent('C:\\')).toBeNull()
  })

  it('Unix 到 / 停住', () => {
    expect(localParent('/home/dong')).toBe('/home')
    expect(localParent('/home')).toBe('/')
    expect(localParent('/')).toBeNull()
  })
})

describe('localJoin', () => {
  it('普通目录补分隔符，根目录不重复补', () => {
    expect(localJoin('C:\\Users', 'dong')).toBe('C:\\Users\\dong')
    expect(localJoin('C:\\', 'Users')).toBe('C:\\Users')
    expect(localJoin('/home', 'dong')).toBe('/home/dong')
    expect(localJoin('/', 'home')).toBe('/home')
  })
})
