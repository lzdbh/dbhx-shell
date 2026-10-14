/**
 * 会话日志的文件名。
 *
 * 命名规则放在前端，是因为「叫什么」是用户看得见的偏好（标签页标题就是它的输入），
 * 而「落在哪儿」不是——那由后端决定，它只接受一个**纯名字**。所以这里必须把任何
 * 可能被解释成路径的东西洗干净：分隔符、盘符冒号、`..`、控制字符、Windows 保留名。
 */

/** 时间戳用本地时间：日志是给人按的时刻，UTC 只会让「刚才那次」对不上号。 */
function stamp(at: Date): string {
  const p = (n: number, w = 2) => String(n).padStart(w, '0')
  return `${at.getFullYear()}${p(at.getMonth() + 1)}${p(at.getDate())}-${p(at.getHours())}${p(at.getMinutes())}${p(at.getSeconds())}`
}

/** Windows 的设备名。带扩展名也不行，`CON.log` 同样开不出来。 */
const RESERVED = /^(con|prn|aux|nul|com[1-9]|lpt[1-9])$/i

/**
 * 标签页标题 ⇒ 文件名的主干。
 *
 * 清洗顺序有讲究：先删非法字符，再收 '.' 和空白，最后限长——反过来会留下
 * 一个「截断之后又以 '.' 结尾」的名字，Windows 建不出来。
 */
function stem(label: string): string {
  const stripped = label
    .replace(/[\\/:*?"<>|\u0000-\u001f\u007f]/g, ' ')
    .replace(/\s+/g, '-')
    // 连续的点合成一个：`../../etc/passwd` 洗完全程若还留着 `..`，读起来就像一次 escape
    .replace(/\.{2,}/g, '.')
    .replace(/^[-.]+|[-.]+$/g, '')
  const cut = stripped.slice(0, 40).replace(/[-.]+$/g, '')
  if (cut === '' || RESERVED.test(cut)) return `dbhx-${cut || 'session'}`
  return cut
}

/** 会话日志名，形如 `dbhx-本机终端-20261014-2359.log`。 */
export function logFileName(label: string, at: Date): string {
  const name = `${stem(label)}-${stamp(at)}.log`
  // 后端的上限是 120 字节，中文标题一个字就三个字节，所以按字节而不是按字符收
  if (new TextEncoder().encode(name).length <= 120) return name
  // 砍主干、保住时间戳和扩展名：宁可名字短一截，也不要变成一个认不出的哈希
  const tail = `-${stamp(at)}.log`
  const budget = 120 - new TextEncoder().encode(tail).length
  let base = stem(label)
  while (new TextEncoder().encode(base).length > budget) base = base.slice(0, -1)
  return `${base.replace(/[-.]+$/g, '') || 'dbhx-session'}${tail}`
}
