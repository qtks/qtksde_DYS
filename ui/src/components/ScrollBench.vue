<script setup lang="ts">
import { nextTick, onMounted, shallowRef, ref } from 'vue'
import { RecycleScroller } from 'vue-virtual-scroller'
import { fetchFlatRows, formatBytes, type FlatRow } from '../api'

const props = defineProps<{ rows: number; autostart: boolean }>()

const items = shallowRef<FlatRow[]>([])
const status = ref('加载真实索引行…')
const loading = ref(true)
const running = ref(false)
const loadMs = ref(0)
const report = ref<{
  rows: number
  frames: number
  duration_ms: number
  avg_frame_ms: number
  p95_frame_ms: number
  worst_frame_ms: number
  dropped_33ms: number
  jank_50ms: number
  fps: number
  load_ms: number
  saved: string | null
  max_scroll: number
  scroll_height: number
  warmup_worst_ms: number
  viewport: string
  valid: boolean
} | null>(null)

onMounted(async () => {
  const started = performance.now()
  try {
    items.value = await fetchFlatRows(props.rows)
  } catch (reason) {
    status.value = `加载失败: ${reason}`
    loading.value = false
    return
  }
  loadMs.value = performance.now() - started
  loading.value = false
  status.value = `已加载 ${items.value.length} 条真实索引行（${loadMs.value.toFixed(0)} ms）`
  await nextTick()
  if (props.autostart) void start()
})

async function start() {
  if (running.value || items.value.length === 0) return
  running.value = true
  report.value = null
  await nextTick()
  const element = document.querySelector<HTMLElement>('.bench-scroller .vue-recycle-scroller')
  if (element) {
    element.style.height = '420px'
    element.style.minHeight = '420px'
    element.style.overflowY = 'auto'
    element.scrollTop = 0
  }
  const frames: number[] = []
  let offset = 0
  let direction = 1
  let observedMax = 0
  let warmupWorst = 0
  let warmup = true
  let last = performance.now()
  let started = last
  const duration = 10000
  const warmupMs = 2000

  const finish = (finalFrames: number[]) => {
    const samples = finalFrames.slice()
    finalFrames.sort((left, right) => left - right)
    const total = finalFrames.reduce((sum, value) => sum + value, 0)
    const avg = total / finalFrames.length
    const p95 = finalFrames[Math.floor(finalFrames.length * 0.95)] ?? 0
    const worst = finalFrames[finalFrames.length - 1] ?? 0
    const dropped = finalFrames.filter((value) => value > 33.4).length
    const jank = finalFrames.filter((value) => value > 50).length
    report.value = {
      rows: items.value.length,
      frames: finalFrames.length,
      duration_ms: total,
      avg_frame_ms: Number(avg.toFixed(2)),
      p95_frame_ms: Number(p95.toFixed(2)),
      worst_frame_ms: Number(worst.toFixed(2)),
      dropped_33ms: dropped,
      jank_50ms: jank,
      fps: Number((1000 / avg).toFixed(1)),
      load_ms: Number(loadMs.value.toFixed(0)),
      saved: null,
      max_scroll: Math.round(observedMax),
      scroll_height: element ? element.scrollHeight : 0,
      warmup_worst_ms: Number(warmupWorst.toFixed(2)),
      viewport: elementSize(),
      valid: finalFrames.length >= 300 && observedMax > 1000,
    }
    void save(samples, total, dropped, jank, avg, p95, worst)
    running.value = false
  }

  const tick = (now: number) => {
    const delta = now - last
    last = now
    if (warmup) {
      warmupWorst = Math.max(warmupWorst, delta)
      if (now - started >= warmupMs) {
        warmup = false
        started = now
        last = now
      }
    } else {
      frames.push(delta)
    }
    if (element) {
      const max = element.scrollHeight - element.clientHeight
      offset += 2400 * direction
      if (offset >= max) {
        offset = max
        direction = -1
      } else if (offset <= 0) {
        offset = 0
        direction = 1
      }
      element.scrollTop = offset
      observedMax = Math.max(observedMax, element.scrollTop)
    }
    if (warmup || now - started < duration) {
      requestAnimationFrame(tick)
    } else {
      finish(frames)
    }
  }
  requestAnimationFrame((now) => {
    last = now
    started = now
    requestAnimationFrame(tick)
  })
  status.value = `预热 ${warmupMs / 1000} 秒，随后以 2400 px/帧 连续滚动 10 秒…`
}

