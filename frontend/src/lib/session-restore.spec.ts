import { beforeEach, describe, expect, it } from 'vitest'

import { resolve, savedSessions, write } from './session-restore'
import type { LiveEndpoint } from './ipc/bindings'

const KEY = 'dbhx:session-restore'

function endpoint(host: string, port: number, username = 'root'): LiveEndpoint {
  return { host, port, username }
}

const hosts = [
  { id: 'h1', endpoint: { host: '10.0.0.1', port: null, username: 'app' } },
  { id: 'h2', endpoint: { host: '10.0.0.1', port: 2222, username: 'app' } },
]

beforeEach(() => {
  localStorage.clear()
})

describe('会话恢复快照', () => {
  it('存进去的下次启动能原样读出来', () => {
    write([
      { label: '本机终端', protocol: 'local', endpoint: null },
      { label: '生产 A', protocol: 'ssh', endpoint: endpoint('10.0.0.1', 22, 'app') },
    ])
    expect(savedSessions()).toEqual([
      { label: '本机终端', protocol: 'local', endpoint: null },
      { label: '生产 A', protocol: 'ssh', endpoint: { host: '10.0.0.1', port: 22, username: 'app' } },
    ])
  })

  it('空列表删掉键，不留一条空记录让提示赖着', () => {
    write([{ label: 'x', protocol: 'local', endpoint: null }])
    write([])
    expect(localStorage.getItem(KEY)).toBeNull()
    expect(savedSessions()).toEqual([])
  })

  it('存储被改坏时返回空而不是把整份列表丢掉', () => {
    localStorage.setItem(KEY, '{not json')
    expect(savedSessions()).toEqual([])
    localStorage.setItem(KEY, '{"label":"x"}')
    expect(savedSessions()).toEqual([])
    // 坏一条只丢那一条
    localStorage.setItem(KEY, JSON.stringify([
      { label: '好', protocol: 'local', endpoint: null },
      { label: '坏', protocol: 'telnet', endpoint: null },
      { label: '缺字段', protocol: 'ssh' },
    ]))
    expect(savedSessions().map(entry => entry.label)).toEqual(['好'])
  })

  it('快照里绝不出现凭据字段', () => {
    write([{ label: '生产 A', protocol: 'ssh', endpoint: endpoint('10.0.0.1', 22, 'app') }])
    const raw = localStorage.getItem(KEY) ?? ''
    // 口令、私钥、vault 引用都不该在这里
    for (const forbidden of ['password', 'passphrase', 'secret', 'identity', 'token', 'host_id']) {
      expect(raw.toLowerCase()).not.toContain(forbidden)
    }
  })

  it('按 endpoint 反查主机：默认端口要能和 null 对上', () => {
    const list = resolve([{ label: '生产 A', protocol: 'ssh', endpoint: endpoint('10.0.0.1', 22, 'app') }], hosts)
    expect(list).toEqual([{ kind: 'host', label: '生产 A', hostId: 'h1' }])
  })

  it('非默认端口按端口区分，不会把 2222 认成 22', () => {
    const list = resolve([{ label: '跳板', protocol: 'ssh', endpoint: endpoint('10.0.0.1', 2222, 'app') }], hosts)
    expect(list[0]).toEqual({ kind: 'host', label: '跳板', hostId: 'h2' })
    const none = resolve([{ label: '别的', protocol: 'ssh', endpoint: endpoint('10.0.0.1', 22, 'other') }], hosts)
    expect(none[0]?.kind).toBe('unknown')
  })

  it('主机记录删了只剩 unknown，由用户自己重选', () => {
    const list = resolve([{ label: '已删除', protocol: 'ssh', endpoint: endpoint('10.0.0.9', 22) }], hosts)
    expect(list).toEqual([{ kind: 'unknown', label: '已删除', endpoint: endpoint('10.0.0.9', 22, 'root') }])
  })

  it('本机终端不需要反查', () => {
    expect(resolve([{ label: '本机', protocol: 'local', endpoint: null }], [])).toEqual([
      { kind: 'local', label: '本机' },
    ])
  })
})
