/**
 * 平台判断。
 *
 * `@tauri-apps/api` v2 没有 os 模块，要走 UA：这是唯一不用申请 IPC 权限、
 * 纯浏览器 dev（vite 不带 tauri）也不会炸的来源。WKWebView 必带 `Macintosh`，
 * WebView2 带 `Windows NT`，两者都不会互相误判。
 *
 * 只有窗口外观需要它——macOS 用原生红绿灯，自绘窗控在那边要整组藏掉。
 * 别拿它做功能开关：真要有「仅某平台」的行为，先想清楚是不是该由后端说。
 */
export function isMac(userAgent: string = navigator.userAgent): boolean {
  return /Macintosh|Mac OS X/.test(userAgent)
}
