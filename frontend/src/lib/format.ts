/** 列表里给用户看的字节数：1.4 MB 比 1468006 好读，也比 1468006 B 短。 */
export function formatSize(bytes: number | null): string {
  if (bytes === null) return '-'
  if (bytes < 1024) return `${bytes} B`
  const units = ['KB', 'MB', 'GB', 'TB']
  let value = bytes
  let index = -1
  do {
    value /= 1024
    index += 1
  } while (value >= 1024 && index < units.length - 1)
  return `${value.toFixed(value >= 10 ? 0 : 1)} ${units[index]}`
}

/** epoch 毫秒 ⇒ 本地时间。`null` 是服务端没给 mtime，显示成「未知」而不是 1970。 */
export function formatTime(ms: number | null): string {
  if (ms === null) return '未知'
  const date = new Date(ms)
  const now = new Date()
  const pad = (n: number) => String(n).padStart(2, '0')
  const time = `${pad(date.getHours())}:${pad(date.getMinutes())}`
  const sameDay = date.toDateString() === now.toDateString()
  if (sameDay) return `今天 ${time}`
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${time}`
}

/** 服务端风格路径的上一级。`/` 的上一级还是 `/`，不返回空串。 */
export function parentPath(path: string): string {
  if (path === '/' || path === '') return '/'
  const trimmed = path.endsWith('/') ? path.slice(0, -1) : path
  const cut = trimmed.lastIndexOf('/')
  return cut <= 0 ? '/' : trimmed.slice(0, cut)
}

/** 把路径拆成面包屑节点：只给 label 与跳转用的完整路径。 */
export function pathSegments(path: string): { label: string, path: string }[] {
  const out = [{ label: '/', path: '/' }]
  let cursor = ''
  for (const part of path.split('/').filter(Boolean)) {
    cursor += `/${part}`
    out.push({ label: part, path: cursor })
  }
  return out
}

/**
 * 本机路径的分隔符。
 *
 * 后端按平台原样回传绝对路径，Windows 是 `\`、其余是 `/`。用「是不是盘符开头」判断
 * 比「字符串里有没有反斜杠」稳，因为一个 Unix 目录名里合法地可以带反斜杠。
 */
export function localSep(path: string): string {
  return /^[A-Za-z]:/.test(path) ? '\\' : '/'
}

/** 本机路径的上一级。到盘符根（`C:\`）或 `/` 就返回 `null`，让「上级」按钮置灰。 */
export function localParent(path: string): string | null {
  const sep = localSep(path)
  const trimmed = path.replace(/[\\/]+$/, '')
  if (trimmed === '' || trimmed === sep) return null
  const cut = Math.max(trimmed.lastIndexOf('\\'), trimmed.lastIndexOf('/'))
  if (cut < 0) return null
  let parent = trimmed.slice(0, cut)
  if (parent === '') return sep === '/' ? '/' : null
  if (/^[A-Za-z]:$/.test(parent)) parent += '\\'
  return parent
}

/** 拼本机子路径。已带分隔符的根（`C:\`、`/`）不重复加分隔符。 */
export function localJoin(path: string, name: string): string {
  const sep = localSep(path)
  return path.endsWith(sep) || path.endsWith('\\') || path.endsWith('/') ? `${path}${name}` : `${path}${sep}${name}`
}
