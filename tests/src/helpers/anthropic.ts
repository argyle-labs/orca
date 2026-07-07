import Anthropic from '@anthropic-ai/sdk'

export function createClient(): Anthropic {
  return new Anthropic({ apiKey: process.env.ANTHROPIC_API_KEY })
}

export async function callModel(
  client: Anthropic,
  prompt: string,
  systemContext?: string,
  model = 'claude-haiku-4-5-20251001',
): Promise<string> {
  const params: Anthropic.MessageCreateParamsNonStreaming = {
    model,
    max_tokens: 1024,
    messages: [{ role: 'user', content: prompt }],
  }

  if (systemContext) {
    // Cache the system context — same fixture is reused across multiple test cases
    params.system = [
      {
        type: 'text',
        text: systemContext,
        cache_control: { type: 'ephemeral' },
      } as Anthropic.TextBlockParam,
    ]
  }

  const response = await client.messages.create(params)
  return (response.content[0] as { type: 'text'; text: string }).text
}
