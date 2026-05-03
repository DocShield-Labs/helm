import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const here = path.dirname(fileURLToPath(import.meta.url))

// Tauri expects a fixed dev port; bun/vite/Tauri all agree on 1420.
const host = process.env.TAURI_DEV_HOST

export default defineConfig({
  plugins: [react(), tailwindcss()],
  clearScreen: false,
  resolve: {
    alias: {
      '@':         path.resolve(here, 'src'),
      '@ui':       path.resolve(here, 'src/ui/index.ts'),
      '@ui/':      path.resolve(here, 'src/ui/'),
      '@features': path.resolve(here, 'src/features'),
      '@lib':      path.resolve(here, 'src/lib'),
      '@bindings': path.resolve(here, 'src/types/bindings.ts'),
    },
  },
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: 'ws', host, port: 1421 } : undefined,
    watch: { ignored: ['**/crates/**', '**/target/**'] },
  },
  envPrefix: ['VITE_', 'TAURI_ENV_*'],
  build: {
    target: process.env.TAURI_ENV_PLATFORM === 'windows' ? 'chrome105' : 'safari15',
    minify: !process.env.TAURI_ENV_DEBUG ? 'esbuild' : false,
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
  },
})
