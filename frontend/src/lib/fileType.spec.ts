import { describe, expect, it } from 'vitest'

import { ACCENTS, iconOf, typeName } from './fileType'

/**
 * 图标分派只测「会不会看错类型」这一件事。
 *
 * 具体字形是画出来的，测不了；但目录必须和文件分家、扩展名认不出时不能抛，
 * 这两条一旦坏了，列表会整列变成同一个图标，用户只能靠猜。
 */
describe('fileType', () => {
  it('把目录排在任何扩展名之前', () => {
    expect(iconOf('logs', 'dir')).toBe('folder')
    // 名叫 app.log 的目录不能因此变成日志文件图标
    expect(iconOf('app.log', 'dir')).toBe('folder')
    expect(typeName('logs', 'dir')).toBe('文件夹')
  })

  it('认得出常见的几类', () => {
    expect(iconOf('main.rs', 'file')).toBe('code')
    expect(iconOf('index.vue', 'file')).toBe('code')
    expect(iconOf('photo.PNG', 'file')).toBe('image')
    expect(iconOf('backup.tar.gz', 'file')).toBe('archive')
    expect(iconOf('notes.md', 'file')).toBe('text')
    expect(iconOf('book.pdf', 'file')).toBe('pdf')
    expect(iconOf('installer.exe', 'file')).toBe('exe')
    expect(iconOf('id_ed25519', 'file')).toBe('key')
    expect(iconOf('Dockerfile', 'file')).toBe('code')
    expect(iconOf('.gitignore', 'file')).toBe('file')
  })

  it('符号链接自成一类', () => {
    expect(iconOf('current', 'symlink')).toBe('folder-link')
    expect(typeName('current', 'symlink')).toBe('符号链接')
  })

  it('认不出的扩展名回落到通用图标，但类型列仍然写得出后缀', () => {
    expect(iconOf('data.qqq', 'file')).toBe('file')
    expect(typeName('data.qqq', 'file')).toBe('QQQ 文件')
    expect(typeName('readme.txt', 'file')).toBe('文本文档')
  })

  it('每个图标都有主色，缺一个就会画出透明块', () => {
    const keys = new Set(Object.keys(ACCENTS))
    for (const name of ['folder', 'folder-link', 'file', 'text', 'code', 'config', 'image', 'archive',
      'audio', 'video', 'doc', 'sheet', 'slide', 'pdf', 'exe', 'key', 'font']) {
      expect(keys.has(name), name).toBe(true)
    }
  })
})
