import { describe, expect, it } from 'vitest'

import { logFileName } from './logName'

const AT = new Date(2026, 9, 14, 23, 59, 3)

describe('logFileName', () => {
  it('把标签页标题和时间拼成一个可读的名字', () => {
    expect(logFileName('本机终端', AT)).toBe('本机终端-20261014-235903.log')
  })

  it('任何可能被当成路径的字符都洗掉', () => {
    for (const label of ['../../etc/passwd', 'F:/windows/system', 'a\\b/c', 'con:log', 'a\u0007b']) {
      const name = logFileName(label, AT)
      expect(name).not.toMatch(/[\\/]/)
      expect(name).not.toContain(':')
      // 主干里不该再留下 '..'：否则拼上目录就是一次数 escape
      expect(name.replace('.log', '')).not.toContain('..')
    }
  })

  it('Windows 保留设备名要错开', () => {
    // `CON.log` 这种名字文件根本建不出来，后端会回一句看不懂的 IO 错误
    expect(logFileName('CON', AT)).toBe('dbhx-CON-20261014-235903.log')
    expect(logFileName('nul', AT)).toBe('dbhx-nul-20261014-235903.log')
  })

  it('空标题也要给出一个能用的名字', () => {
    expect(logFileName('   ', AT)).toBe('dbhx-session-20261014-235903.log')
  })

  it('中文长标题不许越过后端 120 字节的上限', () => {
    // 主干限 40 个字符，中文一个字符三个字节，光主干就 120 字节
    const long = '生产数据库主从切换演练用的临时会话'.repeat(3)
    const name = logFileName(long, AT)
    expect(new TextEncoder().encode(name).length).toBeLessThanOrEqual(120)
    expect(name.endsWith('-20261014-235903.log'), '时间戳和扩展名不能被截掉').toBe(true)
  })

  it('时间戳按本地时刻，不是 UTC', () => {
    // 记日志的人看的是自己墙上的钟
    expect(logFileName('x', new Date(2026, 0, 5, 0, 7, 0))).toBe('x-20260105-000700.log')
  })
})
