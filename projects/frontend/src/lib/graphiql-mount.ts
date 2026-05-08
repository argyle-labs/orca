import 'graphiql/style.css';
import React from 'react';
import { createRoot } from 'react-dom/client';
import { GraphiQL } from 'graphiql';
import { createGraphiQLFetcher } from '@graphiql/toolkit';
import { buildSchema, type GraphQLSchema } from 'graphql';

export async function createGraphiQL(container: HTMLElement, repo: string) {
  // Load SDL from disk for local schema introspection (no live endpoint needed)
  let schema: GraphQLSchema | undefined;
  try {
    const res = await fetch(`/api/specs/${repo}/graphql`);
    if (res.ok) {
      const sdl = await res.text();
      schema = buildSchema(sdl);
    }
  } catch {
    // schema stays undefined — GraphiQL will show empty docs
  }

  const fetcher = createGraphiQLFetcher({ url: `/api/specs/${repo}/graphql/proxy` });

  const root = createRoot(container);
  root.render(React.createElement(GraphiQL, { fetcher, schema }));
  return { unmount: () => root.unmount() };
}
