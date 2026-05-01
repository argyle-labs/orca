import { readFileSync } from 'fs'
import { join, dirname } from 'path'
import { fileURLToPath } from 'url'

const __dirname = dirname(fileURLToPath(import.meta.url))
const FIXTURES_DIR = join(__dirname, '..', '..', 'fixtures')

export function loadFixture(filename: string): string {
  return readFileSync(join(FIXTURES_DIR, filename), 'utf-8')
}
