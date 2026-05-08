export declare const FILENAME = "orca-plugin.toml";
export type Sensitivity = 'general' | 'sensitive';
export type RuntimeMode = 'process';
export interface PluginSection {
    id: string;
    version: string;
    min_orca_version: string;
}
export interface RuntimeSection {
    binary?: string;
    image?: string;
    mode: RuntimeMode;
    eager: boolean;
}
export interface SurfacesSection {
    mcp: boolean;
    cli: boolean;
    ui: boolean;
    docs: boolean;
    jobs: boolean;
    storage: boolean;
    federation: boolean;
}
export interface CapabilityDecl {
    name: string;
    sensitivity: Sensitivity;
}
export interface Manifest {
    plugin: PluginSection;
    runtime: RuntimeSection;
    surfaces: SurfacesSection;
    capabilities: CapabilityDecl[];
}
/** Parse a manifest from a TOML string. */
export declare function parseString(s: string): Manifest;
/** Parse a manifest from a file path. */
export declare function parseFile(path: string): Promise<Manifest>;
//# sourceMappingURL=manifest.d.ts.map