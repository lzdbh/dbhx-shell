/// <reference types="vite/client" />

declare module '*.vue' {
  import type { DefineComponent } from 'vue'
  // 仅语言服务用的兜底声明；vue-tsc 走 Volar 拿到真实组件类型，不受这里的宽松签名影响。
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const component: DefineComponent<any, any, any>
  export default component
}
