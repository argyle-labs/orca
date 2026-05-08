export function ZoomControls({ onZoomIn, onZoomOut, onFitAll }: { onZoomIn: () => void; onZoomOut: () => void; onFitAll: () => void }) {
  return (
    <div id="zoom-controls">
      <button title="Zoom in" onClick={onZoomIn}>
        +
      </button>
      <button title="Zoom out" onClick={onZoomOut}>
        &minus;
      </button>
      <button title="Fit all" onClick={onFitAll}>
        &#x25A3;
      </button>
    </div>
  );
}
