<script setup lang="ts">
import { ref, watch } from 'vue'
import { fetchChildren, formatBytes, type ChildRow } from '../api'

interface Node {
  path: string
  name: string
  depth: number
  is_dir: boolean
  bytes: number
  expanded: boolean
  loading: boolean
  loaded: boolean
}

const props = defineProps<{ rootPath: string }>()
const emit = defineEmits<{ (event: 'select', path: string): void }>()

const nodes = ref<Node[]>([])
const error = ref<string | null>(null)

function makeNode(row: ChildRow, depth: number): Node {
  return {
    path: row.path,
    name: row.name,
    depth,
    is_dir: row.is_dir,
    bytes: row.bytes,
    expanded: false,
    loading: false,
    loaded: false,
  }
}

async function loadRoot(path: string) {
  nodes.value = []
  error.value = null
  try {
    const page = await fetchChildren(path, 500, 'size')
    nodes.value = page.children.map((row) => makeNode(row, 0))
    emit('select', page.path)
  } catch (reason) {
    error.value = String(reason)
  }
}

async function toggle(node: Node, index: number) {
  if (!node.is_dir) {
    emit('select', node.path)
    return
  }
  if (node.expanded) {
    collapse(node)
    return
  }
  node.expanded = true
  if (node.loaded) {
    emit('select', node.path)
    return
  }
  node.loading = true
  try {
    const page = await fetchChildren(node.path, 500, 'size')
    const children = page.children.map((row) => makeNode(row, node.depth + 1))
    const insertAt = index + 1
    nodes.value.splice(insertAt, 0, ...children)
    node.loaded = true
    emit('select', node.path)
  } catch (reason) {
    error.value = String(reason)
    node.expanded = false
  } finally {
    node.loading = false
  }
}

function collapse(node: Node) {
  node.expanded = false
  const start = nodes.value.indexOf(node) + 1
  let end = start
  while (end < nodes.value.length && nodes.value[end].depth > node.depth) {
    end += 1
  }
  nodes.value.splice(start, end - start)
  node.loaded = false
}

watch(
  () => props.rootPath,
  (path) => {
    if (path) loadRoot(path)
  },
  { immediate: true },
)
</script>

<template>
  <section class="panel tree-panel">
    <header class="panel-header">
      <h2>目录树（懒加载）</h2>
      <span class="badge">点击目录加载子节点</span>
    </header>
    <p v-if="error" class="error">{{ error }}</p>
    <div class="tree-list">
      <div
        v-for="(node, index) in nodes"
        :key="node.path"
        class="tree-row"
        :class="{ directory: node.is_dir }"
        :style="{ paddingLeft: `${8 + node.depth * 16}px` }"
        @click="toggle(node, index)"
      >
        <span class="tree-icon">{{ node.is_dir ? (node.expanded ? '▾' : '▸') : '·' }}</span>
        <span class="tree-name" :title="node.path">{{ node.name }}</span>
        <span class="tree-size">{{ formatBytes(node.bytes) }}</span>
      </div>
    </div>
  </section>
</template>
