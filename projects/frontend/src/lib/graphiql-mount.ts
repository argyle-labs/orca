// GraphiQL is a React-only library. Rather than reintroduce React into our
// bundle, we render it inside an iframe loaded from a CDN — graphiql brings
// its own React, and our app stays React-free.
//
// The iframe POSTs queries to `/api/specs/<repo>/graphql/proxy` on the parent
// origin (the orca server has CORS allow-any, so this works directly).

const GRAPHIQL_VERSION = '3.7.2';
const REACT_VERSION = '18.3.1';

function buildHtml(repo: string): string {
  // Absolute URL so it resolves correctly from the iframe's about:srcdoc origin.
  const proxyUrl = `${window.location.origin}/api/specs/${encodeURIComponent(repo)}/graphql/proxy`;

  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<title>GraphiQL — ${repo}</title>
<link rel="stylesheet" href="https://unpkg.com/graphiql@${GRAPHIQL_VERSION}/graphiql.min.css" />
<style>html,body,#root{height:100%;margin:0;padding:0;background:#0d1117}</style>
</head>
<body>
<div id="root" style="color:#9aa4b2;font:14px system-ui;display:flex;align-items:center;justify-content:center;height:100%">Loading GraphiQL…</div>
<script crossorigin src="https://unpkg.com/react@${REACT_VERSION}/umd/react.production.min.js"></script>
<script crossorigin src="https://unpkg.com/react-dom@${REACT_VERSION}/umd/react-dom.production.min.js"></script>
<script src="https://unpkg.com/graphiql@${GRAPHIQL_VERSION}/graphiql.min.js" crossorigin></script>
<script>
  const fetcher = GraphiQL.createFetcher({ url: ${JSON.stringify(proxyUrl)} });
  ReactDOM.createRoot(document.getElementById('root')).render(
    React.createElement(GraphiQL, { fetcher, defaultEditorToolsVisibility: true })
  );
</script>
</body>
</html>`;
}

export async function createGraphiQL(
  container: HTMLElement,
  repo: string,
): Promise<{ unmount: () => void }> {
  container.innerHTML = '';
  const iframe = document.createElement('iframe');
  iframe.style.width = '100%';
  iframe.style.height = '100%';
  iframe.style.border = '0';
  iframe.setAttribute('title', `GraphiQL — ${repo}`);
  iframe.srcdoc = buildHtml(repo);
  container.appendChild(iframe);

  return {
    unmount: () => {
      container.innerHTML = '';
    },
  };
}