async function save(
  samples: number[],
  total: number,
  dropped: number,
  jank: number,
  avg: number,
  p95: number,
  worst: number,
) {
  const payload = {
    name: `scroll-${items.value.length}`,
    rows: items.value.length,
    frames: report.value?.frames,
    samples,
    warmup_worst_ms: report.value?.warmup_worst_ms,
    duration_ms: Number(total.toFixed(1)),
    avg_frame_ms: Number(avg.toFixed(2)),
    p95_frame_ms: Number(p95.toFixed(2)),
    worst_frame_ms: Number(worst.toFixed(2)),
    dropped_33ms: dropped,
    jank_50ms: jank,
    fps: Number((1000 / avg).toFixed(1)),
    load_ms: Number(loadMs.value.toFixed(0)),
    max_scroll: report.value?.max_scroll,
    scroll_height: report.value?.scroll_height,
    valid: report.value?.valid,
    viewport: elementSize(),
    window_size: `${window.innerWidth}x${window.innerHeight}`,
  }
  try {
    const response = await fetch('/api/bench-report', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload),
    })
    const result = (await response.json()) as { saved: string }
    if (report.value) report.value.saved = result.saved
    status.value = `结果已写入 ${result.saved}`
  } catch (reason) {
    status.value = `保存失败: ${reason}`
  }
}

function elementSize() {
  const element = document.querySelector<HTMLElement>('.bench-scroller .vue-recycle-scroller')
  return element ? `${element.clientWidth}x${element.clientHeight}` : 'unknown'
}
</script>

<template>
  <div class="bench">
    <header class="bench-header">
      <h1>虚拟滚动实机基准（真实 M1 索引行）</h1>
      <p>{{ status }}</p>
      <button class="primary" :disabled="running || loading" @click="start">
        {{ running ? '测试中…' : '重新测试' }}
      </button>
    </header>

    <div v-if="report" class="bench-report">
      <div v-if="!report.valid" class="error">
        本次结果无效：rAF 被节流或列表未滚动（帧数 {{ report.frames }}，滚动
        {{ report.max_scroll }} px），请在前台窗口重跑。
      </div>
      <div><strong>{{ report.rows.toLocaleString() }}</strong> rows</div>
      <div><strong>{{ report.frames }}</strong> frames / {{ report.duration_ms.toFixed(0) }} ms</div>
      <div>avg <strong>{{ report.avg_frame_ms }} ms</strong>（{{ report.fps }} fps）</div>
      <div>p95 <strong>{{ report.p95_frame_ms }} ms</strong></div>
      <div>worst <strong>{{ report.worst_frame_ms }} ms</strong></div>
      <div>
        掉帧(>33ms) <strong>{{ report.dropped_33ms }}</strong> · 卡顿(&gt;50ms)
        <strong>{{ report.jank_50ms }}</strong>
      </div>
      <div>
        视口 <strong>{{ report.viewport }}</strong> · 内容高度
        <strong>{{ report.scroll_height.toLocaleString() }} px</strong> · 实际滚动距离
        <strong>{{ report.max_scroll.toLocaleString() }} px</strong>
      </div>
      <div class="muted">
        预热期最差帧 {{ report.warmup_worst_ms }} ms · 列表加载 {{ report.load_ms }} ms
      </div>
      <div v-if="report.saved" class="muted">已保存: {{ report.saved }}</div>
    </div>

    <div class="file-scroller bench-scroller">
      <RecycleScroller
        :items="items"
        :item-size="32"
        key-field="path"
        v-slot="{ item }"
      >
        <div class="file-row">
          <span class="file-name">{{ item.name }}</span>
          <span class="file-path">{{ item.path }}</span>
          <span class="file-size">{{ formatBytes(item.size) }}</span>
        </div>
      </RecycleScroller>
    </div>
  </div>
</template>
