/**
 * Local LLM test suite — runs the same 23 test cases against a local OpenAI-compatible
 * runtime (LM Studio or Ollama). Skip gracefully if the runtime is unreachable.
 *
 * Configuration (env vars):
 *   LOCAL_LLM_URL    Base URL (default: http://localhost:1234/v1 — LM Studio)
 *   LOCAL_LLM_MODEL  Model ID (default: auto-discovered from /models)
 *
 * Usage:
 *   npm run test:local                      # LM Studio default
 *   LOCAL_LLM_URL=http://localhost:11434/v1 npm run test:local   # Ollama
 *   LOCAL_LLM_MODEL=qwen3:14b npm run test:local                 # specific model
 */
import { describe, it, expect, beforeAll } from 'vitest'
import { readFileSync } from 'fs'
import { join, dirname } from 'path'
import { fileURLToPath } from 'url'
import {
  resolveConfig,
  isReachable,
  chatCompletion,
  makeLocalJudge,
  type LocalLLMConfig,
} from './helpers/local-llm'
import { runValidations, type ValidationRule } from './helpers/validate'
import { loadFixture } from './helpers/fixtures'

const __dirname = dirname(fileURLToPath(import.meta.url))

interface TestCase {
  id: string
  category: string
  name: string
  system_context_file?: string
  prompt: string
  validation: ValidationRule[]
}

const testCases: TestCase[] = JSON.parse(
  readFileSync(join(__dirname, '..', 'test-cases.json'), 'utf-8'),
)

let config: LocalLLMConfig
let skip = false
let skipReason = ''

beforeAll(async () => {
  const baseUrl = process.env.LOCAL_LLM_URL ?? 'http://localhost:1234/v1'

  if (!(await isReachable(baseUrl))) {
    skip = true
    skipReason = `Local LLM runtime not reachable at ${baseUrl}. Start LM Studio or Ollama, or set LOCAL_LLM_URL.`
    return
  }

  try {
    config = await resolveConfig()
    console.log(`\nLocal LLM: ${config.model} @ ${config.baseUrl}`)
  } catch (err) {
    skip = true
    skipReason = `Could not resolve local model: ${err}`
  }
})

const categories = [...new Set(testCases.map(t => t.category))]

for (const category of categories) {
  describe(category, () => {
    const cases = testCases.filter(t => t.category === category)

    for (const tc of cases) {
      it(`[${tc.id}] ${tc.name}`, async () => {
        if (skip) {
          console.warn(`SKIP: ${skipReason}`)
          return
        }

        const systemContext = tc.system_context_file
          ? loadFixture(tc.system_context_file)
          : undefined

        const response = await chatCompletion(
          config.baseUrl,
          config.model,
          tc.prompt,
          systemContext,
          0.1,
        )

        const judge = makeLocalJudge(config)
        const results = await runValidations(response, tc.validation, judge)
        const failures = results.filter(r => !r.pass)

        if (failures.length > 0) {
          const details = failures.map(f => `  - ${f.detail ?? JSON.stringify(f.rule)}`).join('\n')
          expect.fail(`Model: ${config.model}\nResponse: "${response}"\n\nFailed:\n${details}`)
        }

        expect(failures).toHaveLength(0)
      })
    }
  })
}
