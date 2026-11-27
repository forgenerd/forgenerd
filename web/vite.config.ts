import tailwindcss from '@tailwindcss/vite';
import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

export default defineConfig({
	plugins: [
		tailwindcss(),
		sveltekit({
			compilerOptions: {
				// Force runes mode for the project, except for libraries. Can be removed in svelte 6.
				runes: ({ filename }) =>
					filename.split(/[/\\]/).includes('node_modules') ? undefined : true
			},
			// SPA build embedded into the Rust binary; the server falls back to index.html.
			adapter: adapter({ pages: 'dist', assets: 'dist', fallback: 'index.html' })
		})
	],
	server: {
		// `forgenerd server` listens on the signature port 4367.
		proxy: { '/api': { target: 'http://127.0.0.1:4367', ws: true } }
	}
});
