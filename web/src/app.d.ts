// See https://svelte.dev/docs/kit/types#app.d.ts
// for information about these interfaces
import type { BlueprintDeployed } from './lib/api';

declare global {
	namespace App {
		// interface Error {}
		// interface Locals {}
		// interface PageData {}
		interface PageState {
			blueprintDeployment?: BlueprintDeployed;
		}
		// interface Platform {}
	}
}

export {};
