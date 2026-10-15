import { ref } from 'vue'

import { defineStore } from 'pinia'

/**
 * 快捷命令栏：一个输入框，回车把整行发给会话，上下键翻历史。
 *
 * 历史**只活在本次运行内**，故意不落盘。这个框存在的意义就是敲一条要发出去的
 * 命令，而命令里完全可能出现 `mysql -p...`、`curl -H 'Authorization: ...'` 这类
 * 一次性凭据——把它们写进 localStorage 等于把口令明文留在磁盘上，跟我们把凭据
 * 关进加密 vault 的立场正好相反。重启就清空，是这一步要的。
 */

/** 每个归属记多少条。再多就不是「翻上几条历史」而是翻档案了。 */
const LIMIT = 50

export const useQuickStore = defineStore('quick', () => {
  /** 全局显示/隐藏：分屏时两格都出条，各发各的会话。 */
  const visible = ref(false)
  /** 按归属分组的历史，新的在前。`null` 归属（还没会话的格子）不会进来。 */
  const history = ref<Record<string, string[]>>({})

  function toggle(): void {
    visible.value = !visible.value
  }

  function entries(owner: string): string[] {
    return history.value[owner] ?? []
  }

  /**
   * 记一条。重复命令挪到最前而不是丢掉：连着敲同一条时，用户按一次 ↑ 想要的
   * 就是刚刚那条，留在原位等于没记。
   */
  function remember(owner: string, command: string): void {
    const trimmed = command.trim()
    if (trimmed === '') return
    const list = history.value[owner] ?? []
    const kept = [trimmed, ...list.filter(item => item !== trimmed)]
    history.value[owner] = kept.slice(0, LIMIT)
  }

  /**
   * 清空某个归属的历史——右键菜单里「清空本机历史」走这里。
   *
   * 按归属清而不是全清：分屏时两条命令栏各发各的机器，用户对着 A 机器的栏子
   * 按清空，不该顺手把 B 机器的也抹了。
   */
  function clear(owner: string): void {
    delete history.value[owner]
  }

  return { visible, toggle, entries, remember, clear }
})
