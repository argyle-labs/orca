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
/**
 * `[[depends_on]]` — peer plugin this plugin needs at runtime. The host
 * enforces presence of required deps before the plugin's tools are
 * dispatchable; optional deps degrade rather than reject.
 */
export interface PluginDependency {
    id: string;
    min_version: string;
    /** Default false — required. */
    optional?: boolean;
}
export interface Manifest {
    plugin: PluginSection;
    runtime: RuntimeSection;
    surfaces: SurfacesSection;
    capabilities: CapabilityDecl[];
    depends_on: PluginDependency[];
}
/** Parse a manifest from a TOML string. */
export declare function parseString(s: string): Manifest;
/**
 * Format a {@link PluginDependency} for HelloOptions.pluginsRequired /
 * pluginsOptional — `<id>>=<min>`.
 */
export declare function formatDep(dep: PluginDependency): string;
/** Required deps formatted for HelloOptions.pluginsRequired. */
export declare function requiredDeps(m: Manifest): string[];
/** Optional deps formatted for HelloOptions.pluginsOptional. */
export declare function optionalDeps(m: Manifest): string[];
/** Parse a manifest from a file path. */
export declare function parseFile(path: string): Promise<Manifest>;
//# sourceMappingURL=manifest.d.ts.map