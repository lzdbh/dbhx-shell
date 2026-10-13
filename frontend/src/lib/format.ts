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
