/** Real PDF worker observations: supported text scope, honest gaps, bounded work and authority. */
import fs from 'node:fs'
import path from 'node:path'
import { createHash } from 'node:crypto'
import { assert, test, report, withTempDir } from './lib/harness.mjs'
import { callWorker, request, envelopeOf, requireWorker } from './lib/boundary.mjs'
import { pdf, stream, show, textPdf } from './lib/pdf-fixture.mjs'
requireWorker('pdf-worker')
const fileAt = (root, value, name = 'document.pdf') => { const file = path.join(root, name); fs.writeFileSync(file, value); return file }
async function run(root, args, options = {}) {
  const payload = { version: 1, requestId: 'pdf-test', component: 'file', budget: { timeoutMs: 30000, maxBytes: options.maxBytes ?? 131072 },
    ...(options.cache ? { storage: { root: options.cache, authorityId: 'a'.repeat(64), allowedTools: ['read_pdf', 'result'] } } : {}),
    request: { ...request(options.tool ?? 'read_pdf', args, { root, readOnly: options.readOnly, writeRoots: [] }), ...options.authority } }
  const raw = await callWorker(JSON.stringify(payload)); assert.ok(Buffer.byteLength(raw.stdout) <= payload.budget.maxBytes)
  return envelopeOf(raw, assert)
}
function failure(body, code) { assert.equal(body.error?.code, code, JSON.stringify(body)); assert.equal(body.error.effectState, 'none'); assert.notEqual(body.complete, true); assert.equal(body.total, undefined); assert.equal(body.revision, undefined) }
const catalog = '<< /Type /Catalog /Pages 2 0 R >>'
const font = '<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>'
const page = content => '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents ' + content + ' >>'
function padded(size, label = 'ok') { const prefix = show(label); return prefix + '%' + 'x'.repeat(size - Buffer.byteLength(prefix) - 2) + '\n' }

