export interface Volume {
  id: string
  name: string
  mount_path: string
  status: string
  bytes: number
  file_count: number
  dir_count: number
}

export interface SearchHit {
  id: string
  path: string
  name: string
  size: number
  is_dir: boolean
  mtime: number
}

export interface SearchResult {
  kind: string
  strategy: string
  norm_query: string
  limit: number
  offset: number
  hits: SearchHit[]
  took_us: number
  plan: string[]
  note: string | null
}

export interface ChildRow {
  id: string
  name: string
  path: string
  is_dir: boolean
  bytes: number
  size: number
  file_count: number
  mtime: number
}

export interface ChildPage {
  path: string
  parent_id: string
  children: ChildRow[]
  truncated: boolean
  took_us: number
}

export interface TreemapRow {
  id: string
  name: string
  path: string
  is_dir: boolean
  bytes: number
  file_count: number
}

export interface TreemapData {
  id: string
  path: string
  is_dir: boolean
  bytes: number
  file_count: number
  dir_count: number
  children: TreemapRow[]
  took_us: number
  truncated: boolean
}

export interface DuplicateFile {
  id: string
  path: string
  volume_id: string
  size: number
}

export interface DuplicateGroup {
  size: number
  hash: string
  file_count: number
  reclaimable_bytes: number
  files: DuplicateFile[]
  members_truncated: boolean
}

export interface FunnelStage {
  stage: string
  files_in: number
  files_out: number
  eliminated: number
  bytes_read: number
  elapsed_ms: number
}

export interface DuplicateReport {
  total_online_files: number
  skipped_empty_files: number
  skipped_offline_files: number
  skipped_missing_files: number
  skipped_unreadable_files: number
  skipped_hardlink_rows: number
  candidate_size_groups: number
  candidate_files: number
  partial_hashed: number
  partial_bytes: number
  partial_eliminated: number
  full_hashed: number
  full_bytes: number
  full_eliminated: number
  full_reused: number
  duplicate_groups: number
  duplicate_files: number
  reclaimable_bytes: number
  naive_full_hash_bytes: number
  io_saved_bytes: number
  io_saved_ratio: number
  partial_bytes_setting: number
  took_ms: number
  funnel: FunnelStage[]
  groups: DuplicateGroup[]
  errors: string[]
}

export interface FlatRow {
  name: string
  path: string
  size: number
  is_dir: boolean
  mtime: number
}

async function getJson<T>(url: string): Promise<T> {
  const response = await fetch(url)
  if (!response.ok) {
    const text = await response.text()
    throw new Error(`HTTP ${response.status}: ${text.slice(0, 200)}`)
  }
  return (await response.json()) as T
}

export function fetchVolumes(): Promise<{ volumes: Volume[] }> {
  return getJson('/api/volumes')
}

export function fetchSearch(
  kind: 'exact' | 'prefix' | 'fts',
  query: string,
  limit: number,
  prefixMode: 'range' | 'like' = 'range',
  rank = false,
): Promise<SearchResult> {
  const params = new URLSearchParams({ kind, q: query, limit: String(limit), prefix_mode: prefixMode })
  if (rank) params.set('rank', '1')
  return getJson(`/api/search?${params.toString()}`)
}

export function fetchChildren(
  path: string,
  limit = 500,
  sort: 'size' | 'name' = 'size',
): Promise<ChildPage> {
  const params = new URLSearchParams({ path, limit: String(limit), sort })
  return getJson(`/api/children?${params.toString()}`)
}

export function fetchTreemap(path: string | null, limit = 200): Promise<TreemapData> {
  const params = new URLSearchParams({ limit: String(limit) })
  if (path) params.set('path', path)
  return getJson(`/api/treemap?${params.toString()}`)
}

export function fetchDuplicates(limit = 50): Promise<DuplicateReport> {
  return getJson(`/api/duplicates?limit=${limit}`)
}

export async function fetchFlatRows(limit: number): Promise<FlatRow[]> {
  const response = await fetch(`/api/files?limit=${limit}`)
  if (!response.ok) throw new Error(`HTTP ${response.status}`)
  const payload = (await response.json()) as {
    rows: [string, string, number, number, number][]
    count: number
    took_ms: number
  }
  const rows: FlatRow[] = []
  for (const [path, name, size, isDir, mtime] of payload.rows) {
    rows.push({ path, name, size, is_dir: isDir !== 0, mtime })
  }
  return rows
}

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes)) return '0 B'
  if (bytes >= 1024 ** 4) return `${(bytes / 1024 ** 4).toFixed(2)} TB`
  if (bytes >= 1024 ** 3) return `${(bytes / 1024 ** 3).toFixed(2)} GB`
  if (bytes >= 1024 ** 2) return `${(bytes / 1024 ** 2).toFixed(2)} MB`
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KB`
  return `${bytes} B`
}

export function basename(path: string): string {
  const parts = path.split('/')
  return parts[parts.length - 1] || path
}
