declare module 'cytoscape-fcose' {
  import type { Ext, LayoutOptions } from 'cytoscape';

  export interface FcoseLayoutOptions extends LayoutOptions {
    name: 'fcose';
    animate?: boolean;
    randomize?: boolean;
    nodeSeparation?: number;
  }

  const fcose: Ext;
  export default fcose;
}
