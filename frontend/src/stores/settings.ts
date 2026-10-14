import { ref, watch } from 'vue'

import { defineStore } from 'pinia'

import type { HighlightRule } from '../lib/terminal/highlight'

/**
 * 本地偏好。
 *
 * 只放「前端自己要用、后端不关心」的东西，所以走 localStorage 而不是 vault：
 * vault 是加密的、跨设备会同步语义的凭据存储，字号和下载目录不属于那里。
 */
const KEY = 'dbhx:settings'

/** 字号区间与默认值。缩放（Ctrl+滚轮）和设置页的输入框共用，两边才不会各说各话。 */
export const DEFAULT_FONT_SIZE = 13
export const MIN_FONT_SIZE = 10
export const MAX_FONT_SIZE = 28

/** 关键字高亮默认空表：凭据、路径、日志格式各家不同，预置一条等于替用户决定什么算错。 */
const DEFAULT_HIGHLIGHTS: HighlightRule[] = []

/** 新增规则的起手底色：够扎眼，又不至于把整行字吃掉。 */
export const DEFAULT_HIGHLIGHT_COLOR = '#ff5252'

/** `#RRGGBB` 才收：别的写法（`red`、`rgb()`、手改过的坏数据）一律回落到默认底色。 */
function colorOr(value: unknown, fallback: string): string {
  return typeof value === 'string' && /^#[0-9a-fA-F]{6}$/.test(value) ? value : fallback
}

/**
 * 规则 id：时间戳配自增，重启后也不会和盘上读回来的旧 id 撞车。
 */
let ruleSeq = 0
function nextRuleId(): string {
  ruleSeq += 1
  return `rule-${Date.now().toString(36)}-${ruleSeq}`
}

/**
 * 盘上的一条规则还原成合法形状。
 *
 * localStorage 是可以被手工编辑、被旧版本写坏的边界，所以每个字段都要过一遍：
 * 布尔取严格 `true`，颜色过正则，`pattern` 只收字符串。认不出来的整条丢掉，
 * 丢一条比让一个 `undefined.pattern` 进编译环节再炸掉整屏好。
 */
function readRule(value: unknown): HighlightRule | null {
  if (typeof value !== 'object' || value === null) return null
  const raw = value as Record<string, unknown>
  if (typeof raw.pattern !== 'string' || raw.pattern === '') return null
  return {
    id: typeof raw.id === 'string' && raw.id !== '' ? raw.id : nextRuleId(),
    pattern: raw.pattern,
    regex: raw.regex === true,
    caseSensitive: raw.caseSensitive === true,
    background: colorOr(raw.background, DEFAULT_HIGHLIGHT_COLOR),
    bell: raw.bell === true,
    enabled: raw.enabled !== false,
  }
}

export const useSettingsStore = defineStore('settings', () => {
  /** 下载落地目录。空表示还没配——此时下载按钮会给出明确提示而不是弹系统对话框。 */
  const downloadDir = ref('')
  const fontSize = ref(DEFAULT_FONT_SIZE)
  /** 面板宽度（px）。拖过一次就记住，省得每次开应用重新调。 */
  const panelWidth = ref(360)
  /**
   * 会话日志的目录。空串表示「不指定」，由后端落在应用数据目录的 `logs` 下。
   *
   * 放在前端而不是让后端记住：换一个目录是一次纯偏好改动，不该为了它动 vault。
   * 后端只接受**绝对路径**，所以这里存什么就原样传什么，不做任何拼接。
   */
  const logDir = ref('')
  /** 关键字高亮规则，顺序即优先级：重叠时排在前面的赢。 */
  const highlightRules = ref<HighlightRule[]>([...DEFAULT_HIGHLIGHTS])

  /** 追加一条空白规则并把它交给调用方改（设置页新增按钮走的就是这里）。 */
  function addHighlightRule(): HighlightRule {
    const rule: HighlightRule = {
      id: nextRuleId(),
      pattern: '',
      regex: false,
      caseSensitive: false,
      background: DEFAULT_HIGHLIGHT_COLOR,
      bell: false,
      enabled: true,
    }
    highlightRules.value.push(rule)
    return rule
  }

  function removeHighlightRule(id: string): void {
    highlightRules.value = highlightRules.value.filter(item => item.id !== id)
  }

  /**
   * 按 `step` 档缩放字号，越界就停在边界上。
   *
   * 缩放写回的是同一个 `fontSize`：它已经是全局偏好，设置页、缩放、重开应用
   * 三条路必须指向同一个值，否则「缩完再打开设置页」会看到另一个数。
   */
  function zoom(step: number): void {
    const next = Math.round(fontSize.value + step)
    fontSize.value = Math.min(MAX_FONT_SIZE, Math.max(MIN_FONT_SIZE, next))
  }

  function resetZoom(): void {
    fontSize.value = DEFAULT_FONT_SIZE
  }

  /** 状态栏上显示的缩放百分比，以默认字号为 100%。 */
  function zoomPercent(): number {
    return Math.round(fontSize.value / DEFAULT_FONT_SIZE * 100)
  }

  function load(): void {
    const raw = localStorage.getItem(KEY)
    if (!raw) return
    try {
      const saved = JSON.parse(raw) as Partial<{ downloadDir: string, logDir: string, fontSize: number, panelWidth: number, highlightRules: unknown }>
      if (typeof saved.downloadDir === 'string') downloadDir.value = saved.downloadDir
      if (typeof saved.logDir === 'string') logDir.value = saved.logDir.trim()
      // 字号夹回区间：手工改过的 localStorage 不该让终端一开屏就是 200px
      if (typeof saved.fontSize === 'number') {
        fontSize.value = Math.min(MAX_FONT_SIZE, Math.max(MIN_FONT_SIZE, Math.round(saved.fontSize)))
      }
      if (typeof saved.panelWidth === 'number') panelWidth.value = saved.panelWidth
      if (Array.isArray(saved.highlightRules)) {
        highlightRules.value = saved.highlightRules.map(readRule).filter((item): item is HighlightRule => item !== null)
      }
    } catch {
      // 坏数据只影响偏好，不值得为它白屏；下面的 watch 会用合法值覆盖回去
    }
  }

  load()
  watch([downloadDir, logDir, fontSize, panelWidth, highlightRules], () => {
    localStorage.setItem(KEY, JSON.stringify({
      downloadDir: downloadDir.value,
      logDir: logDir.value,
      fontSize: fontSize.value,
      panelWidth: panelWidth.value,
      highlightRules: highlightRules.value,
    }))
  }, { deep: true })

  return { downloadDir, logDir, fontSize, panelWidth, highlightRules, addHighlightRule, removeHighlightRule, zoom, resetZoom, zoomPercent }
})
