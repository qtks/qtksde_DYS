<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { fetchVolumes, formatBytes, type Volume } from './api'
import SearchPanel from './components/SearchPanel.vue'
import TreePanel from './components/TreePanel.vue'
import TreemapPanel from './components/TreemapPanel.vue'
import DupPanel from './components/DupPanel.vue'
import ScrollBench from './components/ScrollBench.vue'

const params = new URLSearchParams(window.location.search)
const benchMode = params.get('bench') === 'scroll'
const benchRows = Number(params.get('rows') ?? '500000')

const volumes = ref<Volume[]>([])
const activeVolumeId = ref<string>('')
const rootPath = ref<string>('')
const selectedPath = ref<string | null>(null)
const tab = ref<'browse' | 'duplicates'>('browse')
const error = ref<string | null>(null)

const activeVolume = computed(() => volumes.value.find((volume) => volume.id === activeVolumeId.value))

async function loadVolumes() {
  try {
    const payload = await fetchVolumes()
    volumes.value = payload.volumes
    const first = payload.volumes.find((volume) => volume.status === 'online') ?? payload.volumes[0]
    if (first) {
      activeVolumeId.value = first.id
      rootPath.value = first.mount_path
      selectedPath.value = first.mount_path
    }
  } catch (reason) {
    error.value = String(reason)
  }
}

function selectVolume(id: string) {
  const volume = volumes.value.find((item) => item.id === id)
  activeVolumeId.value = id
  if (volume) {
    rootPath.value = volume.mount_path
    selectedPath.value = volume.mount_path
  }
}

onMounted(loadVolumes)
</script>

<template>
  <ScrollBench v-if="benchMode" :rows="benchRows" :autostart="true" />

  <div v-else class="app">
    <header class="topbar">
      <div class="brand">
        <h1>Panopticon <span>M2</span></h1>
        <p class="tagline">真实 M1 SQLite 索引 · Vue 3 + 虚拟滚动</p>
      </div>
      <div class="volume-picker">
        <label for="volume">卷</label>
        <select
          id="volume"
          :value="activeVolumeId"
          @change="selectVolume(($event.target as HTMLSelectElement).value)"
        >
          <option v-for="volume in volumes" :key="volume.id" :value="volume.id">
            {{ volume.name }} — {{ volume.mount_path }} ({{ volume.status }})
          </option>
        </select>
      </div>
      <div v-if="activeVolume" class="volume-stats">
        <span>{{ formatBytes(activeVolume.bytes) }}</span>
        <span>{{ activeVolume.file_count.toLocaleString() }} files</span>
        <span>{{ activeVolume.dir_count.toLocaleString() }} dirs</span>
      </div>
      <a class="bench-link" href="?bench=scroll&rows=500000">50 万行滚动实测 →</a>
    </header>

    <p v-if="error" class="error top-error">{{ error }}</p>

    <main class="layout">
      <SearchPanel class="column left" />

      <div class="column right">
        <div class="tabs">
          <button :class="{ active: tab === 'browse' }" @click="tab = 'browse'">
            目录 / Treemap
          </button>
          <button :class="{ active: tab === 'duplicates' }" @click="tab = 'duplicates'">
            重复文件
          </button>
        </div>

        <div v-if="tab === 'browse'" class="browse">
          <TreePanel :root-path="rootPath" @select="(path) => (selectedPath = path)" />
          <TreemapPanel
            :path="selectedPath"
            :root-path="rootPath"
            @select="(path) => (selectedPath = path)"
          />
        </div>
        <DupPanel v-else />
      </div>
    </main>

    <footer class="statusbar">
      <span>后端: Rust · walker-query · 127.0.0.1:8787</span>
      <span v-if="selectedPath">当前目录: {{ selectedPath }}</span>
    </footer>
  </div>
</template>
