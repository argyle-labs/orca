import { describe, it, expect, beforeAll } from 'vitest'
import { readFileSync } from 'fs'
import { join, dirname } from 'path'
import { fileURLToPath } from 'url'
import Anthropic from '@anthropic-ai/sdk'
import { createClient, callModel } from './helpers/anthropic'
import { runValidations, makeAnthropicJudge, type ValidationRule } from './helpers/validate'
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

let client: Anthropic

beforeAll(() => {
  if (!process.env.ANTHROPIC_API_KEY) {
    throw new Error(
      'ANTHROPIC_API_KEY required for behavioral tests. Run: export ANTHROPIC_API_KEY=sk-...',
    )
  }
  client = createClient()
})

const categories = [...new Set(testCases.map(t => t.category))]

for (const category of categories) {
  describe(category, () => {
    const cases = testCases.filter(t => t.category === category)

    for (const tc of cases) {
      it(`[${tc.id}] ${tc.name}`, async () => {
        const systemContext = tc.system_context_file
          ? loadFixture(tc.system_context_file)
          : undefined

        const response = await callModel(client, tc.prompt, systemContext)
        const judge = makeAnthropicJudge(client)
        const results = await runValidations(response, tc.validation, judge)
        const failures = results.filter(r => !r.pass)

        if (failures.length > 0) {
          const details = failures.map(f => `  - ${f.detail ?? JSON.stringify(f.rule)}`).join('\n')
          expect.fail(`Response: "${response}"\n\nFailed:\n${details}`)
        }

        expect(failures).toHaveLength(0)
      })
    }
  })
}
