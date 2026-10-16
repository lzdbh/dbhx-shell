import { flushPromises, mount } from '@vue/test-utils'
import { defineComponent, type Ref } from 'vue'
import { beforeEach, describe, expect, it, vi } from 'vitest'

import { useMacFullscreen } from './fullscreen'

const stub = vi.hoisted(() => ({
  fullscreen: false,
  broken: false,
  cb: null as null | (() => Promise<void>),
  unsubscribed: false,
}))

vi.mock('@tauri-apps/api/window', () => ({
  getCurrentWindow: () => {
    if (stub.broken)
      throw new Error('不是 tauri 窗口')
    return {
      isFullscreen: async () => stub.fullscreen,
      onResized: async (cb: () => Promise<void>) => {
        stub.cb = cb
        return () => { stub.unsubscribed = true }
      },
    }
  },
}))

function probe(): Ref<boolean> {
  let flag: Ref<boolean> | undefined
  const Probe = defineComponent({
    setup() {
      flag = useMacFullscreen()
      return () => null
    },
  })
  mount(Probe)
  return flag!
}

describe('useMacFullscreen', () => {
  beforeEach(() => {
    stub.fullscreen = false
    stub.broken = false
    stub.cb = null
    stub.unsubscribed = false
  })

  it('挂载时回读当前全屏态', async () => {
    stub.fullscreen = true
    const flag = probe()
    await flushPromises()
    expect(flag.value).toBe(true)
  })

  it('resize 后重新回读：进全屏取消占位，退全屏恢复', async () => {
    const flag = probe()
    await flushPromises()
    expect(flag.value).toBe(false)

    stub.fullscreen = true
    await stub.cb?.()
    expect(flag.value).toBe(true)

    stub.fullscreen = false
    await stub.cb?.()
    expect(flag.value).toBe(false)
  })

  it('卸载时取消监听', async () => {
    const Probe = defineComponent({
      setup() {
        useMacFullscreen()
        return () => null
      },
    })
    const wrapper = mount(Probe)
    await flushPromises()
    wrapper.unmount()
    expect(stub.unsubscribed).toBe(true)
  })

  it('拿不到 tauri 窗口时保持 false 且不抛', async () => {
    stub.broken = true
    const flag = probe()
    await flushPromises()
    expect(flag.value).toBe(false)
  })
})
