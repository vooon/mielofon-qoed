import { defineConfig } from 'vite';
import vue from '@vitejs/plugin-vue';

// Base is relative so the built assets resolve from any path the daemon
// serves them at (the admin listener serves the SPA under "/"). The built
// JS/CSS and the bundled vis-network library all live in "assets/" — one
// self-contained directory. No CDN, no absolute paths — everything is
// offline-capable.
export default defineConfig({
	plugins: [vue()],
	base: './',
	build: {
		outDir: 'dist',
		assetsDir: 'assets',
		sourcemap: false,
	},
	// Dev: serve the SPA and proxy the admin API to a running daemon
	// (default: the loopback admin listener). Change target if your daemon
	// binds elsewhere.
	server: {
		port: 5173,
		proxy: {
			'/v1': {
				target: 'http://localhost:9553',
				changeOrigin: true,
			},
			'/metrics': {
				target: 'http://localhost:9553',
				changeOrigin: true,
			},
		},
	},
	test: {
		environment: 'jsdom',
		globals: true,
		include: ['src/**/*.test.js'],
	},
});