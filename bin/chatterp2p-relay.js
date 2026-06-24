#!/usr/bin/env node
import { spawnSync } from 'node:child_process'
import { existsSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const exe = process.platform === 'win32' ? 'chatterp2p-relay.exe' : 'chatterp2p-relay'
const binary = path.join(root, 'native', exe)

if (!existsSync(binary)) {
  console.error(JSON.stringify({
    success: false,
    error: `Missing native chatterp2p-relay binary at ${binary}. Re-run npm install, or run cargo build --release and copy target/release/${exe} to native/${exe}.`,
    code: 'MISSING_NATIVE_BINARY'
  }))
  process.exit(1)
}

const result = spawnSync(binary, process.argv.slice(2), { stdio: 'inherit' })
if (result.error != null) {
  console.error(JSON.stringify({
    success: false,
    error: result.error.message,
    code: 'SPAWN_FAILED'
  }))
  process.exit(1)
}
process.exit(result.status ?? 1)
