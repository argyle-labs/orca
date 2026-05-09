#!/usr/bin/env node
/**
 * orca-conformance-plugin (TypeScript) — companion to the Rust and Go
 * reference plugins. Exercises the canonical conformance scenario through
 * the TS SDK so a single host can diff observations across language ports.
 *
 * Reads four env vars set by orca_sdk::conformance::run_subprocess:
 *
 *   ORCA_PLUGIN_ADDR    — host:port of the conformance host (TCP+mTLS)
 *   ORCA_PKI_DIR        — directory holding CA + this plugin's cert/key
 *   ORCA_PLUGIN_ID      — id to claim in orca/hello (matches cert CN)
 *   ORCA_MANIFEST_PATH  — path to the canonical manifest fixture
 */
import * as manifest from '../manifest.js';
import * as pki from '../pki.js';
import { ToolHandlerError } from '../tools.js';
import { Transport } from '../transport.js';
const SCENARIO = {
    typeName: 'Greeting',
    typeSchemaVersion: '0.1.0',
    contextID: 'conformance:hello',
    manifestIDPayloadKey: 'manifest_id',
    typeSchema: {
        type: 'object',
        properties: { text: { type: 'string' }, manifest_id: { type: 'string' } },
        required: ['text', 'manifest_id'],
    },
    // Tools surface — must match projects/sdk/src/conformance.rs SCENARIO.
    toolName: 'echo',
    toolArgKey: 'value',
    toolArgValue: 'ping',
    toolResultEchoKey: 'echoed',
    toolInputSchema: {
        type: 'object',
        properties: { value: { type: 'string' } },
        required: ['value'],
    },
};
function envRequired(name) {
    const v = process.env[name];
    if (!v)
        throw new Error(`required env var ${name} not set`);
    return v;
}
async function main() {
    const addr = envRequired('ORCA_PLUGIN_ADDR');
    const pkiDir = envRequired('ORCA_PKI_DIR');
    const pluginID = envRequired('ORCA_PLUGIN_ID');
    const manifestPath = envRequired('ORCA_MANIFEST_PATH');
    const mf = await manifest.parseFile(manifestPath);
    if (mf.plugin.id !== pluginID) {
        throw new Error(`manifest plugin.id "${mf.plugin.id}" != ORCA_PLUGIN_ID "${pluginID}"`);
    }
    const bundle = await pki.loadPlugin(pkiDir, pluginID);
    const transport = await Transport.connect({ addr, bundle });
    try {
        await transport.hello(pluginID, 'headless');
        await transport.declareTypes([
            {
                type_name: SCENARIO.typeName,
                schema_version: SCENARIO.typeSchemaVersion,
                schema: SCENARIO.typeSchema,
                sensitivity: 'general',
            },
        ]);
        const value = {
            type: `${pluginID}.${SCENARIO.typeName}`,
            schema_version: SCENARIO.typeSchemaVersion,
            sensitivity: 'general',
            payload: {
                text: 'hello from the TypeScript conformance plugin',
                [SCENARIO.manifestIDPayloadKey]: mf.plugin.id,
            },
        };
        await transport.publishContext(SCENARIO.contextID, value);
        // Register the echo tool, declare it, then idle so the host's
        // tools.call can land and the read loop can serve the response.
        transport.registerTool(SCENARIO.toolName, 'echo back the value argument', SCENARIO.toolInputSchema, 'general', async (args) => {
            const obj = args;
            const v = obj?.[SCENARIO.toolArgKey];
            if (typeof v !== 'string') {
                throw new ToolHandlerError(`missing '${SCENARIO.toolArgKey}' arg`);
            }
            return { [SCENARIO.toolResultEchoKey]: v };
        });
        await transport.declareTools();
        await new Promise(resolve => setTimeout(resolve, 2000));
    }
    finally {
        transport.close();
    }
}
main().catch(err => {
    process.stderr.write(`orca-conformance-plugin (ts): ${err instanceof Error ? err.message : err}\n`);
    process.exit(1);
});
//# sourceMappingURL=conformance-plugin.js.map