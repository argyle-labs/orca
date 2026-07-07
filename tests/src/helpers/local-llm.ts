/**
 * OpenAI-compatible HTTP client for local LLM runtimes (LM Studio, Ollama).
 * No SDK dependency — raw fetch only.
 */

export interface LocalLLMConfig {
  baseUrl: string
  model: string
}

const DEFAULT_LMSTUDIO_URL = 'http://localhost:1234/v1'
const DEFAULT_OLLAMA_URL = 'http://localhost:11434/v1'

export const BACKENDS: Record<string, string> = {
  lmstudio: DEFAULT_LMSTUDIO_URL,
  ollama: DEFAULT_OLLAMA_URL,
}

export async function discoverModel(baseUrl: string): Promise<string> {
  const res = await fetch(`${baseUrl}/models`, {
    signal: AbortSignal.timeout(10_000),
  })
  if (!res.ok) throw new Error(`/models returned ${res.status}`)
  const data = (await res.json()) as { data: Array<{ id: string }> }
  if (!data.data?.length) throw new Error('No models loaded in local runtime')
  return data.data[0].id
}

export async function isReachable(baseUrl: string): Promise<boolean> {
  try {
    await fetch(`${baseUrl}/models`, { signal: AbortSignal.timeout(5_000) })
    return true
  } catch {
    return false
  }
}

export async function chatCompletion(
  baseUrl: string,
  model: string,
  prompt: string,
  system?: string,
  temperature = 0.1,
): Promise<string> {
  const messages: Array<{ role: string; content: string }> = []
  if (system) messages.push({ role: 'system', content: system })
  messages.push({ role: 'user', content: prompt })

  const res = await fetch(`${baseUrl}/chat/completions`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ model, messages, temperature, max_tokens: 1024 }),
    signal: AbortSignal.timeout(120_000),
  })

  if (!res.ok) {
    const body = await res.text()
    throw new Error(`Chat completion failed (${res.status}): ${body.slice(0, 200)}`)
  }

  const data = (await res.json()) as {
    choices: Array<{ message: { content: string } }>
  }
  return data.choices[0].message.content.trim()
}

// Judge function matching Python's PASS/FAIL prompt format, at temperature 0.0
export function makeLocalJudge(config: LocalLLMConfig) {
  return async (response: string, criteria: string): Promise<boolean> => {
    const prompt = [
      'You are a test validator. A model was asked a question and gave this response:',
      '',
      '---RESPONSE---',
      response,
      '---END RESPONSE---',
      '',
      `Criteria: ${criteria}`,
      '',
      "Does the response meet the criteria? Reply with ONLY 'PASS' or 'FAIL' followed by a one-sentence reason.",
    ].join('\n')

    const verdict = await chatCompletion(config.baseUrl, config.model, prompt, undefined, 0.0)
    return verdict.toUpperCase().startsWith('PASS')
  }
}

async function verifyChatCapable(baseUrl: string, model: string): Promise<void> {
  // Probe with a minimal request to confirm the model supports chat completions.
  // Embedding-only models will return 400 with "No models loaded" or similar.
  const res = await fetch(`${baseUrl}/chat/completions`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      model,
      messages: [{ role: 'user', content: 'hi' }],
      max_tokens: 1,
      temperature: 0,
    }),
    signal: AbortSignal.timeout(15_000),
  })
  if (!res.ok) {
    const body = await res.text()
    throw new Error(
      `Model "${model}" does not support chat completions (${res.status}). ` +
        `Load a chat model in your local runtime. Details: ${body.slice(0, 200)}`,
    )
  }
}

export async function resolveConfig(): Promise<LocalLLMConfig> {
  const baseUrl = process.env.LOCAL_LLM_URL ?? DEFAULT_LMSTUDIO_URL
  const model = process.env.LOCAL_LLM_MODEL ?? (await discoverModel(baseUrl))

  await verifyChatCapable(baseUrl, model)

  return { baseUrl, model }
}
