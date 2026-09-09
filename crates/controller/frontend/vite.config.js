import { defineConfig } from 'vite';
import vue from '@vitejs/plugin-vue';

// Base is relative so the built assets resolve from any path the daemon
// serves them at (the admin listener serves the SPA under "/" and the asset
// bundle under "/assets/"). No CDN, no absolute paths — everything is self-
// contained for the embedded, offline-capable admin panel.
export default defineConfig({
	plugins: [vue()],
	base: './',
	build: {
		outDir: 'dist',
		assetsDir: 'assets',
		sourcemap: false,
	},
	test: {
		environment: 'jsdom',
		globals: true,
		include: ['src/**/*.test.js'],
	},
});