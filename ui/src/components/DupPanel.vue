<script setup lang="ts">
import { ref } from 'vue'
import { fetchDuplicates, formatBytes, type DuplicateReport } from '../api'

const report = ref<DuplicateReport | null>(null)
const loading = ref(false)
const error = ref<string | null>(null)

async function run() {
  loading.value = true
  error.value = null
  try {
    report.value = await fetchDuplicates(50)
  } catch (reason) {
    error.value = String(reason)
    report.value = null
  } finally {
    loading.value = false
  }
}
</script>

<template>
  <section class="panel dup-panel">
    <header class="panel-header">
      <h2>重复文件</h2>
      <span v-if="report" class="badge">{{ report.took_ms }} ms</span>
    </header>
    <button class="primary" :disabled="loading" @click="run">
      {{ loading ? '正在扫描…' : '运行三级漏斗' }}
    </button>
    <p class="note">
      同 size 分组 → 首尾各 4 KiB 部分哈希 → BLAKE3 全量；跳过目录、offline/missing 与硬链接。
    </p>
    <p v-if="error" class="error">{{ error }}</p>

    <div v-if="report" class="dup-summary">
      <div><strong>{{ report.duplicate_groups }}</strong> 组重复</div>
      <div><strong>{{ report.duplicate_files }}</strong> 个冗余文件</div>
      <div><strong>{{ formatBytes(report.reclaimable_bytes) }}</strong> 可回收</div>
      <div>
        IO 节省 <strong>{{ (report.io_saved_ratio * 100).toFixed(1) }}%</strong>
        （{{ formatBytes(report.io_saved_bytes) }}）
      </div>
      <div class="muted">
        候选 {{ report.candidate_files }} · 硬链接跳过 {{ report.skipped_hardlink_rows }} ·
        空文件跳过 {{ report.skipped_empty_files }} ·
        offline {{ report.skipped_offline_files }}
      </div>
      <div class="funnel">
        <span v-for="stage in report.funnel" :key="stage.stage">
          {{ stage.stage }}: {{ stage.files_in }}→{{ stage.files_out }} 淘汰
          {{ stage.eliminated }}（读 {{ formatBytes(stage.bytes_read) }}）
        </span>
      </div>
    </div>

    <div v-if="report" class="dup-groups">
      <div v-for="group in report.groups" :key="group.hash" class="dup-group">
        <div class="dup-group-header">
          {{ formatBytes(group.size) }} × {{ group.file_count }}，可回收
          {{ formatBytes(group.reclaimable_bytes) }}
        </div>
        <div v-for="file in group.files.slice(0, 6)" :key="file.id" class="dup-file">
          {{ file.path }}
        </div>
      </div>
    </div>
  </section>
</template>
