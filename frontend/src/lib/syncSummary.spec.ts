import { describe, expect, it } from 'vitest'

import type { SyncReport } from './ipc/bindings'
import { lastSyncAgo, summarize } from './syncSummary'

function report(over: Partial<SyncReport> = {}): SyncReport {
  return {
    outcome: 'nothing',
    hosts: 0,
    imported: 0,
    conflicts: [],
    missing_identities: [],
    missing_credentials: [],
    ...over,
  }
}

const MINUTE = 60_000

describe('summarize', () => {
  it('says what each outcome did', () => {
    expect(summarize(report({ outcome: 'not_configured' }))).toContain('还没配齐')
    expect(summarize(report({ outcome: 'nothing' }))).toContain('什么都没动')
    expect(summarize(report({ outcome: 'pulled', imported: 4, hosts: 9 }))).toBe('从云上取回 4 台，本机共 9 台')
    expect(summarize(report({ outcome: 'created', hosts: 2 }))).toBe('已在云上建好片段，共 2 台')
    expect(summarize(report({ outcome: 'pushed', hosts: 2 }))).toContain('推上云')
  })

  it('appends every follow-up the user has to do by hand', () => {
    const text = summarize(report({
      outcome: 'pulled',
      imported: 1,
      hosts: 3,
      conflicts: ['a', 'b'],
      missing_identities: ['k'],
      missing_credentials: ['p'],
    }))
    expect(text).toContain('2 条两边都改过')
    expect(text).toContain('1 台还缺对应的私钥')
    expect(text).toContain('1 台要重新设一次口令')
  })
})

describe('lastSyncAgo', () => {
  const now = 1_700_000_000_000

  it('says nothing when this machine never synced', () => {
    expect(lastSyncAgo(null, now)).toBeNull()
  })

  it('rounds down to the bucket it is in', () => {
    expect(lastSyncAgo(now - 30_000, now)).toBe('上次同步：刚刚')
    expect(lastSyncAgo(now - 5 * MINUTE, now)).toBe('上次同步：5 分钟前')
    expect(lastSyncAgo(now - 130 * MINUTE, now)).toBe('上次同步：2 小时前')
    expect(lastSyncAgo(now - 3 * 24 * 60 * MINUTE, now)).toBe('上次同步：3 天前')
  })
})
