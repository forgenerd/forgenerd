import type { Node, User } from './api.ts';

export const session = $state<{ user: User | null; node: Node | null }>({ user: null, node: null });
