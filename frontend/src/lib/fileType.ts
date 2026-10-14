import type { FileKind } from './ipc/bindings'

/**
 * 条目的图标种类与「类型」列文案。
 *
 * 按 Windows 资源管理器的口径来：文件夹看得到颜色，文件按扩展名分派到可辨识的图标，
 * 认不出的扩展名回落到通用文件图标，但「类型」列仍写「XXX 文件」——用户靠这列
 * 判断「为什么两个同名文件图标不一样」。
 */
export type IconKey =
  | 'folder'
  | 'folder-link'
  | 'file'
  | 'text'
  | 'code'
  | 'config'
  | 'image'
  | 'archive'
  | 'audio'
  | 'video'
  | 'doc'
  | 'sheet'
  | 'slide'
  | 'pdf'
  | 'exe'
  | 'key'
  | 'font'

/** 无扩展名时按文件名认的那些。键必须全小写——查之前名字已经 lower 过了。 */
const NAMED: Record<string, IconKey> = {
  dockerfile: 'code',
  makefile: 'code',
  readme: 'text',
  authorized_keys: 'key',
  known_hosts: 'key',
  // 私钥文件名里没有后缀可认，`.pem` 那类才有
  id_rsa: 'key',
  id_ed25519: 'key',
  id_ecdsa: 'key',
}

const BY_EXT: Record<string, IconKey> = {
  txt: 'text', md: 'text', markdown: 'text', log: 'text', nfo: 'text', rtf: 'doc',
  js: 'code', cjs: 'code', mjs: 'code', jsx: 'code', ts: 'code', tsx: 'code', vue: 'code',
  rs: 'code', go: 'code', py: 'code', java: 'code', kt: 'code', swift: 'code',
  c: 'code', h: 'code', cc: 'code', cpp: 'code', hpp: 'code', cs: 'code', rb: 'code',
  php: 'code', pl: 'code', lua: 'code', sql: 'code', proto: 'code',
  sh: 'code', bash: 'code', zsh: 'code', fish: 'code', ps1: 'code', psm1: 'code',
  bat: 'code', cmd: 'code', nginx: 'code', dockerfile: 'code', tf: 'code',
  json: 'config', jsonc: 'config', yaml: 'config', yml: 'config', toml: 'config',
  ini: 'config', conf: 'config', cfg: 'config', properties: 'config', xml: 'config',
  html: 'code', htm: 'code', css: 'code', scss: 'code', less: 'code', lock: 'config',
  png: 'image', jpg: 'image', jpeg: 'image', gif: 'image', webp: 'image', avif: 'image',
  bmp: 'image', ico: 'image', tif: 'image', tiff: 'image', psd: 'image', xcf: 'image',
  svg: 'image',
  zip: 'archive', rar: 'archive', '7z': 'archive', tar: 'archive', gz: 'archive',
  bz2: 'archive', xz: 'archive', zst: 'archive', tgz: 'archive', iso: 'archive',
  apk: 'archive', jar: 'archive', war: 'archive',
  mp3: 'audio', wav: 'audio', flac: 'audio', ogg: 'audio', m4a: 'audio', aac: 'audio', opus: 'audio',
  mp4: 'video', mkv: 'video', avi: 'video', mov: 'video', webm: 'video', wmv: 'video', flv: 'video', m4v: 'video',
  doc: 'doc', docx: 'doc', odt: 'doc', pages: 'doc', xls: 'sheet', xlsx: 'sheet',
  ods: 'sheet', csv: 'sheet', tsv: 'sheet', numbers: 'sheet',
  ppt: 'slide', pptx: 'slide', odp: 'slide', key: 'slide',
  pdf: 'pdf',
  exe: 'exe', msi: 'exe', dll: 'exe', msix: 'exe', app: 'exe', deb: 'exe', rpm: 'exe', dmg: 'exe', bin: 'exe',
  pem: 'key', crt: 'key', cer: 'key', p12: 'key', pfx: 'key', id_rsa: 'key', id_ed25519: 'key', asc: 'key',
  ttf: 'font', otf: 'font', woff: 'font', woff2: 'font', fon: 'font',
}

/** 每个图标的主色。深色与浅色主题下都够醒目，所以不跟主题走。 */
export const ACCENTS: Record<IconKey, string> = {
  folder: '#e8b34b',
  'folder-link': '#5ba8f5',
  file: '#8f9bab',
  text: '#9aa7b8',
  code: '#54b3f6',
  config: '#7f8fa4',
  image: '#49c2a4',
  archive: '#c9974a',
  audio: '#d16fd1',
  video: '#e8734b',
  doc: '#4a7dd7',
  sheet: '#2fa36b',
  slide: '#d78a3c',
  pdf: '#d75454',
  exe: '#7a86c9',
  key: '#d6b24a',
  font: '#a08ac4',
}

/**
 * 远端没有扩展名的可执行位与 setuid 之类不进图标判断：这里只看名字。
 * `kind` 优先，因为把目录按扩展名上色会让用户误读成「这是个文件」。
 */
export function iconOf(name: string, kind: FileKind): IconKey {
  if (kind === 'dir') return 'folder'
  if (kind === 'symlink') return 'folder-link'
  const lower = name.toLowerCase()
  const named = NAMED[lower]
  if (named) return named
  const dot = lower.lastIndexOf('.')
  if (dot < 1) return 'file'
  return BY_EXT[lower.slice(dot + 1)] ?? 'file'
}

/** 「类型」列的文案，等价于资源管理器详情面板里那一行。 */
export function typeName(name: string, kind: FileKind): string {
  if (kind === 'dir') return '文件夹'
  if (kind === 'symlink') return '符号链接'
  const dot = name.lastIndexOf('.')
  const ext = dot < 1 ? '' : name.slice(dot + 1).toLowerCase()
  if (ext === '') return '文件'
  const labels: Record<string, string> = {
    txt: '文本文档', md: 'Markdown 文档', log: '日志文件', pdf: 'PDF 文档',
    zip: '压缩文件', rar: '压缩文件', '7z': '压缩文件', tar: '压缩文件', gz: '压缩文件',
    exe: '应用程序', msi: '安装程序', dll: '应用程序库',
  }
  return `${labels[ext] ?? `${ext.toUpperCase()} 文件`}`
}