await test('raw and Flate PDF pages preserve literal/hex/TJ runs; text infers no separators', async () => {
  await withTempDir('pdf-text', async root => {
    const content = 'BT /F1 12 Tf (A\\(B\\)\\\\C) Tj [<436166e9> -120 ( ) 5 <80>] TJ T* (next) \' 0 0 (last) " ET\n'
    for (const deflate of [false, true]) {
      const bytes = textPdf([content], { deflate }), body = await run(root, { path: fileAt(root, bytes) })
      assert.equal(body.error, undefined); assert.equal(body.status, 'completed'); assert.equal(body.complete, true)
      assert.equal(body.scope, 'page-text-layer-content-order'); assert.ok(body.excludedContent.includes('images and OCR'))
      // text concatenates the runs in content order; no space or line break is inferred between them.
      assert.deepEqual(body.items, [{ page: 1, text: 'A(B)\\CCafé €nextlast', runs: ['A(B)\\C', 'Café €', 'next', 'last'] }])
      assert.equal(body.total, 1); assert.equal(body.revision, createHash('sha256').update(bytes).digest('hex')); assert.equal(body.nextOffset, 2)
    }
    const aliases = await run(root, { path: fileAt(root, textPdf([show(Buffer.from([127,129,141,143,144,157,160,173,159]))])) })
    assert.equal(aliases.items[0].text, '•••••• -Ÿ')
    const standard = await run(root, { path: fileAt(root, textPdf([show("'quote" + String.fromCharCode(96))], { encoding: null })) })
    assert.equal(standard.items[0].text, '’quote‘')
  })
})
await test('a Word-style empty incremental revision reads the earlier classic xref', async () => {
  await withTempDir('pdf-incremental', async root => {
    const base = textPdf([show('incremental text')])
    // Word 2021 closes with an empty `0 0` subsection whose trailer only follows /Prev.
    const previous = base.lastIndexOf(Buffer.from('xref\n0 '))
    assert.notEqual(previous, -1)
    const revision = Buffer.from('xref\n0 0\ntrailer\n<< /Size 6 /Root 1 0 R /Prev ' + previous +
      ' >>\nstartxref\n' + base.length + '\n%%EOF\n')
    const body = await run(root, { path: fileAt(root, Buffer.concat([base, revision]), 'incremental.pdf') })
    assert.equal(body.error, undefined); assert.equal(body.status, 'completed'); assert.equal(body.complete, true)
    assert.deepEqual(body.items.map(item => item.text), ['incremental text'])
  })
})
await test('page offsets have complete-scan totals, ten-page default and honest blank/empty documents', async () => {
  await withTempDir('pdf-pages', async root => {
    const file = fileAt(root, textPdf(Array.from({ length: 12 }, (_, i) => show('Page ' + (i + 1)))))
    const first = await run(root, { path: file }); assert.equal(first.items.length, 10); assert.equal(first.nextOffset, 11); assert.equal(first.total, 12); assert.equal(first.scanComplete, true); assert.equal(first.pageComplete, true); assert.equal(first.complete, false)
    const last = await run(root, { path: file, offset: 11, limit: 2 }); assert.deepEqual(last.items.map(i => i.page), [11, 12]); assert.equal(last.complete, true); assert.equal(last.revision, first.revision)
    const blank = await run(root, { path: fileAt(root, textPdf([' \n\t'])) }); assert.deepEqual(blank.items, [{ page: 1, text: '', runs: [] }]); assert.equal(blank.complete, true)
    const empty = await run(root, { path: fileAt(root, textPdf([])) }); assert.equal(empty.total, 0); assert.deepEqual(empty.items, []); assert.equal(empty.complete, true)
    for (const args of [{ offset: 0 }, { offset: 33 }, { limit: 0 }, { limit: 33 }, { unknown: true }]) failure(await run(root, { path: file, ...args }), 'INVALID_ARGUMENT')
  })
})
await test('unsupported content is a per-window gap and never leaks into a clean window', async () => {
  await withTempDir('pdf-gaps', async root => {
    // `BX` opens a compatibility section the reader does not traverse: a window that stops before
    // it stays a complete observation, so the later gap cannot make it partial.
    const gaps = fileAt(root, textPdf([show('safe'), 'BX weird EX', show('unobserved')]))
    const clean = await run(root, { path: gaps, limit: 1 })
    assert.equal(clean.error, undefined); assert.equal(clean.status, 'completed'); assert.equal(clean.scanComplete, true)
    assert.equal(clean.pageComplete, true); assert.equal(clean.total, 3); assert.equal(clean.nextOffset, 2)
    assert.equal(clean.pagesObserved, 1); assert.deepEqual(clean.items.map(i => i.text), ['safe'])
    assert.equal(clean.failedPages, undefined); assert.equal(clean.nextAction, undefined)
    // Requesting the gap reports it as a value without failing pages outside the window.
    const gap = await run(root, { path: gaps, offset: 2, limit: 1 })
    assert.equal(gap.error, undefined); assert.equal(gap.status, 'partial'); assert.equal(gap.scanComplete, false)
    assert.equal(gap.pageComplete, true); assert.equal(gap.nextOffset, 3); assert.deepEqual(gap.items, [])
    assert.deepEqual(gap.failedPages.map(entry => [entry.page, entry.code]), [[2, 'UNSUPPORTED_DOCUMENT']])
    // A form XObject's content stream is traversed: an empty form adds no text, so the second
    // page completes and the scan reaches its real page count.
    const objects = [catalog, '<< /Type /Pages /Kids [4 0 R 6 0 R] /Count 2 >>', font, page('5 0 R'), stream(show('safe')),
      '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> /XObject << /Fm1 8 0 R >> >> /Contents 7 0 R >>',
      stream(show('not complete') + '/Fm1 Do\n'),
      stream('', { extra: '/Type /XObject /Subtype /Form /BBox [0 0 1 1]' })]
    const form = await run(root, { path: fileAt(root, pdf(objects), 'form.pdf'), limit: 2 })
    assert.equal(form.error, undefined); assert.equal(form.status, 'completed'); assert.equal(form.scanComplete, true)
    assert.equal(form.total, 2); assert.equal(form.nextOffset, 3); assert.equal(form.complete, true)
    // The empty form contributes no run, so page 2 keeps its own text and the pages stay in order.
    assert.deepEqual(form.items.map(i => i.text), ['safe', 'not complete'])
    assert.deepEqual(form.items.map(i => i.page), [1, 2])
    // A Do name absent from /XObject is malformed rather than merely unsupported; the readable
    // pages on both sides still return, and the failure is named inside the requested window.
    const missing = await run(root, { path: fileAt(root, textPdf([show('safe'), '/X1 Do', show('unobserved')])) })
    assert.equal(missing.error, undefined); assert.equal(missing.status, 'partial'); assert.equal(missing.scanComplete, false)
    assert.equal(missing.pageComplete, true)
    assert.deepEqual(missing.items.map(i => i.text), ['safe', 'unobserved'])
    assert.deepEqual(missing.failedPages.map(entry => [entry.page, entry.code]), [[2, 'DOCUMENT_PARSE_ERROR']])
    assert.match(missing.failedPages[0].message, /Missing PDF XObject/)
    assert.match(missing.nextAction, /file structure/)
  })
})
await test('paint and graphics-state operators between text segments are skipped', async () => {
  await withTempDir('pdf-paint', async root => {
    // Each operator paints no glyph, so skipping it cannot change the extracted runs. A page
    // whose content would otherwise hold no text still fails as "no supported text layer", so
    // every case keeps text on both sides of the skipped operator. Text concatenates the runs in
    // content order and infers no separator, so the two segments meet directly.
    const cases = ['/Tag BMC EMC', '/GS1 gs', '0 0 m 1 1 l S', '/Sh1 sh', 'BI /W 1 /H 1 ID x EI']
    for (const operator of cases) {
      const content = 'BT /F1 12 Tf (before) Tj ET ' + operator + ' BT /F1 12 Tf (after) Tj ET'
      const body = await run(root, { path: fileAt(root, textPdf([content])) })
      assert.equal(body.error, undefined, operator); assert.equal(body.status, 'completed'); assert.equal(body.complete, true); assert.equal(body.scanComplete, true)
      assert.deepEqual(body.items[0], { page: 1, text: 'beforeafter', runs: ['before', 'after'] }, operator)
    }
    // An image XObject paints pixels only, so its Do is skipped and both text segments survive.
    const objects = [catalog, '<< /Type /Pages /Kids [4 0 R] /Count 1 >>', font,
      '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> /XObject << /Im1 6 0 R >> >> /Contents 5 0 R >>',
      stream('BT /F1 12 Tf (before) Tj ET /Im1 Do BT /F1 12 Tf (after) Tj ET'),
      stream('x', { extra: '/Type /XObject /Subtype /Image /Width 1 /Height 1 /BitsPerComponent 8 /ColorSpace /DeviceGray' })]
    const painted = await run(root, { path: fileAt(root, pdf(objects), 'image.pdf') })
    assert.equal(painted.error, undefined); assert.equal(painted.status, 'completed'); assert.equal(painted.complete, true)
    assert.deepEqual(painted.items[0], { page: 1, text: 'beforeafter', runs: ['before', 'after'] })
  })
})
await test('unsupported fonts mappings controls and older encoding semantics fail explicitly', async () => {
  await withTempDir('pdf-fonts', async root => {
    // A named encoding outside WinAnsi, a built-in encoding outside the standard Latin
    // families, and PDF 1.2 semantics stay unsupported.
    for (const options of [{ encoding: 'MacRomanEncoding' }, { font: 'Symbol', encoding: null }, { font: 'Arial', encoding: null }, { version: '1.2' }]) {
      failure(await run(root, { path: fileAt(root, textPdf([show('text')], options)) }), 'UNSUPPORTED_DOCUMENT')
    }
    // A /ToUnicode entry that names no stream is malformed, not merely unsupported.
    failure(await run(root, { path: fileAt(root, textPdf([show('text')], { fontExtra: '/ToUnicode 3 0 R' })) }), 'DOCUMENT_PARSE_ERROR')
    failure(await run(root, { path: fileAt(root, textPdf([show(Buffer.from([0xe9]))], { encoding: null })) }), 'UNSUPPORTED_DOCUMENT')
    failure(await run(root, { path: fileAt(root, textPdf([show(Buffer.from([0,10]))])) }), 'UNSUPPORTED_DOCUMENT')
  })
})
await test('a FontDescriptor does not gate WinAnsiEncoding while another named encoding stays unsupported', async () => {
  await withTempDir('pdf-font-descriptor', async root => {
    // The embedded simple font carries a /FontDescriptor but its program is never read:
    // /WinAnsiEncoding carries the mapping, so byte 0x80 still decodes to the euro sign.
    const built = encoding => pdf([catalog, '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
      '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 6 0 R >> >> /Contents 4 0 R >>',
      stream(show(Buffer.from([0x41, 0x80]))), '<< /Type /FontDescriptor /FontName /Arial >>',
      '<< /Type /Font /Subtype /TrueType /BaseFont /Arial /Encoding /' + encoding + ' /FontDescriptor 5 0 R >>'])
    const winansi = await run(root, { path: fileAt(root, built('WinAnsiEncoding'), 'winansi.pdf') })
    assert.equal(winansi.error, undefined); assert.equal(winansi.status, 'completed'); assert.equal(winansi.items[0].text, 'A€')
    failure(await run(root, { path: fileAt(root, built('MacRomanEncoding'), 'macroman.pdf') }), 'UNSUPPORTED_DOCUMENT')
  })
})
await test('malformed content cannot be accepted as complete text', async () => {
  await withTempDir('pdf-grammar', async root => {
    for (const content of ['BT BT ET ET', '(x) Tj', 'ET', 'BT /F1 Tf ET', 'BT /F1 12 Tf [(x) /bad] TJ ET', 'BT /F1 12 Tf (x) Tj', 'q', 'Q', '1 2', 'BT /F1 12 Tf 8 Tr (x) Tj ET', 'BT /F1 12 Tf <xy> Tj ET']) {
      failure(await run(root, { path: fileAt(root, textPdf([content])) }), 'DOCUMENT_PARSE_ERROR')
    }
  })
})
await test('page counts aliases cycles parents and the physical page bound truncates instead of failing', async () => {
  await withTempDir('pdf-tree', async root => {
    for (const tree of ['<< /Type /Pages /Kids [4 0 R] /Count 2 >>', '<< /Type /Pages /Kids [4 0 R 4 0 R] /Count 2 >>', '<< /Type /Pages /Kids [2 0 R] /Count 1 >>']) {
      failure(await run(root, { path: fileAt(root, pdf([catalog, tree, font, page('5 0 R'), stream(show('safe'))])) }), 'DOCUMENT_PARSE_ERROR')
    }
    failure(await run(root, { path: fileAt(root, pdf([catalog, '<< /Type /Pages /Kids [4 0 R] /Count 1 >>', font, page('5 0 R').replace('/Parent 2', '/Parent 1'), stream(show('safe'))])) }), 'DOCUMENT_PARSE_ERROR')
    // The physical page bound truncates instead of failing: the leading pages come back, and the
    // scan claims neither a total nor completeness.
    const bounded = await run(root, { path: fileAt(root, textPdf(Array.from({ length: 33 }, () => show('x')))), limit: 1 })
    assert.equal(bounded.error, undefined); assert.equal(bounded.status, 'completed')
    assert.equal(bounded.scanComplete, false); assert.equal(bounded.pageComplete, true); assert.equal(bounded.complete, false)
    assert.equal(bounded.truncated, true); assert.equal(bounded.truncatedBy, 'page-limit'); assert.equal(bounded.pagesObserved, 1)
    assert.equal(bounded.items.length, 1); assert.deepEqual(bounded.items.map(item => item.text), ['x']); assert.equal(bounded.total, undefined)
    assert.match(bounded.nextAction, /32 physical pages.*dedicated PDF reader/)
  })
})
await test('raw file and actual decoded stream limits are independent of advertised compressed sizes', async () => {
  await withTempDir('pdf-bounds', async root => {
    failure(await run(root, { path: fileAt(root, Buffer.alloc(16 * 1024 * 1024 + 1)) }), 'OUTPUT_LIMIT')
    for (const deflate of [false, true]) failure(await run(root, { path: fileAt(root, textPdf([padded(1024 * 1024 + 1)], { deflate })) }), 'OUTPUT_LIMIT')
    // Predictors 2 and 10-15 are decoded; an unknown predictor value still fails explicitly.
    failure(await run(root, { path: fileAt(root, textPdf([show('x')], { deflate: true, extra: '/DecodeParms << /Predictor 99 >>' })) }), 'UNSUPPORTED_DOCUMENT')
  })
})
await test('unique decoded streams and repeated shared work have separate aggregate caps', async () => {
  await withTempDir('pdf-aggregate', async root => {
    const content = padded(1024 * 1024)
    // The window covers all 17 pages: the seventeenth crosses each aggregate budget, and that
    // page is a reported failure rather than a call-level error.
    const unique = await run(root, { path: fileAt(root, textPdf(Array(17).fill(content), { deflate: true })), limit: 17 })
    assert.equal(unique.error, undefined); assert.equal(unique.status, 'partial'); assert.equal(unique.pageComplete, true); assert.equal(unique.scanComplete, false)
    assert.equal(unique.pagesObserved, 17); assert.equal(unique.decodedBytes, 16 * 1024 * 1024); assert.equal(unique.items[0].text, 'ok')
    assert.deepEqual(unique.failedPages.map(entry => [entry.page, entry.code]), [[17, 'OUTPUT_LIMIT']])
    const objects = [catalog, '<< /Type /Pages /Kids [' + Array.from({ length: 17 }, (_, i) => (5 + i) + ' 0 R').join(' ') + '] /Count 17 >>', font, stream(content, { deflate: true }), ...Array(17).fill(page('4 0 R'))]
    const shared = await run(root, { path: fileAt(root, pdf(objects)), limit: 17 })
    assert.equal(shared.error, undefined); assert.equal(shared.status, 'partial'); assert.equal(shared.pagesObserved, 17)
    assert.equal(shared.decodedBytes, 1024 * 1024); assert.equal(shared.processedBytes, 16 * 1024 * 1024)
    assert.deepEqual(shared.failedPages.map(entry => [entry.page, entry.code]), [[17, 'OUTPUT_LIMIT']])
    const array = [catalog, '<< /Type /Pages /Kids [4 0 R] /Count 1 >>', font, page('[5 0 R 6 0 R]'), stream(padded(600000), { deflate: true }), stream(padded(600000), { deflate: true })]
    failure(await run(root, { path: fileAt(root, pdf(array)) }), 'OUTPUT_LIMIT')
  })
})
await test('an inline font dictionary is decoded and charged once per page', async () => {
  await withTempDir('pdf-inline-font', async root => {
    // Each page holds its font dictionary itself, so the document-wide cache has no object to key
    // it on; a decoder built at every Tf would charge the ToUnicode table again per showing.
    const content = 'BT /F1 12 Tf <0102> Tj ET\n'.repeat(3)
    const cmap = '/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CMapType 2 def\n' +
      '1 begincodespacerange\n<00> <FF>\nendcodespacerange\n2 beginbfchar\n<01> <0051>\n<02> <0033>\nendbfchar\n' +
      'endcmap CMapName currentdict /CMap defineresource pop end end'
    const pageWith = contents => '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 ' +
      '<< /Type /Font /Subtype /TrueType /BaseFont /BAAAAA+LiberationMono /ToUnicode 7 0 R >> >> >> /Contents ' + contents + ' 0 R >>'
    const objects = [catalog, '<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>',
      pageWith(4), stream(content), pageWith(6), stream(content), stream(cmap)]
    const body = await run(root, { path: fileAt(root, pdf(objects)) })
    assert.equal(body.error, undefined); assert.equal(body.status, 'completed'); assert.equal(body.complete, true)
    // Three shows become three runs; text concatenates them without inferring separators.
    assert.deepEqual(body.items.map(item => item.text), ['Q3Q3Q3', 'Q3Q3Q3'])
    assert.equal(body.decodedBytes, 2 * (Buffer.byteLength(content) + Buffer.byteLength(cmap)))
  })
})
await test('content arrays concatenate in order and inherited resources resolve without a second decoding', async () => {
  await withTempDir('pdf-inherit', async root => {
    const objects = [catalog, '<< /Type /Pages /Kids [4 0 R 5 0 R] /Count 2 /Resources << /Font << /F1 3 0 R >> >> >>', font,
      '<< /Type /Page /Parent 2 0 R /Contents [6 0 R 7 0 R] >>', '<< /Type /Page /Parent 2 0 R /Contents 6 0 R >>', stream(show('first'), { deflate: true }), stream(show('second'), { deflate: true })]
    const body = await run(root, { path: fileAt(root, pdf(objects)) })
    assert.equal(body.complete, true); assert.deepEqual(body.items.map(i => i.runs), [['first', 'second'], ['first']])
    assert.equal(body.decodedBytes, Buffer.byteLength(show('first') + show('second'))); assert.equal(body.processedBytes, body.decodedBytes + Buffer.byteLength(show('first')))
  })
})
await test('read-only PDF observations create nothing and repeat credential/no-follow checks', async () => {
  await withTempDir('pdf-authority', async root => {
    const file = fileAt(root, textPdf([show('safe')])), before = fs.readdirSync(root)
    assert.equal((await run(root, { path: file }, { readOnly: true })).complete, true); assert.deepEqual(fs.readdirSync(root), before)
    failure(await run(root, { path: file }, { authority: { deny_roots: [root] } }), 'CREDENTIAL_DENIED')
    const target = path.join(root, 'target'); fs.mkdirSync(target); const hidden = fileAt(target, textPdf([show('secret')]))
    const alias = path.join(root, 'alias'); fs.symlinkSync(target, alias, process.platform === 'win32' ? 'junction' : 'dir')
    const denied = await run(root, { path: path.join(alias, path.basename(hidden)) }); assert.ok(denied.error); assert.equal(denied.error.effectState, 'none'); assert.equal(denied.items, undefined)
  })
})
await test('reference spill reconstructs complete PDF pages and retains source coverage and root', async () => {
  await withTempDir('pdf-spill', async root => {
    const cache = path.join(root, 'cache'); fs.mkdirSync(cache, { mode: 0o700 })
    const value = Buffer.alloc(30000, 0xe9), expected = 'é'.repeat(30000)
    const file = fileAt(root, textPdf([show(value), show('last')], { deflate: true }))
    const head = await run(root, { path: file }, { cache, maxBytes: 4096 })
    assert.ok(head.reference, JSON.stringify(head)); assert.equal(head.complete, false); assert.equal(head.nextOffset, 1)
    // The inline list was cut, so this observation claims neither a complete scan nor a total;
    // the reference sink still captured every page before clipping.
    assert.equal(head.pageComplete, false); assert.equal(head.scanComplete, false); assert.equal(head.truncatedBy, 'output-budget')
    assert.equal(head.total, undefined)
    assert.equal(head.referenceRoot, process.platform === 'win32' ? path.toNamespacedPath(fs.realpathSync.native(root)) : fs.realpathSync(root))
    const records = []; let offset = 0, source
    for (;;) { const next = await run(head.referenceRoot, { action: 'page', id: head.reference.refId, offset, limit: 512, maxBytes: 65536 }, { cache, tool: 'result', readOnly: true }); assert.equal(next.error, undefined); records.push(...next.items); source = next.source; assert.ok(next.nextOffset > offset); offset = next.nextOffset; if (next.complete) break }
    assert.equal(source.scope, 'page-text-layer-content-order'); assert.equal(source.scanComplete, false); assert.equal(source.captureComplete, true); assert.equal(source.total, undefined)
    const rows = []; let chunks = [], byteOffset = 0
    for (const item of records) { assert.equal(item.index, rows.length); if (item.kind === 'item') { rows.push(item.value); continue } assert.equal(item.chunk, chunks.length); assert.equal(item.byteOffset, byteOffset); chunks.push(item.text); byteOffset += Buffer.byteLength(item.text); if (item.complete) { rows.push(JSON.parse(chunks.join(''))); chunks = []; byteOffset = 0 } }
    assert.deepEqual(rows.map(row => row.text), [expected, 'last']); assert.equal(chunks.length, 0); assert.deepEqual(rows[0].runs, [expected])
    const readonly = await run(root, { path: file }, { cache, readOnly: true, maxBytes: 4096 }); assert.equal(readonly.reference, undefined); assert.equal(readonly.storageError.code, 'POLICY_DENIED'); assert.equal(readonly.complete, false)
  })
})
await test('transport output limit still reports no business effects in writable sessions', async () => {
  await withTempDir('pdf-transport', async root => { failure(await run(root, { path: fileAt(root, textPdf([show('x'.repeat(30000))])) }, { maxBytes: 1024 }), 'OUTPUT_LIMIT') })
})
await test('offsets past the last page are argument errors and sparse blank pages remain empty', async () => {
  await withTempDir('pdf-sparse', async root => {
    const file=fileAt(root,textPdf([show('one'),show('two')]))
    // A window that starts after the last page must not look like a completed read of nothing.
    const beyond=await run(root,{path:file,offset:32,limit:1})
    failure(beyond,'INVALID_ARGUMENT')
    assert.match(beyond.error.message,/offset 32 exceeds the document's 2 pages/)
    assert.match(beyond.error.nextAction,/Use an offset from 1 to 2/)
    const one=fileAt(root,textPdf([show('only')]),'one.pdf')
    const past=await run(root,{path:one,offset:2})
    failure(past,'INVALID_ARGUMENT')
    assert.match(past.error.message,/offset 2 exceeds the document's 1 pages/)
    const blanks=[catalog,'<< /Type /Pages /Kids [4 0 R 5 0 R 6 0 R] /Count 3 >>',font,
      '<< /Type /Page /Parent 2 0 R >>',page('null'),page('7 0 R'),stream('% comment only\n')]
    const body=await run(root,{path:fileAt(root,pdf(blanks))})
    assert.equal(body.complete,true);assert.deepEqual(body.items.map(item=>item.runs),[[],[],[]])
    failure(await run(root,{path:fileAt(root,textPdf(['BT /F1 12 Tf (x) Tj 0 0 m ET']))}),'DOCUMENT_PARSE_ERROR')
  })
})
await test('an encrypted PDF names the password repair and no page window bypasses it', async () => {
  await withTempDir('pdf-encrypted', async root => {
    const encrypted = () => fileAt(root, textPdf([show('secret')], { trailer: '/Encrypt 9 0 R' }), 'encrypted.pdf')
    const body = await run(root, { path: encrypted() })
    failure(body, 'UNSUPPORTED_DOCUMENT')
    assert.match(body.error.message, /Encrypted PDFs are unsupported/)
    assert.match(body.error.nextAction, /Remove the open password/)
    assert.match(body.error.nextAction, /re-export/)
    assert.match(body.error.nextAction, /dedicated PDF reader/)
    assert.match(body.error.nextAction, /offsets and limits cannot decrypt/)
    assert.doesNotMatch(body.error.nextAction, /Correct the request or inspect the manifest/)
    // A different page window reaches the same encrypted file, not a readable page.
    failure(await run(root, { path: encrypted(), offset: 2 }), 'UNSUPPORTED_DOCUMENT')
  })
})
await test('stream-valued font and resource dictionaries cannot be interpreted as plain values', async () => {
  await withTempDir('pdf-stream-dicts',async root=>{
    const tree='<< /Type /Pages /Kids [4 0 R] /Count 1 >>'
    const fontStream=stream('',{extra:'/Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding'})
    failure(await run(root,{path:fileAt(root,pdf([catalog,tree,fontStream,page('5 0 R'),stream(show('text'))]))}),'DOCUMENT_PARSE_ERROR')
    const resourcePage='<< /Type /Page /Parent 2 0 R /Resources 6 0 R /Contents 5 0 R >>'
    failure(await run(root,{path:fileAt(root,pdf([catalog,tree,font,resourcePage,stream(show('text')),stream('',{extra:'/Font << /F1 3 0 R >>'})]))}),'DOCUMENT_PARSE_ERROR')
  })
})
report('pdf-worker')
