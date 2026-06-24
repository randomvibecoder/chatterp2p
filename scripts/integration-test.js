import assert from 'node:assert/strict'
import { spawn, spawnSync } from 'node:child_process'
import { closeSync, existsSync, mkdtempSync, openSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

const root = path.resolve(path.dirname(new URL(import.meta.url).pathname), '..')
const bin = path.join(root, 'target', 'debug', process.platform === 'win32' ? 'chatterp2p.exe' : 'chatterp2p')
const relayBin = path.join(root, 'target', 'debug', process.platform === 'win32' ? 'chatterp2p-relay.exe' : 'chatterp2p-relay')

function runRaw (agent, args, options = {}) {
  return spawnSync(bin, args, {
    cwd: root,
    env: {
      ...process.env,
      CHATTERP2P_CONFIG_DIR: agent.config,
      CHATTERP2P_DATA_DIR: agent.data
    },
    encoding: 'utf8',
    ...options
  })
}

function runJson (agent, args) {
  const result = runRaw(agent, args)
  const output = result.stdout || result.stderr
  assert.equal(result.status, 0, `${args.join(' ')} failed\nstdout:\n${result.stdout}\nstderr:\n${result.stderr}`)
  return JSON.parse(output)
}

function runFail (agent, args) {
  const result = runRaw(agent, args)
  assert.notEqual(result.status, 0, `${args.join(' ')} unexpectedly succeeded`)
  return JSON.parse(result.stderr)
}

function agent (name) {
  const dir = mkdtempSync(path.join(tmpdir(), `chatterp2p-rust-${name}-`))
  return {
    dir,
    config: path.join(dir, 'config'),
    data: path.join(dir, 'data')
  }
}

function waitForCard (agent) {
  for (let i = 0; i < 80; i++) {
    const result = runRaw(agent, ['contact', 'card'])
    if (result.status === 0) {
      const card = JSON.parse(result.stdout)
      if (card.multiaddrs?.length > 0) return card
    }
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100)
  }
  throw new Error(`daemon did not advertise addresses\n${readFileSync(path.join(agent.data, 'daemon.log'), 'utf8')}`)
}

function waitForRelayedCard (agent) {
  for (let i = 0; i < 120; i++) {
    const card = waitForCard(agent)
    if (card.multiaddrs?.some(addr => addr.includes('/p2p-circuit/'))) return card
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 150)
  }
  throw new Error(`daemon did not advertise relayed address\n${readFileSync(path.join(agent.data, 'daemon.log'), 'utf8')}`)
}

function parseJsonObjects (raw) {
  const values = []
  let acc = ''
  for (const line of raw.split('\n')) {
    if (line.trim() === '' && acc === '') continue
    acc += `${line}\n`
    try {
      values.push(JSON.parse(acc))
      acc = ''
    } catch {}
  }
  return values
}

function waitForRelayAddress (logFile) {
  for (let i = 0; i < 120; i++) {
    if (existsSync(logFile)) {
      const parsed = parseJsonObjects(readFileSync(logFile, 'utf8'))
      const started = parsed.find(value => value.success === true && value.mode === 'relay' && value.addresses?.length > 0)
      if (started) return started.addresses[0]
    }
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100)
  }
  throw new Error(`relay did not advertise address\n${existsSync(logFile) ? readFileSync(logFile, 'utf8') : ''}`)
}

const a = agent('a')
const b = agent('b')
const relayRoot = mkdtempSync(path.join(tmpdir(), 'chatterp2p-relay-'))
const relayLog = path.join(relayRoot, 'relay.log')
let relay = null
let relayFd = null

