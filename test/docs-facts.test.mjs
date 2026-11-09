/** Current user-facing docs mirror catalogue facts; keep the current surface from drifting again. */
import fs from 'node:fs'
import path from 'node:path'
import { getEncoding } from 'js-tiktoken'
import { CAPABILITIES, FILE_TOOLS, capability } from '../lib/capabilities/registry.js'
import { DOCUMENT_DESCRIPTIONS } from '../lib/capabilities/documents.js'
import { DISCOVERY_TERMS } from '../lib/capabilities/discovery.js'
import { publicToolName } from '../lib/policy.js'
import { assert, test, report, PACKAGE_ROOT } from './lib/harness.mjs'

const read = name => fs.readFileSync(path.join(PACKAGE_ROOT, name), 'utf8')
const docs = fs.readdirSync(path.join(PACKAGE_ROOT, 'docs'))
  .filter(name => name.endsWith('.md') && name !== 'CHANGELOG.md')
  .map(name => 'docs/' + name)
const CURRENT_DOCS = [...docs, 'README.md', 'README.en.md', 'README.zh.md']
// Derived from the catalogue, never hand-copied: a renamed or added tool must move these numbers.
const PTC_MEMBERS = CAPABILITIES.filter(entry => 'parameters' in entry && entry.presentations.includes('ptc')).length
const DISCOVERABLE = Object.keys(DISCOVERY_TERMS).sort()
const STALE = /ops_(?:read_many|edit_many|table_read|doc_read|pdf_read|sdk|discover)\b|\b(?:read_many|edit_many|table_read|doc_read|pdf_read)\b/u

await test('current docs never spell a pre-rename tool name, prefixed or bare', () => {
  for (const file of CURRENT_DOCS) {
    const match = read(file).match(STALE)
    assert.equal(match, null, `${file} still mentions ${String(match?.[0])}`)
  }
})

await test('the catalogue counts in the current docs match the registry', () => {
  assert.match(read('docs/component-reference.md'), new RegExp(`has ${CAPABILITIES.length} entries`))
  assert.match(read('docs/component-reference.md'), new RegExp(`${PTC_MEMBERS} APIs: ${FILE_TOOLS.length} file/document tools`))
  assert.match(read('docs/组件参考.md'), new RegExp(`有 ${CAPABILITIES.length} 项`))
  assert.match(read('docs/组件参考.md'), new RegExp(`${PTC_MEMBERS} 个 API：${FILE_TOOLS.length} 个文件/文档工具`))
  assert.match(read('docs/ptc-sdk-loading.md'), new RegExp(`${PTC_MEMBERS} APIs`))
  assert.match(read('docs/PTC按需加载.md'), new RegExp(`${PTC_MEMBERS} 个 API`))
  assert.match(read('README.md'), new RegExp(`${FILE_TOOLS.length} 个文件/文档工具`))
  assert.match(read('README.zh.md'), new RegExp(`${FILE_TOOLS.length} 个文件/文档工具`))
  assert.match(read('README.en.md'), new RegExp(`${FILE_TOOLS.length} file/document tools`))
})

await test('the current token and byte budget matches the measured gate', () => {
  assert.match(read('docs/discovery.md'), /261 tokens \/ 1196 bytes/u)
  assert.match(read('docs/工具发现.md'), /261 tokens \/ 1196 字节/u)
  assert.match(read('docs/benchmark-gates.md'), /261 tokens \/ 1,196 bytes/u)
  assert.doesNotMatch(read('docs/discovery.md'), /241 tokens/u)
})

