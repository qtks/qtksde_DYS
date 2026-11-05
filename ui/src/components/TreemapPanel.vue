<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { fetchTreemap, formatBytes, type TreemapData } from '../api'
import { squarify, type LayoutRect } from '../squarify'

const props = defineProps<{ path: string | null; rootPath: string }>()
const emit = defineEmits<{ (event: 'select', path: string): void }>()

const data = ref<TreemapData | null>(null)
const error = ref<string | null>(null)
const loading = ref(false)

const WIDTH = 1000
const HEIGHT = 620

interface Cell {
  row: TreemapData['children'][number]
  rect: LayoutRect
  color: string
}

const cells = computed<Cell[]>(() => {
  const current = data.value
  if (!current || current.children.length === 0) return []
  const rects = squarify(
    current.children.map((child) => Math.max(child.bytes, 1)),
    0,
    0,
    WIDTH,
    HEIGHT,
  )
  return current.children.map((row, index) => ({
    row,
    rect: rects[index] ?? { x: 0, y: 0, w: 0, h: 0 },
    color: `hsl(${(index * 47) % 360} 55% ${row.is_dir ? 45 : 62}%)`,
  }))
})

const breadcrumbs = computed(() => {
  const root = props.rootPath
  const current = data.value?.path ?? props.path ?? root
  if (!root || !current.startsWith(root)) return [{ name: root, path: root }]
  const tail = current.slice(root.length).split('/').filter(Boolean)
  const crumbs = [{ name: root, path: root }]
  let cursor = root
  for (const part of tail) {
    cursor = `${cursor}/${part}`
    crumbs.push({ name: part, path: cursor })
  }
  return crumbs
})

async function load(path: string | null) {
  loading.value = true
  error.value = null
  try {
    data.value = await fetchTreemap(path ?? props.rootPath, 300)
  } catch (reason) {
    error.value = String(reason)
    data.value = null
  } finally {
    loading.value = false
  }
}

watch(() => props.path, (path) => load(path), { immediate: true })
</script>

<template>
  <section class="panel treemap-panel">
    <header class="panel-header">
      <h2>Treemap</h2>
      <span v-if="data" class="badge">
        {{ formatBytes(data.bytes) }} / {{ data.file_count }} files /
        {{ (data.took_us / 1000).toFixed(2) }} ms
      </span>
    </header>

    <div class="breadcrumbs">
      <span
        v-for="crumb in breadcrumbs"
        :key="crumb.path"
        class="crumb"
        @click="emit('select', crumb.path)"
      >
        {{ crumb.name }}
      </span>
    </div>

    <p v-if="error" class="error">{{ error }}</p>
    <p v-if="loading" class="note">加载中…</p>
    <p v-if="data && data.bytes === 0" class="note">
      当前目录占用为 0（M1 合成数据集的目录多为空文件），Treemap 无面积可比。
    </p>

    <svg v-if="!loading" class="treemap" :viewBox="`0 0 ${WIDTH} ${HEIGHT}`" preserveAspectRatio="none">
      <g v-for="cell in cells" :key="cell.row.id">
        <rect
          :x="cell.rect.x"
          :y="cell.rect.y"
          :width="Math.max(cell.rect.w - 1, 0)"
          :height="Math.max(cell.rect.h - 1, 0)"
          :fill="cell.color"
          class="treemap-rect"
          @click="cell.row.is_dir && emit('select', cell.row.path)"
        >
          <title>{{ cell.row.path }} — {{ formatBytes(cell.row.bytes) }}</title>
        </rect>
        <text
          v-if="cell.rect.w > 70 && cell.rect.h > 22"
          :x="cell.rect.x + 4"
          :y="cell.rect.y + 14"
          class="treemap-label"
        >
          {{ cell.row.name }}{{ cell.row.is_dir ? '/' : '' }}
        </text>
        <text
          v-if="cell.rect.w > 90 && cell.rect.h > 36"
          :x="cell.rect.x + 4"
          :y="cell.rect.y + 28"
          class="treemap-size"
        >
          {{ formatBytes(cell.row.bytes) }}
        </text>
      </g>
    </svg>

    <p v-if="data?.truncated" class="note">子项超过 300，仅展示最大的 300 个。</p>
  </section>
</template>