try {
  const help = runRaw(a, ['--help'])
  assert.equal(help.status, 0)
  assert.match(help.stdout, /peer show/)
  assert.doesNotMatch(help.stdout, /peer ping/)

  const version = runRaw(a, ['--version'])
  assert.equal(version.status, 0)
  assert.match(version.stdout, /^0\.0\.1\n$/)

  const aInit = runJson(a, ['init'])
  const aInitAgain = runJson(a, ['init'])
  const bInit = runJson(b, ['init'])
  assert.match(aInit.peer_id, /^12D3KooW/)
  assert.equal(aInit.peer_id, aInitAgain.peer_id)

  const me = runJson(a, ['me'])
  assert.equal(me.peer_id, aInit.peer_id)
  assert.deepEqual(me.listen, ['/ip4/0.0.0.0/tcp/0/ws'])
  assert.equal(me.relays, undefined)

  const removed = runFail(a, ['relay', 'list'])
  assert.match(removed.error, /Usage: chatterp2p <init\|me\|contact\|peer\|message\|inbox\|read\|daemon>/)

  runJson(b, ['daemon', 'start', '--listen', '/ip4/127.0.0.1/tcp/0/ws'])
  const card = waitForCard(b)
  assert.equal(card.peer_id, bInit.peer_id)
  const addr = card.multiaddrs.find(addr => addr.includes('/ws'))
  assert.ok(addr)

  const added = runJson(a, ['peer', 'add', bInit.peer_id, 'bob', addr])
  assert.equal(added.peer.name, 'bob')

  const shown = runJson(a, ['peer', 'show', 'bob'])
  assert.equal(shown.peer.peer_id, bInit.peer_id)
  assert.deepEqual(shown.peer.addresses, [addr])

  const listed = runJson(a, ['peer', 'list'])
  assert.equal(listed.peers.length, 1)

  const tooLarge = runFail(a, ['message', 'bob', 'x'.repeat(1001)])
  assert.match(tooLarge.error, /exceeds 1000/)

  const sent = runJson(a, ['message', 'bob', 'hello-rust-test'])
  assert.equal(sent.message.body, 'hello-rust-test')

  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 300)
  const inbox = runJson(b, ['inbox'])
  assert.equal(inbox.messages.length, 1)
  assert.equal(inbox.messages[0].body, 'hello-rust-test')

  const read = runJson(b, ['read', inbox.messages[0].id])
  assert.equal(read.message.id, inbox.messages[0].id)

  const stopped = runJson(b, ['daemon', 'stop'])
  assert.equal(stopped.stopped, true)

  const status = runJson(b, ['daemon', 'status'])
  assert.equal(status.running, false)

  relayFd = openSync(relayLog, 'a')
  relay = spawn(relayBin, ['--listen', '/ip4/127.0.0.1/tcp/0/ws', '--identity', path.join(relayRoot, 'identity.json')], {
    cwd: root,
    stdio: ['ignore', relayFd, relayFd]
  })
  const relayAddr = waitForRelayAddress(relayLog)

  let c = null
  let d = null
  let e = null
  try {
    c = agent('c')
    d = agent('d')
    e = agent('e')
    const cInit = runJson(c, ['init'])
    const dInit = runJson(d, ['init'])
    const eInit = runJson(e, ['init'])
    runJson(c, ['daemon', 'start', '--listen', '/ip4/127.0.0.1/tcp/0/ws', '--relay', relayAddr])
    runJson(d, ['daemon', 'start', '--listen', '/ip4/127.0.0.1/tcp/0/ws', '--relay', relayAddr])
    runJson(e, ['daemon', 'start', '--listen', '/ip4/127.0.0.1/tcp/0/ws', '--relay', relayAddr])

    const cCard = waitForRelayedCard(c)
    const dCard = waitForRelayedCard(d)
    const eCard = waitForRelayedCard(e)
    const cRelayed = cCard.multiaddrs.find(addr => addr.includes('/p2p-circuit/'))
    const dRelayed = dCard.multiaddrs.find(addr => addr.includes('/p2p-circuit/'))
    const eRelayed = eCard.multiaddrs.find(addr => addr.includes('/p2p-circuit/'))
    assert.ok(cRelayed)
    assert.ok(dRelayed)
    assert.ok(eRelayed)

    runJson(c, ['peer', 'add', dInit.peer_id, 'dana', dRelayed])
    runJson(d, ['peer', 'add', cInit.peer_id, 'casey', cRelayed])
    runJson(c, ['peer', 'add', eInit.peer_id, 'elliot', eRelayed])
    runJson(d, ['peer', 'add', eInit.peer_id, 'elliot', eRelayed])
    runJson(e, ['peer', 'add', cInit.peer_id, 'casey', cRelayed])
    runJson(e, ['peer', 'add', dInit.peer_id, 'dana', dRelayed])

    const cToD = runJson(c, ['message', 'dana', 'hello-over-relay'])
    assert.match(cToD.dialed, /p2p-circuit/)

    const dToC = runJson(d, ['message', 'casey', 'reply-over-relay'])
    assert.match(dToC.dialed, /p2p-circuit/)

    const cToE = runJson(c, ['message', 'elliot', 'c-to-e-over-relay'])
    assert.match(cToE.dialed, /p2p-circuit/)

    const eToD = runJson(e, ['message', 'dana', 'e-to-d-over-relay'])
    assert.match(eToD.dialed, /p2p-circuit/)

    const dToE = runJson(d, ['message', 'elliot', 'd-to-e-over-relay'])
    assert.match(dToE.dialed, /p2p-circuit/)

    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 500)
    const dInbox = runJson(d, ['inbox'])
    const cInbox = runJson(c, ['inbox'])
    const eInbox = runJson(e, ['inbox'])
    assert.ok(dInbox.messages.some(message => message.body === 'hello-over-relay'))
    assert.ok(dInbox.messages.some(message => message.body === 'e-to-d-over-relay'))
    assert.ok(cInbox.messages.some(message => message.body === 'reply-over-relay'))
    assert.ok(eInbox.messages.some(message => message.body === 'c-to-e-over-relay'))
    assert.ok(eInbox.messages.some(message => message.body === 'd-to-e-over-relay'))
  } finally {
    if (c != null) {
      runRaw(c, ['daemon', 'stop'])
      rmSync(c.dir, { recursive: true, force: true })
    }
    if (d != null) {
      runRaw(d, ['daemon', 'stop'])
      rmSync(d.dir, { recursive: true, force: true })
    }
    if (e != null) {
      runRaw(e, ['daemon', 'stop'])
      rmSync(e.dir, { recursive: true, force: true })
    }
  }
} finally {
  runRaw(b, ['daemon', 'stop'])
  if (relay != null && relay.exitCode == null) relay.kill('SIGTERM')
  if (relayFd != null) closeSync(relayFd)
  rmSync(a.dir, { recursive: true, force: true })
  rmSync(b.dir, { recursive: true, force: true })
  rmSync(relayRoot, { recursive: true, force: true })
}
