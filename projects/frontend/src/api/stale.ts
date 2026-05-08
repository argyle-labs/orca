/** Returns 0 in dev (always fresh) and the given ms in production. */
export const staleMs = (ms: number): number => (import.meta.env.DEV ? 0 : ms);
