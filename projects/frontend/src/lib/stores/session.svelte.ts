// Browser session store. Wraps the hey-api auth endpoints. The HttpOnly
// session cookie is set/cleared by the server; this store only mirrors the
// "who am I" result so the UI can branch on signed-in state.

import { goto } from '$app/navigation';
import {
  authChangePassword,
  authMe,
  authSignin,
  authSignout,
  authSignup,
  authSignupStatus,
} from '$lib/client/sdk.gen';
import type { MeOk, SignupStatus } from '$lib/client/types.gen';

type State = { kind: 'loading' } | { kind: 'anonymous' } | { kind: 'signed-in'; user: MeOk };

let state = $state<State>({ kind: 'loading' });

export function sessionSnapshot(): State {
  return state;
}

export async function refreshSession(): Promise<State> {
  try {
    const res = await authMe();
    if (res.response?.ok && res.data) {
      state = { kind: 'signed-in', user: res.data };
    } else {
      state = { kind: 'anonymous' };
    }
  } catch (e) {
    console.error('[session] authMe threw', e);
    state = { kind: 'anonymous' };
  }
  return state;
}

export async function signupStatus(): Promise<SignupStatus | null> {
  const res = await authSignupStatus();
  return res.data ?? null;
}

export async function signIn(username: string, password: string): Promise<void> {
  const res = await authSignin({ body: { username, password } });
  if (!res.response?.ok || !res.data) {
    const msg = (res.error as { error?: string } | undefined)?.error ?? 'sign-in failed';
    throw new Error(msg);
  }
  await refreshSession();
}

export async function signUp(username: string, password: string): Promise<void> {
  const res = await authSignup({ body: { username, password } });
  if (!res.response?.ok || !res.data) {
    const msg = (res.error as { error?: string } | undefined)?.error ?? 'sign-up failed';
    throw new Error(msg);
  }
  await refreshSession();
}

export async function changePassword(
  current_password: string,
  new_password: string,
): Promise<void> {
  const res = await authChangePassword({
    body: { current_password, new_password },
  });
  if (!res.response?.ok) {
    const msg = (res.error as { error?: string } | undefined)?.error ?? 'change password failed';
    throw new Error(msg);
  }
}

export async function signOut(): Promise<void> {
  await authSignout();
  state = { kind: 'anonymous' };
  await goto('/signin');
}
