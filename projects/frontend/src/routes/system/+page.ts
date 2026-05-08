import type { PageLoad } from './$types';
import { system_status_handler, listDockerRuntimes } from '$lib/api/client';

export const ssr = false;

export const load: PageLoad = async () => {
  const [status, runtimes] = await Promise.allSettled([
    system_status_handler(),
    listDockerRuntimes(),
  ]);
  return {
    status: status.status === 'fulfilled' ? status.value : null,
    runtimes: runtimes.status === 'fulfilled' ? runtimes.value : [],
  };
};
