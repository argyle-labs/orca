/**
 * Structural tests — no LLM required. Verifies the brain vault infrastructure:
 * file existence, frontmatter validity, cross-references, memory index coherence.
 */
import { describe, it, expect } from 'vitest'
import { readFileSync, existsSync, readdirSync } from 'fs'
import { join } from 'path'
import os from 'os'

const HOME = os.homedir()
const BRAIN = join(HOME, 'brain', 'ai', 'claude')
const AGENTS = join(BRAIN, 'agents')
const COMMANDS = join(BRAIN, 'commands')
const homeSlug = HOME.replace(/\//g, '-').replace(/^-/, '')
const MEMORY = join(HOME, '.claude', 'projects', `-${homeSlug}-code-example`, 'memory')

function read(path: string): string {
  return readFileSync(path, 'utf-8')
}

function exists(path: string): boolean {
  return existsSync(path)
}

// ---------------------------------------------------------------------------
// Shared reference docs
// ---------------------------------------------------------------------------
describe('shared reference docs', () => {
  const sharedDocs = [
    'DELEGATION.md',
    'SEVERITY_RUBRIC.md',
    'TOOL_RULES.md',
    'CANONICAL_SOURCES.md',
  ]

  for (const doc of sharedDocs) {
    it(`${doc} exists and is non-empty`, () => {
      const path = join(BRAIN, doc)
      expect(exists(path), `${doc} not found at ${path}`).toBe(true)
      expect(read(path).trim().length, `${doc} is empty`).toBeGreaterThan(0)
    })
  }
})

// ---------------------------------------------------------------------------
// Agent frontmatter
// ---------------------------------------------------------------------------
describe('agent frontmatter', () => {
  const agentFiles = readdirSync(AGENTS).filter(f => f.endsWith('.md'))

  for (const file of agentFiles) {
    it(`${file} has valid frontmatter with name, description, tools`, () => {
      const content = read(join(AGENTS, file))
      expect(content.startsWith('---'), `${file}: missing opening --- frontmatter delimiter`).toBe(true)

      const end = content.indexOf('---', 3)
      expect(end, `${file}: missing closing --- frontmatter delimiter`).toBeGreaterThan(3)

      const frontmatter = content.slice(3, end)
      expect(frontmatter, `${file}: missing "name:" field`).toMatch(/^name:\s*.+/m)
      expect(frontmatter, `${file}: missing "description:" field`).toMatch(/^description:\s*.+/m)
      expect(frontmatter, `${file}: missing "tools:" field`).toMatch(/^tools:\s*.+/m)
    })
  }
})

// ---------------------------------------------------------------------------
// Workflow skills (commands)
// ---------------------------------------------------------------------------
describe('workflow skills (commands)', () => {
  const required = [
    'survey-confirm-fix.md',
    'lint-workflow.md',
    'typecheck-workflow.md',
    'pr-review-format.md',
  ]

  for (const cmd of required) {
    it(`${cmd} exists`, () => {
      const path = join(COMMANDS, cmd)
      expect(exists(path), `Workflow skill not found: ${path}`).toBe(true)
    })
  }
})

// ---------------------------------------------------------------------------
// Example platform context skills
// ---------------------------------------------------------------------------
describe('example context skills', () => {
  const contextSkills = [
    'example-engine-context.md',
    'example-db-context.md',
    'example-cli-context.md',
    'example-admin-nextjs-context.md',
    'example-admin-api-context.md',
    'example-onsite-context.md',
    'example-installer-context.md',
    'example-env.md',
    'example-pr.md',
    'example-migrate.md',
  ]

  for (const skill of contextSkills) {
    it(`${skill} exists`, () => {
      const path = join(COMMANDS, skill)
      expect(exists(path), `Context skill not found: ${path}`).toBe(true)
    })
  }
})

// ---------------------------------------------------------------------------
// Example platform agents
// ---------------------------------------------------------------------------
describe('example platform agents', () => {
  const agents = ['example-kb.md', 'example-deploy.md', 'example-migrate.md']

  for (const agent of agents) {
    it(`${agent} exists`, () => {
      const path = join(AGENTS, agent)
      expect(exists(path), `Example agent not found: ${path}`).toBe(true)
    })
  }
})

// ---------------------------------------------------------------------------
// Memory index coherence
// ---------------------------------------------------------------------------
describe('memory index coherence', () => {
  it('MEMORY.md exists', () => {
    expect(exists(join(MEMORY, 'MEMORY.md'))).toBe(true)
  })

  it('all MEMORY.md entries point to existing files', () => {
    const memoryIndex = read(join(MEMORY, 'MEMORY.md'))
    // Match markdown links: [text](filename.md)
    const linkPattern = /\[.*?\]\(([\w\-]+\.md)\)/g
    const missing: string[] = []
    let match: RegExpExecArray | null

    while ((match = linkPattern.exec(memoryIndex)) !== null) {
      const file = match[1]
      if (!exists(join(MEMORY, file))) {
        missing.push(file)
      }
    }

    expect(
      missing,
      `MEMORY.md links to non-existent files: ${missing.join(', ')}`,
    ).toHaveLength(0)
  })
})

// ---------------------------------------------------------------------------
// Cross-references: shared docs referenced from agents
// ---------------------------------------------------------------------------
describe('cross-reference resolution', () => {
  const sharedDocs = ['DELEGATION.md', 'SEVERITY_RUBRIC.md', 'TOOL_RULES.md', 'CANONICAL_SOURCES.md']

  it('agents that reference shared docs link to files that exist', () => {
    const agentFiles = readdirSync(AGENTS).filter(f => f.endsWith('.md'))
    const broken: string[] = []

    for (const file of agentFiles) {
      const content = read(join(AGENTS, file))
      for (const doc of sharedDocs) {
        if (content.includes(doc) && !exists(join(BRAIN, doc))) {
          broken.push(`${file} → ${doc}`)
        }
      }
    }

    expect(broken, `Broken cross-references:\n  ${broken.join('\n  ')}`).toHaveLength(0)
  })

  it('agents that reference workflow skills point to existing commands', () => {
    const workflowSkills = [
      'survey-confirm-fix',
      'lint-workflow',
      'typecheck-workflow',
      'pr-review-format',
    ]
    const agentFiles = readdirSync(AGENTS).filter(f => f.endsWith('.md'))
    const broken: string[] = []

    for (const file of agentFiles) {
      const content = read(join(AGENTS, file))
      for (const skill of workflowSkills) {
        if (content.includes(`/${skill}`) && !exists(join(COMMANDS, `${skill}.md`))) {
          broken.push(`${file} → /${skill}`)
        }
      }
    }

    expect(broken, `Broken workflow skill refs:\n  ${broken.join('\n  ')}`).toHaveLength(0)
  })
})

// ---------------------------------------------------------------------------
// Fixtures used by behavioral tests
// ---------------------------------------------------------------------------
describe('behavioral test fixtures', () => {
  const FIXTURES = join(HOME, 'brain', 'ai', 'claude', 'tests', 'fixtures')
  const requiredFixtures = [
    'agents-summary.txt',
    'claude-md-rules.txt',
    'sample-memory.txt',
    'sample-stale-memory.txt',
    'long-context-padding.txt',
    'sample-agent-def.txt',
  ]

  for (const fixture of requiredFixtures) {
    it(`fixture ${fixture} exists`, () => {
      const path = join(FIXTURES, fixture)
      expect(exists(path), `Fixture not found: ${path}`).toBe(true)
    })
  }
})
