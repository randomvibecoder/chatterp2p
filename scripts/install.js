import { spawnSync } from 'node:child_process'
import { chmodSync, copyFileSync, existsSync, mkdirSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const exes = process.platform === 'win32' ? ['chatterp2p.exe', 'chatterp2p-relay.exe'] : ['chatterp2p', 'chatterp2p-relay']
const nativeDir = path.join(root, 'native')
const prebuiltDir = path.join(root, 'prebuilds', `${process.platform}-${process.arch}`)

mkdirSync(nativeDir, { recursive: true })

if (exes.every(exe => existsSync(path.join(prebuiltDir, exe)))) {
  for (const exe of exes) {
    const nativeBin = path.join(nativeDir, exe)
    copyFileSync(path.join(prebuiltDir, exe), nativeBin)
    chmodSync(nativeBin, 0o755)
  }
  process.exit(0)
}

const cargo = spawnSync('cargo', ['build', '--release'], {
  cwd: root,
  stdio: 'inherit'
})

if (cargo.error != null) {
  console.error(`Failed to run cargo: ${cargo.error.message}`)
  console.error('Install Rust from https://rustup.rs/ or use a release package with a prebuilt chatterp2p binary.')
  process.exit(1)
}

if (cargo.status !== 0) {
  process.exit(cargo.status ?? 1)
}

for (const exe of exes) {
  const built = path.join(root, 'target', 'release', exe)
  const nativeBin = path.join(nativeDir, exe)
  copyFileSync(built, nativeBin)
  chmodSync(nativeBin, 0o755)
}
