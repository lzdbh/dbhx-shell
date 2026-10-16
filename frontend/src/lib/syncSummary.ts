import type { SyncReport } from './ipc/bindings'

/**
 * 把一次同步的结局念成一句话。
 *
 * 顶栏那颗按钮和设置抽屉里的「立即同步」报的是同一件事，措辞必须一模一样——
 * 分头写两份的话，总有一边会先漏掉「冲突副本」或者「还缺私钥」这种尾巴。
 */
export function summarize(report: SyncReport): string {
  const head: Record<SyncReport['outcome'], string> = {
    not_configured: '同步串或令牌还没配齐，什么都没做',
    recreate_required: '云上那个片段已经不在了，需要你点头重建',
    nothing: '两边内容一致，什么都没动',
    pulled: `从云上取回 ${report.imported} 台，本机共 ${report.hosts} 台`,
    created: `已在云上建好片段，共 ${report.hosts} 台`,
    pushed: `已把本机这份推上云，共 ${report.hosts} 台`,
  }
  const notes: string[] = []
  if (report.conflicts.length > 0) notes.push(`${report.conflicts.length} 条两边都改过，已另存为「（冲突副本）」`)
  if (report.missing_identities.length > 0) notes.push(`${report.missing_identities.length} 台还缺对应的私钥`)
  if (report.missing_credentials.length > 0) notes.push(`${report.missing_credentials.length} 台要重新设一次口令`)
  return notes.length > 0 ? `${head[report.outcome]}；${notes.join('；')}` : head[report.outcome]
}

/** 「上次同步」那半句。没有跑过就不显示，而不是显示「从未」——空着更短，也不需要解释。 */
export function lastSyncAgo(at: number | null, now: number = Date.now()): string | null {
  if (at === null) return null
  const mins = Math.floor((now - at) / 60_000)
  if (mins < 1) return '上次同步：刚刚'
  if (mins < 60) return `上次同步：${mins} 分钟前`
  const hours = Math.floor(mins / 60)
  if (hours < 24) return `上次同步：${hours} 小时前`
  return `上次同步：${Math.floor(hours / 24)} 天前`
}
