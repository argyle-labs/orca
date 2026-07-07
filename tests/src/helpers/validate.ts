import type Anthropic from '@anthropic-ai/sdk'

export type ValidationRule =
  | { type: 'contains'; expected: string }
  | { type: 'not_contains'; expected: string }
  | { type: 'one_of'; expected: string[] }
  | { type: 'json_valid' }
  | { type: 'json_has_keys'; expected: string[] }
  | { type: 'llm_judge'; expected: string }

export interface ValidationResult {
  pass: boolean
  rule: ValidationRule
  detail?: string
}

// Callback signature — each test suite provides its own judge so local and
// cloud tests can self-judge with the model under test.
export type JudgeFn = (response: string, criteria: string) => Promise<boolean>

export function stripThinkAndFences(text: string): string {
  // Remove <think>...</think> blocks (reasoning model artifacts)
  let out = text.replace(/<think>[\s\S]*?<\/think>/g, '').trim()
  // Strip markdown fences (leading and trailing)
  out = out.replace(/^```[\w]*\n?/gm, '').replace(/^```$/gm, '').trim()
  return out
}

function checkContains(response: string, expected: string): ValidationResult {
  const pass = response.toLowerCase().includes(expected.toLowerCase())
  return { pass, rule: { type: 'contains', expected }, detail: pass ? undefined : `Missing "${expected}"` }
}

function checkNotContains(response: string, expected: string): ValidationResult {
  const pass = !response.toLowerCase().includes(expected.toLowerCase())
  return { pass, rule: { type: 'not_contains', expected }, detail: pass ? undefined : `Unexpectedly contains "${expected}"` }
}

function checkOneOf(response: string, expected: string[]): ValidationResult {
  const lower = response.toLowerCase()
  const pass = expected.some(e => lower.includes(e.toLowerCase()))
  return { pass, rule: { type: 'one_of', expected }, detail: pass ? undefined : `None of [${expected.join(', ')}] found` }
}

function checkJsonValid(response: string): ValidationResult {
  try {
    JSON.parse(response)
    return { pass: true, rule: { type: 'json_valid' } }
  } catch {
    return { pass: false, rule: { type: 'json_valid' }, detail: 'Not valid JSON' }
  }
}

function checkJsonHasKeys(response: string, expected: string[]): ValidationResult {
  try {
    const parsed = JSON.parse(response)
    const obj = Array.isArray(parsed) ? parsed[0] : parsed
    const missing = expected.filter(k => !(k in (obj as Record<string, unknown>)))
    const pass = missing.length === 0
    return { pass, rule: { type: 'json_has_keys', expected }, detail: pass ? undefined : `Missing keys: ${missing.join(', ')}` }
  } catch {
    return { pass: false, rule: { type: 'json_has_keys', expected }, detail: 'Cannot check keys — not valid JSON' }
  }
}

async function checkLlmJudge(judge: JudgeFn, response: string, criteria: string): Promise<ValidationResult> {
  try {
    const pass = await judge(response, criteria)
    return { pass, rule: { type: 'llm_judge', expected: criteria }, detail: pass ? undefined : 'Judge returned FAIL' }
  } catch (err) {
    return { pass: false, rule: { type: 'llm_judge', expected: criteria }, detail: `Judge error: ${err}` }
  }
}

export async function runValidations(
  rawResponse: string,
  rules: ValidationRule[],
  judge?: JudgeFn,
): Promise<ValidationResult[]> {
  const response = stripThinkAndFences(rawResponse)
  const results: ValidationResult[] = []

  for (const rule of rules) {
    switch (rule.type) {
      case 'contains':
        results.push(checkContains(response, rule.expected))
        break
      case 'not_contains':
        results.push(checkNotContains(response, rule.expected))
        break
      case 'one_of':
        results.push(checkOneOf(response, rule.expected))
        break
      case 'json_valid':
        results.push(checkJsonValid(response))
        break
      case 'json_has_keys':
        results.push(checkJsonHasKeys(response, rule.expected))
        break
      case 'llm_judge':
        if (!judge) {
          results.push({ pass: false, rule, detail: 'No judge function provided — cannot run llm_judge' })
        } else {
          results.push(await checkLlmJudge(judge, response, rule.expected))
        }
        break
    }
  }

  return results
}

// ---------------------------------------------------------------------------
// Built-in judge implementations
// ---------------------------------------------------------------------------

// Matches Python's prompt format exactly: expects PASS or FAIL verdict
export function makeAnthropicJudge(client: Anthropic, model = 'claude-haiku-4-5-20251001'): JudgeFn {
  return async (response, criteria) => {
    const result = await client.messages.create({
      model,
      max_tokens: 64,
      messages: [
        {
          role: 'user',
          content: [
            'You are a test validator. A model was asked a question and gave this response:',
            '',
            '---RESPONSE---',
            response,
            '---END RESPONSE---',
            '',
            `Criteria: ${criteria}`,
            '',
            "Does the response meet the criteria? Reply with ONLY 'PASS' or 'FAIL' followed by a one-sentence reason.",
          ].join('\n'),
        },
      ],
    })
    const verdict = (result.content[0] as { type: 'text'; text: string }).text.trim()
    return verdict.toUpperCase().startsWith('PASS')
  }
}
