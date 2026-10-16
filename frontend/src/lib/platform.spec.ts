import { describe, expect, it } from 'vitest'

import { isMac } from './platform'

describe('isMac', () => {
  it('认出 WKWebView 的 macOS UA', () => {
    expect(isMac('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko)'))
      .toBe(true)
  })

  it('不把 WebView2 / WebKitGTK 当成 macOS', () => {
    expect(isMac('Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36'))
      .toBe(false)
    expect(isMac('Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/4.0 Safari/605.1.15'))
      .toBe(false)
  })

  it('不传参数时读当前环境，且纯浏览器下不抛', () => {
    expect(() => isMac()).not.toThrow()
  })
})
