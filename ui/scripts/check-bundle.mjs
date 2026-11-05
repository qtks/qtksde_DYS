#!/usr/bin/env node
// Packaging contract check for the built UI.
//
// The desktop window loads `ui/dist/index.html` over the embedded HTTP server,
// and the installer ships that same `ui/dist` under `usr/lib/Panopticon/ui`.
// If index.html ever references a file that is missing from `dist`, the
// installed app renders a blank window. This script fails the build instead.

import { existsSync, readFileSync, statSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const dist = join(root, 'dist')
const indexPath = join(dist, 'index.html')

function fail(message) {
  console.error(`check:bundle FAILED: ${message}`)
  process.exit(1)
}

if (!existsSync(indexPath)) {
  fail(`${indexPath} is missing - run "npm run build" first`)
}

const html = readFileSync(indexPath, 'utf8')

// Anyone deleting the startup fallback would bring the white screen back with
// no explanation to the user.
if (!html.includes('boot-error')) {
  fail('index.html lost the inline startup-failure fallback ("boot-error")')
}

const references = [...html.matchAll(/\b(?:src|href)\s*=\s*["']([^"']+)["']/g)].map(
  (match) => match[1],
)

const localReferences = references.filter(
  (reference) => !/^(?:data:|https?:|\/\/)/i.test(reference),
)

if (localReferences.length === 0) {
  fail('index.html references no local scripts or styles - build output looks wrong')
}

let assets = 0
for (const reference of localReferences) {
  const path = reference.split(/[?#]/)[0].replace(/^\.?\//, '')
  if (path === '') continue
  const file = join(dist, path)
  if (!existsSync(file)) {
    fail(`index.html references "${reference}" but ${file} does not exist in ui/dist`)
  }
  if (statSync(file).size === 0) {
    fail(`${file} is empty`)
  }
  if (path.startsWith('assets/')) assets += 1
}

if (assets === 0) {
  fail('index.html references no assets/ files - Vite output or base path is broken')
}

console.log(
  `check:bundle OK - ${localReferences.length} local resource(s), ${assets} from assets/, ` +
    'startup fallback present',
)