await test('the discovery allowedTools example is exactly the discoverable catalogue', () => {
  for (const file of ['docs/discovery.md', 'docs/工具发现.md']) {
    const line = read(file).split('\n').find(entry => entry.includes('allowedTools:'))
    assert.ok(line, `${file} has no allowedTools example`)
    const body = line.slice(line.indexOf('allowedTools:') + 'allowedTools:'.length)
    const names = body.replace(/^[^[]*\[/u, '').replace(/\].*$/u, '').split(',').map(name => name.trim()).filter(Boolean)
    assert.deepEqual([...new Set(names)].sort(), DISCOVERABLE, `${file} diverges from DISCOVERY_TERMS`)
  }
})

// #408: always-loaded document readers are the documented alternative to a discovery
// round, so the docs' cost figure is derived from the catalogue here instead of being
// hand-copied; a reader schema or an always-loaded switch changing re-opens both docs.
await test('the documented always-loaded document trio cost matches the catalogue', () => {
  const wire = JSON.stringify(['read_pdf', 'read_docx', 'read_table'].map(raw => {
    const entry = capability(raw)
    return { name: publicToolName(raw), description: entry.description, parameters: entry.parameters }
  }))
  const measured = `${getEncoding('cl100k_base').encode(wire).length} tokens / ${Buffer.byteLength(wire)}`
  for (const file of ['docs/discovery.md', 'docs/工具发现.md']) {
    assert.ok(read(file).includes(measured), `${file} no longer states the measured ${measured} for the trio`)
  }
})

// The description routes the model: the old "Type1 ... only" sentence made real WPS,
// Chrome and LibreOffice files look unsupported. Pin the features it must advertise, the
// exclusive claim it must never regain, and the two behaviours that must not regress into
// refusals — incremental updates and the 32-page truncation; reading the catalogue keeps
// this off a copy.
await test('the read_pdf description advertises the implemented PDF layer, not the old exclusive claim', () => {
  const description = DOCUMENT_DESCRIPTIONS.read_pdf
  assert.doesNotMatch(description, /Restricted classic-xref/u)
  assert.doesNotMatch(description, /Standard\/WinAnsi encoding only/u)
  assert.doesNotMatch(description, /\bonly\b[^.]*\bType1\b|\bType1\b[^.]*\bonly\b/iu)
  assert.doesNotMatch(description, /Rejects[^.]*incremental/iu)
  assert.match(description, /embedded/iu)
  assert.match(description, /ToUnicode/u)
  assert.match(description, /object streams?|objstm/u)
  assert.match(description, /xref stream/iu)
  assert.match(description, /page-limit/u)
  // The argument names sit at the start so a first call reads them before guessing `pages`,
  // `maxPages` or `file_path`, which the tool rejects as INVALID_ARGUMENT.
  assert.match(description, /Arguments: path, optional offset \(1-32\), optional limit \(1-32\)\./u)
  // Per-page failures are window-scoped: the description must name the report and say that a page
  // outside the requested window cannot change the observation.
  assert.match(description, /failedPages/u)
  assert.match(description, /outside it|outside the (?:requested )?window/iu)
})

// The old descriptions called forms and pattern colours unsupported, which contradicted the
// reader and the native-kernel docs. Pin the implemented behaviour in both directions: the terms
// must be present, and the stale refusals must not come back in any current doc.
await test('the read_pdf description and docs describe form traversal and pattern skips, not refusals', () => {
  const description = DOCUMENT_DESCRIPTIONS.read_pdf
  assert.match(description, /form XObjects reached by Do are traversed/iu)
  assert.match(description, /pattern colours are validated then skipped/iu)
  assert.match(description, /status is the window verdict/iu)
  // The implemented wording is pinned per file: the kernel docs carry the content-order traversal
  // and the budget charge, and the one-line references carry the short traversal and exclusion.
  const stated = {
    'docs/native-kernel.md': ['traversed in content order', 'charged to the 16 MiB aggregate', 'tiling pattern'],
    'docs/原生内核.md': ['按内容顺序展开', '16 MiB', '平铺图案'],
    'docs/component-reference.md': ['traverses form XObjects reached by `Do`', 'tiling patterns'],
    'docs/组件参考.md': ['Form XObject 按内容顺序展开', '平铺图案'],
  }
  for (const [file, anchors] of Object.entries(stated)) {
    const text = read(file)
    for (const anchor of anchors) assert.ok(text.includes(anchor), `${file} lost "${anchor}"`)
  }
  for (const file of ['docs/native-kernel.md', 'docs/原生内核.md', 'docs/component-reference.md', 'docs/组件参考.md', 'docs/discovery.md', 'docs/工具发现.md']) {
    const text = read(file)
    assert.doesNotMatch(text, /\bforms?\b[^.]*\brejected\b/iu, `${file} calls forms rejected`)
    assert.doesNotMatch(text, /pattern colou?rs?[^.]*\bunsupported\b/iu, `${file} calls pattern colours unsupported`)
    assert.doesNotMatch(text, /Form XObjects?[^.]*\bunsupported\b/iu, `${file} calls form XObjects unsupported`)
    assert.doesNotMatch(text, /图案颜色[^。]*不支持/u, `${file} calls pattern colours unsupported`)
  }
})

// #300: a published checksum without its object is not a contract — the notes said
// `SHA-256` and meant the worker in one release, the archive in the next. The docs now
// have to name both objects, and the checker they recommend has to be a real script.
await test('docs name the object of each published checksum and keep the checker', () => {
  const releaseProcess = read('docs/release-process.md')
  // Both clauses are unique to the stated contract: `SHA256SUMS` already appears as a
  // build artifact elsewhere, so matching the word alone would stay green on a revert.
  assert.match(releaseProcess, /authoritative in `SHA256SUMS`/u)
  assert.match(releaseProcess, /never defines a checksum/u)
  assert.doesNotMatch(releaseProcess, /正文为准/u)
  for (const file of ['docs/troubleshooting.md', 'docs/常见问题.md']) {
    assert.match(read(file), /native-runtime\.mjs check --root package --platform/u, `${file} lost the in-package checker`)
  }
  assert.equal(fs.existsSync(path.join(PACKAGE_ROOT, 'bin/native-runtime.mjs')), true,
    'the checker the docs recommend no longer exists')
})

// #524: the two facts reported from a failing machine — an unmapped Windows `open`
// error, and the Git channel — are documented as a bilingual pair, because a reader
// who only finds the English section will re-file the same issue. The assertion is
// deliberately structural (the same command string in both files, each in its own
// language's words) rather than a wording check: it fails when one language loses the
// section, and stays green through any rewrite that keeps the diagnostic entry point.
await test('both languages document the unmapped pnpm-store error and the Git channel', () => {
  // Tokens no honest rewrite drops: the error code the report names, the libuv text it
  // quotes, the executable recovery step, and the reason the Git channel cannot work.
  const anchors = ['ERR_PNPM_UNKNOWN', 'unknown error, open', 'pnpm --store-dir', 'npm run build:ptc', 'prepare']
  for (const file of ['docs/upgrading.md', 'docs/升级与卸载.md']) {
    const text = read(file)
    for (const anchor of anchors) assert.ok(text.includes(anchor), `${file} lost "${anchor}"`)
  }
})

report('docs-facts')
