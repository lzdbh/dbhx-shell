/**
 * 一声短促的提示音。
 *
 * 不走 `term.write('\x07')`：BEL 到 WebView2 这一层基本什么都不会发生（不响也不闪），
 * 等于没提醒。这里直接合成一个 0.15 秒的 880Hz 正弦音，音量刻意压低——它是照着
 * 「盯日志时会连着命中」设计的东西，太响就从提醒变成打扰了。
 *
 * `AudioContext` 要等用户跟页面打过交道才允许出声（自动播放策略）。所以只在建好
 * 之后检查一次 `suspended` 并尝试恢复，恢复失败就算了：没声音不该打断终端。
 */
let audio: AudioContext | null = null

export function beep(): void {
  try {
    const Ctor = window.AudioContext
    audio ??= new Ctor()
    if (audio.state === 'suspended') void audio.resume()
    const osc = audio.createOscillator()
    const gain = audio.createGain()
    osc.frequency.value = 880
    gain.gain.value = 0.05
    osc.connect(gain)
    gain.connect(audio.destination)
    osc.start()
    osc.stop(audio.currentTime + 0.15)
  } catch {
    // 没有音频设备、或者被策略挡住：这声听不见就算了
  }
}
