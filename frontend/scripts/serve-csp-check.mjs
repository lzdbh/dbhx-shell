/**
 * 用 tauri.conf.json 里那份 CSP 头把 frontend/dist 起成静态站，检查构建产物在
 * 真实 CSP 下会不会被拦（AntDV 的 cssinjs 与 xterm 都是运行时注入 <style>）。
 *
 * 只是检查工具，不参与打包：`node scripts/serve-csp-check.mjs` 然后浏览器打开
 * http://127.0.0.1:4173 。Tauri 的 IPC 在这里必然失败，那是预期的，
 * 要看的是控制台有没有 `Refused to ... Content Security Policy`。
 */
import { createServer } from 'node:http'
import { readFile } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import path from 'node:path'

import config from '../../src-tauri/tauri.conf.json' with { type: 'json' }

const root = path.dirname(fileURLToPath(import.meta.url)) + '/../dist'
const csp = config.app.security.csp

const TYPES = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.ico': 'image/x-icon',
  '.woff2': 'font/woff2',
}

createServer(async (req, res) => {
  const url = new URL(req.url ?? '/', 'http://127.0.0.1')
  const requested = url.pathname === '/' ? '/index.html' : url.pathname
  try {
    const body = await readFile(path.join(root, requested.replace(/^\/+/, '')))
    const ext = path.extname(requested)
    res.writeHead(200, {
      'content-type': TYPES[ext] ?? 'application/octet-stream',
      'content-security-policy': csp,
    })
    res.end(body)
  }
  catch {
    res.writeHead(404, { 'content-security-policy': csp })
    res.end('not found')
  }
}).listen(4173, '127.0.0.1', () => {
  console.log(`CSP check on http://127.0.0.1:4173\n${csp}`)
})
