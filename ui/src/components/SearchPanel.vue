<script setup lang="ts">
import { reactive, ref } from 'vue'
import { fetchSearch, type SearchHit, type SearchResult } from '../api'
import FileList from './FileList.vue'

const state = reactive({
  kind: 'prefix' as 'exact' | 'prefix' | 'fts',
  query: 'f000_00',
  prefixMode: 'range' as 'range' | 'like',
  rank: false,
  limit: 500,
})

const result = ref<SearchResult | null>(null)
const loading = ref(false)
const error = ref<string | null>(null)
const showPlan = ref(false)
const items = ref<SearchHit[]>([])

async function run() {
  if (!state.query) return
  loading.value = true
  error.value = null
  try {
    const response = await fetchSearch(
      state.kind,
      state.query,
      state.limit,
      state.prefixMode,
      state.rank,
    )
    result.value = response
    items.value = response.hits
  } catch (reason) {
    error.value = String(reason)
    result.value = null
    items.value = []
  } finally {
    loading.value = false
  }
}
</script>

<template>
  <section class="panel search-panel">
    <header class="panel-header">
      <h2>搜索</h2>
      <span v-if="result" class="badge">{{ result.strategy }}</span>
    </header>

    <div class="search-controls">
      <div class="segmented">
        <button :class="{ active: state.kind === 'exact' }" @click="state.kind = 'exact'">精确</button>
        <button :class="{ active: state.kind === 'prefix' }" @click="state.kind = 'prefix'">前缀</button>
        <button :class="{ active: state.kind === 'fts' }" @click="state.kind = 'fts'">FTS</button>
      </div>
      <input
        v-model="state.query"
        class="query-input"
        placeholder="文件名 / 子串（NFC 规范化）"
        @keyup.enter="run"
      />
      <button class="primary" :disabled="loading" @click="run">{{ loading ? '查询中…' : '搜索' }}</button>
    </div>

    <div class="search-options">
      <label v-if="state.kind === 'prefix'">
        前缀策略
        <select v-model="state.prefixMode">
          <option value="range">范围扫描 (BETWEEN)</option>
          <option value="like">LIKE 'x%'</option>
        </select>
      </label>
      <label v-if="state.kind === 'fts'">
        <input v-model="state.rank" type="checkbox" />
        bm25 相关度排序（慢，全量打分）
      </label>
      <span v-if="result" class="latency">
        {{ result.hits.length }} 条 / {{ (result.took_us / 1000).toFixed(2) }} ms
      </span>
    </div>

    <p v-if="error" class="error">{{ error }}</p>
    <p v-if="result?.note" class="note">{{ result.note }}</p>

    <div v-if="result" class="plan-toggle" @click="showPlan = !showPlan">
      EXPLAIN QUERY PLAN {{ showPlan ? '▾' : '▸' }}
    </div>
    <ul v-if="showPlan && result" class="plan">
      <li v-for="line in result.plan" :key="line">{{ line }}</li>
    </ul>

    <FileList :items="items" />
  </section>
</template>
