import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

// The Rust backend owns /api; Vite proxies to it so the browser sees one origin.
export default defineConfig({
  plugins: [vue()],
  server: {
    host: '127.0.0.1',
    port: 5173,
    proxy: {
      '/api': 'http://127.0.0.1:8787',
    },
  },
  build: {
    target: 'es2020',
    outDir: 'dist',
  },
})
