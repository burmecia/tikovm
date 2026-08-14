import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';

// Dev mode: Vite serves the SPA and proxies the webapp API to the backend.
export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      '/api': 'http://127.0.0.1:4000',
    },
  },
});
