import { events } from './bindings'
import { writeTerminal } from '../terminal/registry'
import { useFilesStore } from '../../stores/files'
import { useHostkeysStore } from '../../stores/hostkeys'
import { useHostsStore } from '../../stores/hosts'
import { useSessionsStore } from '../../stores/sessions'
import { useTransfersStore } from '../../stores/transfers'

/**
 * 事件总线 ⇒ 各 store 的唯一接线处。
 *
 * 集中在这里有两个原因：
 * - 终端字节流必须**不进任何 store**，直接 `writeTerminal`；这条最短路径一旦散到组件里，
 *   很快就会有人顺手 `ref` 住终端实例。
 * - 事件名与归属是契约的一部分，散在各组件里接线，改一个事件要翻五个文件。
 *
 * 返回卸载函数：HMR 或测试里重复注册会让同一条事件被处理两次。
 */
export async function startEventBridge(): Promise<() => void> {
  const sessions = useSessionsStore()
  const files = useFilesStore()
  const hosts = useHostsStore()
  const hostkeys = useHostkeysStore()
  const transfers = useTransfersStore()

  const unlisten = await Promise.all([
    events.termData.listen(({ payload }) => {
      writeTerminal(payload.session, payload.data)
    }),
    events.sessionCwd.listen(({ payload }) => {
      sessions.setCwd(payload.session, payload.path)
    }),
    events.sessionEnded.listen(({ payload }) => {
      sessions.markEnded(payload.session, payload.exit_status, payload.signal)
    }),
    events.sftpFollowed.listen(({ payload }) => {
      files.onFollowed(payload)
    }),
    events.sftpCwdChanged.listen(({ payload }) => {
      files.onCwdChanged(payload.connection, payload.path)
    }),
    events.hostKeyPending.listen(({ payload }) => {
      hostkeys.onHostKeyPending(payload)
    }),
    events.hostKeyConflict.listen(({ payload }) => {
      hostkeys.onHostKeyConflict(payload)
    }),
    events.passphraseRequest.listen(({ payload }) => {
      hostkeys.onPassphraseRequest(payload)
    }),
    events.transferProgress.listen(({ payload }) => {
      transfers.onProgress(payload)
    }),
    events.transferBatchEnded.listen(({ payload }) => {
      transfers.onBatchEnded(payload)
    }),
    // vault 的变更可能来自别的窗口或后端导入，计数本身就要显示在侧栏，直接重取
    events.vaultChanged.listen(() => {
      void hosts.refresh()
    }),
  ])

  return () => {
    for (const dispose of unlisten) dispose()
  }
}
