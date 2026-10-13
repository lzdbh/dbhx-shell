import { ref, watch } from 'vue'

import { defineStore } from 'pinia'

import { lockApi } from '../lib/ipc/client'

/**
 * 应用锁的前端门面。
 *
 * 真正的把关在后端：锁定态下新拨号与凭据读写一律被 `require_unlocked` 挡掉。
 * 这个 store 只做三件事——把「锁没锁 / 有没有设 PIN」搬到界面上、把用户敲的 PIN
 * 明文朝内送一次、以及跑空闲自动上锁的计时。
 *
 * 刻意不持有任何秘密：`status` 从来不带 PIN，解锁失败也不区分「PIN 错」和「没设 PIN」，
 * 免得界面反过来变成侧信道。
 */

const IDLE_KEY = 'dbhx:lock-idle-mins'

export const useLockStore = defineStore('lock', () => {
  /** 当前是否锁定。以后端为准，前端只在成功后乐观翻转本地值。 */
  const locked = ref(false)
  /** 是否设过解锁 PIN。没设过就没有锁，覆盖层与自动上锁都据此退场。 */
  const hasPin = ref(false)
  /** 空闲多少分钟后自动上锁，0 表示关闭。这是前端偏好，不进 vault。 */
  const idleMinutes = ref(readIdle())

  let idleTimer: ReturnType<typeof setTimeout> | null = null

  function clearIdle(): void {
    if (idleTimer !== null) {
      clearTimeout(idleTimer)
      idleTimer = null
    }
  }

  /** 只在「已解锁 + 设了 PIN + 开了自动锁」三者齐备时计时。 */
  function armIdle(): void {
    clearIdle()
    if (idleMinutes.value <= 0 || !hasPin.value || locked.value) return
    idleTimer = setTimeout(() => {
      idleTimer = null
      void lock()
    }, idleMinutes.value * 60_000)
  }

  /** 用户每一次可见动作都把上锁往后推；已经锁着就不用管（键盘被覆盖层吃掉了）。 */
  function activity(): void {
    if (locked.value) return
    armIdle()
  }

  async function refresh(): Promise<void> {
    const status = await lockApi.status()
    locked.value = status.locked
    hasPin.value = status.has_pin
    armIdle()
  }

  async function lock(): Promise<void> {
    if (locked.value) return
    await lockApi.lock()
    locked.value = true
    clearIdle()
  }

  /**
   * 返回是否解锁成功。错 PIN 后端回 `permission_denied`，这里折成 false 交给覆盖层
   * 显示统一提示——不把后端原文透出，免得帮攻击者判断「是不是根本没设 PIN」。
   */
  async function unlock(pin: string): Promise<boolean> {
    try {
      await lockApi.unlock(pin)
    } catch {
      return false
    }
    locked.value = false
    armIdle()
    return true
  }

  async function setPin(pin: string): Promise<void> {
    await lockApi.setPin(pin)
    hasPin.value = true
    armIdle()
  }

  async function clearPin(): Promise<void> {
    await lockApi.clearPin()
    hasPin.value = false
    locked.value = false
    clearIdle()
  }

  function setIdleMinutes(minutes: number): void {
    idleMinutes.value = Number.isInteger(minutes) && minutes >= 0 ? minutes : 0
    try {
      localStorage.setItem(IDLE_KEY, String(idleMinutes.value))
    } catch {
      /* 存不下就算了，自动锁不是必须跨重启的东西 */
    }
  }

  watch(idleMinutes, () => { armIdle() })

  return {
    locked,
    hasPin,
    idleMinutes,
    refresh,
    lock,
    unlock,
    setPin,
    clearPin,
    setIdleMinutes,
    activity,
  }
})

function readIdle(): number {
  const raw = Number(localStorage.getItem(IDLE_KEY))
  return Number.isInteger(raw) && raw >= 0 ? raw : 0
}
