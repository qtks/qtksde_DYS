<script setup lang="ts">
import { RecycleScroller } from 'vue-virtual-scroller'
import { formatBytes } from '../api'

export interface ListRow {
  path: string
  name: string
  size: number
  is_dir?: boolean
}

defineProps<{ items: ListRow[]; itemSize?: number }>()
const emit = defineEmits<{ (event: 'open', path: string): void }>()
</script>

<template>
  <div class="file-scroller">
    <RecycleScroller
      :items="items"
      :item-size="itemSize ?? 30"
      key-field="path"
      v-slot="{ item }"
    >
      <div class="file-row" @click="emit('open', item.path)">
        <span class="file-name" :title="item.path">{{ item.name }}</span>
        <span class="file-path">{{ item.path }}</span>
        <span class="file-size">{{ formatBytes(item.size) }}</span>
      </div>
    </RecycleScroller>
  </div>
</template>
