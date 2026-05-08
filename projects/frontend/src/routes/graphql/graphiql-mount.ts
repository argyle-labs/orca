import 'graphiql/style.css';
import React from 'react';
import { createRoot } from 'react-dom/client';
import { GraphiQL } from 'graphiql';
import { createGraphiQLFetcher } from '@graphiql/toolkit';

export function fetcherForRepo(repo: string) {
  return createGraphiQLFetcher({ url: `/api/specs/${repo}/graphql/proxy` });
}

export function createGraphiQL(container: HTMLElement, fetcher: ReturnType<typeof createGraphiQLFetcher>) {
  const root = createRoot(container);
  root.render(React.createElement(GraphiQL, { fetcher }));
  return { unmount: () => root.unmount() };
}
