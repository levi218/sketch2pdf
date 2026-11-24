// Runs the converter off the main thread. Messages in: {font: ArrayBuffer} | {sketch: ArrayBuffer}.
// Messages out: {ready} | {pdf: ArrayBuffer, warnings: string[], ms} | {error: string}.
import init from './sketch2pdf.js';

const m = await init();
const text = (ptr, len) => new TextDecoder().decode(m.HEAPU8.slice(ptr, ptr + len));

function pass(buf, fn) {
  const bytes = new Uint8Array(buf);
  const p = m._malloc(bytes.length);
  m.HEAPU8.set(bytes, p);
  try { return fn(p, bytes.length); } finally { m._free(p); }
}

onmessage = ({ data }) => {
  try {
    if (data.font) return void pass(data.font, m._s2p_add_font);
    const t0 = performance.now();
    const failed = pass(data.sketch, m._s2p_convert);
    const ptr = m._s2p_out_ptr(), len = m._s2p_out_len();
    if (failed) return void postMessage({ error: text(ptr, len) });
    const pdf = m.HEAPU8.slice(ptr, ptr + len).buffer;
    const warnings = text(m._s2p_warn_ptr(), m._s2p_warn_len()).split('\n').filter(Boolean);
    postMessage({ pdf, warnings, ms: performance.now() - t0 }, [pdf]);
  } catch (e) {
    postMessage({ error: `The converter crashed (${e.message}). Reload the page to try again.` });
  }
};
postMessage({ ready: true });
