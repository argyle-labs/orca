export interface Project {
  name: string;
  running: boolean;
  path: string;
}

export interface DockerService {
  name: string;
  state: string;
  running: boolean;
  health: string;
  ports: string[];
}

export type ControlLayerVariant = 'mcp' | 'compose' | 'container' | 'default';
