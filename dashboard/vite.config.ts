import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import { visualizer } from 'rollup-plugin-visualizer'

// https://vite.dev/config/
export default defineConfig({
  plugins: [
    react(),
    visualizer({
      filename: 'bundle-stats.html',
      open: false,
    }),
  ],
  build: {
    rollupOptions: {
      output: {
        manualChunks(id: string | null) {
          if (!id) return
          if (id.includes('node_modules')) {
            if (id.includes('react-router') || id.includes('@remix-run')) return 'router'
            if (id.includes('@tanstack')) return 'tanstack'
            if (id.includes('cmdk') || id.includes('radix') || id.includes('lucide')) return 'ui'
            return 'vendor'
          }
        },
      },
    },
  },
})
