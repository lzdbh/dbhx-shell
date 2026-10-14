/**
 * 关键字高亮的规则与扫描。
 *
 * 这里是**纯函数**：不碰 xterm、不碰 DOM。命中判定是这个功能唯一容易写错的地方
 * （重叠、非法正则、大小写），把它单独拎出来才能不依赖终端实例就把它测干净；
 * 上色与生命周期归 `registry.ts`。
 */

export interface HighlightRule {
  /** 稳定 id：列表项的 key，也是删改时定位的依据。 */
  id: string
  pattern: string
  /** 按正则解释 `pattern`；关掉时它按字面量匹配，方括号之类的元字符不用逃。 */
  regex: boolean
  caseSensitive: boolean
  /**
   * 底色，`#RRGGBB`。
   *
   * 只有底色没有字色：DOM 渲染器把装饰画成盖在文字上（或下面）的一个绝对定位元素，
   * 它改不了底下那些 span 的颜色。想要「把 ERROR 三个字染红」得换 canvas 渲染器，
   * 而那是整条渲染链路的改动，不该由这一个功能顺带决定。
   */
  background: string
  /** 命中时响一声。Xshell 的 keyword set 也有这一档，用来盯长跑任务里的报错。 */
  bell: boolean
  enabled: boolean
}

/** 一条编译好的规则：`re` 为 null 表示这条根本不参与匹配（关掉、空串、非法正则、没配色）。 */
export interface CompiledRule {
  rule: HighlightRule
  re: RegExp | null
}

/** 一行里的一处命中，`[start, end)` 是行内下标。 */
export interface HighlightHit {
  start: number
  end: number
  /** 命中它的规则在 `compile()` 结果里的下标，重叠时取最小的那个。 */
  rule: number
}

function escapeForLiteral(pattern: string): string {
  return pattern.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
}

/** 只收 `#RRGGBB`：别的写法（`red`、`rgb()`、手改过的坏数据）一律当没配色，宁可不上色也别画花。 */
function isColor(value: string): boolean {
  return /^#[0-9a-fA-F]{6}$/.test(value)
}

/**
 * 把规则编译成正则。
 *
 * 非法正则是常态（用户边写边改，`err(` 这种半截随时在），所以这里不抛出去：
 * 编不出来的规则当作没启用，下一次改对了自然又生效。空串一律不编——它会命中
 * 每一个字符间隙，等于把整屏刷成一块颜色。
 */
export function compile(rules: HighlightRule[]): CompiledRule[] {
  return rules.map(item => {
    if (!item.enabled || item.pattern === '' || !isColor(item.background)) return { rule: item, re: null }
    const source = item.regex ? item.pattern : escapeForLiteral(item.pattern)
    const flags = item.caseSensitive ? 'g' : 'gi'
    try {
      return { rule: item, re: new RegExp(source, flags) }
    } catch {
      return { rule: item, re: null }
    }
  })
}

/**
 * 扫一行，返回按起点排序、互不重叠的命中。
 *
 * 重叠时先到先得——规则的顺序就是优先级，列表里排在上面的赢。这比「后写的覆盖
 * 先写的」更符合「我把 FATAL 放在 ERROR 前面」的直觉。
 */
export function scanLine(line: string, compiled: CompiledRule[]): HighlightHit[] {
  if (line === '') return []
  const hits: HighlightHit[] = []
  const taken: boolean[] = new Array(line.length).fill(false)
  compiled.forEach((entry, index) => {
    const re = entry.re
    if (re === null) return
    re.lastIndex = 0
    for (let match = re.exec(line); match !== null; match = re.exec(line)) {
      // 零宽匹配（`x*` 之类）不会自己前进，不挪一下就是一条死循环
      if (match[0] === '') {
        re.lastIndex += 1
        continue
      }
      const start = match.index
      const end = start + match[0].length
      if (taken.slice(start, end).some(Boolean)) continue
      for (let i = start; i < end; i += 1) taken[i] = true
      hits.push({ start, end, rule: index })
    }
  })
  return hits.sort((a, b) => a.start - b.start)
}

/** 这一批规则里有没有需要响铃的，供上层决定要不要盯着新输出。 */
export function hasBell(compiled: CompiledRule[]): boolean {
  return compiled.some(entry => entry.re !== null && entry.rule.bell)
}
