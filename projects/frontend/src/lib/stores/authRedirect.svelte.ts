import { goto } from '$app/navigation';
import { sessionSnapshot, signupStatus } from '$lib/stores/session.svelte';

const PUBLIC_ROUTES = ['/signin', '/signup'];

// Wire auth-driven redirects:
//   anonymous on private route → /signin or /signup (depending on first-user state)
//   signed-in  on public route → /
// Call once from a $effect that depends on the current pathname.
export function applyAuthRedirect(pathname: string) {
  const session = sessionSnapshot();
  const onPublicRoute = PUBLIC_ROUTES.includes(pathname);

  if (session.kind === 'anonymous' && !onPublicRoute) {
    void (async () => {
      const status = await signupStatus();
      const target = status?.allowed && status.reason === 'first_user' ? '/signup' : '/signin';
      await goto(target);
    })();
    return;
  }

  if (session.kind === 'signed-in' && onPublicRoute) {
    void goto('/');
  }
}
