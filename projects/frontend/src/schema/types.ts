// ─── Data contract ───────────────────────────────────────────────────────────
// Types that define the JSON data format passed into the visualizer.
// These are the only types that belong in the library's public API
// (re-exported via src/visualizer/index.ts as needed).

interface Column {
  name: string;
  type: string;
  pk: boolean;
  fk: boolean;
  fkTarget?: string;
}

interface Table {
  name: string;
  columns: Column[];
}

interface FK {
  from: string;
  fromCol: string;
  to: string;
}

interface Domain {
  key: string;
  label: string;
  color: string;
  tables: string[];
  group?: string;
  subgroup?: string;
}

interface ConstraintDrift {
  missingDbConstraints: { table: string; column: string; target: string }[];
  extraDbConstraints: { table: string; column: string; referencedTable: string }[];
  mismatchedFkTargets: {
    table: string;
    column: string;
    configTarget: string;
    actualTarget: string;
  }[];
}

interface DriftReport {
  unassignedTables: string[];
  ghostTables: string[];
  unmappedFkColumns: { table: string; column: string }[];
  invalidFkTargets: { column: string; target: string }[];
  constraintDrift?: ConstraintDrift;
  totalIssues: number;
}

interface TabData {
  title: string;
  tables: Table[];
  fks: FK[];
  domains: Domain[];
  drift?: DriftReport;
}

// eslint-disable-next-line @typescript-eslint/no-unused-vars
interface SchemaData {
  tabs: TabData[];
  showTabs: boolean;
}

// ─── Internal ────────────────────────────────────────────────────────────────
// Client renderer implementation details. Not part of the public API.
// Named TableNode (not Node) to avoid collision with the DOM's built-in Node type.

interface TableNode {
  id: string;
  table: Table;
  domain?: Domain;
  x: number;
  y: number;
  w: number;
  h: number;
}

interface Edge {
  source: TableNode;
  target: TableNode;
  col: string;
}

// eslint-disable-next-line @typescript-eslint/no-unused-vars
interface Cam {
  x: number;
  y: number;
  z: number;
}

// eslint-disable-next-line @typescript-eslint/no-unused-vars
interface LegendItem {
  key: string;
  label: string;
  color: string;
  groupKeys: string[];
}

type Block = {
  domain: Domain;
  nodes: TableNode[];
  w: number;
  h: number;
  x: number;
  y: number;
};

type Group = {
  key: string;
  label: string;
  color: string;
  subs: Block[];
  w: number;
  h: number;
  x: number;
  y: number;
};

// eslint-disable-next-line @typescript-eslint/no-unused-vars
interface LayoutResult {
  nodes: TableNode[];
  nodeMap: Record<string, TableNode>;
  edges: Edge[];
  blocks: Block[];
  groups: Group[];
  adj: Record<string, Edge[]>;
  adjT: Record<string, Set<string>>;
  wW: number;
  wH: number;
}
