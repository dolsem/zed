// Generates the PWA icons (a bold "Z" on a dark rounded square) as PNGs
// without any image dependencies. Used by the Vite config; can also be run
// directly (`node scripts/icons.mjs <out-dir>`) to inspect the output.
import { deflateSync } from "node:zlib";
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const CRC_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c >>> 0;
  }
  return table;
})();

function crc32(buffer) {
  let crc = 0xffffffff;
  for (const byte of buffer) crc = CRC_TABLE[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  return (crc ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([length, body, crc]);
}

function encodePng(size, rgba) {
  const header = Buffer.alloc(13);
  header.writeUInt32BE(size, 0);
  header.writeUInt32BE(size, 4);
  header[8] = 8; // bit depth
  header[9] = 6; // RGBA
  const rows = Buffer.alloc(size * (size * 4 + 1));
  for (let y = 0; y < size; y++) {
    rows[y * (size * 4 + 1)] = 0;
    rgba.copy(rows, y * (size * 4 + 1) + 1, y * size * 4, (y + 1) * size * 4);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", header),
    chunk("IDAT", deflateSync(rows, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

// The "Z" as a polygon in a unit box: two bars joined by a diagonal band.
function zPolygon(bar, diagonal) {
  return [
    [0, 0], [1, 0], [1, bar], [diagonal, 1 - bar], [1, 1 - bar], [1, 1],
    [0, 1], [0, 1 - bar], [1 - diagonal, bar], [0, bar],
  ];
}

function insidePolygon(points, x, y) {
  let inside = false;
  for (let i = 0, j = points.length - 1; i < points.length; j = i++) {
    const [xi, yi] = points[i];
    const [xj, yj] = points[j];
    if (yi > y !== yj > y && x < ((xj - xi) * (y - yi)) / (yj - yi) + xi) inside = !inside;
  }
  return inside;
}

function insideRoundedSquare(x, y, radius) {
  const cx = Math.min(Math.max(x, radius), 1 - radius);
  const cy = Math.min(Math.max(y, radius), 1 - radius);
  return (x - cx) ** 2 + (y - cy) ** 2 <= radius * radius;
}

const mix = (a, b, t) => a + (b - a) * t;

function renderIcon(size, { cornerRadius, glyphScale, background = true }) {
  const rgba = Buffer.alloc(size * size * 4);
  const glyph = zPolygon(0.2, 0.34);
  const glyphLeft = (1 - glyphScale) / 2;
  const top = [0x2b, 0x30, 0x3b];
  const bottom = [0x12, 0x14, 0x19];
  const ink = [0xee, 0xf1, 0xf6];
  const samples = 4;
  for (let py = 0; py < size; py++) {
    for (let px = 0; px < size; px++) {
      let coverBackground = 0;
      let coverGlyph = 0;
      for (let sy = 0; sy < samples; sy++) {
        for (let sx = 0; sx < samples; sx++) {
          const x = (px + (sx + 0.5) / samples) / size;
          const y = (py + (sy + 0.5) / samples) / size;
          if (!background || cornerRadius === 0 || insideRoundedSquare(x, y, cornerRadius)) coverBackground++;
          const gx = (x - glyphLeft) / glyphScale;
          const gy = (y - glyphLeft) / glyphScale;
          if (gx >= 0 && gx <= 1 && gy >= 0 && gy <= 1 && insidePolygon(glyph, gx, gy)) coverGlyph++;
        }
      }
      const total = samples * samples;
      const t = py / size;
      const glyphAlpha = coverGlyph / total;
      const offset = (py * size + px) * 4;
      if (background) {
        const base = top.map((value, index) => mix(value, bottom[index], t));
        const color = base.map((value, index) => mix(value, ink[index], glyphAlpha));
        rgba[offset] = Math.round(color[0]);
        rgba[offset + 1] = Math.round(color[1]);
        rgba[offset + 2] = Math.round(color[2]);
        rgba[offset + 3] = Math.round((coverBackground / total) * 255);
      } else {
        rgba[offset] = 255;
        rgba[offset + 1] = 255;
        rgba[offset + 2] = 255;
        rgba[offset + 3] = Math.round(glyphAlpha * 255);
      }
    }
  }
  return encodePng(size, rgba);
}

/** @returns {Map<string, Buffer>} output path (relative to the site root) → PNG */
export function generateIcons() {
  return new Map([
    ["icons/icon-192.png", renderIcon(192, { cornerRadius: 0.22, glyphScale: 0.5 })],
    ["icons/icon-512.png", renderIcon(512, { cornerRadius: 0.22, glyphScale: 0.5 })],
    ["icons/icon-maskable-512.png", renderIcon(512, { cornerRadius: 0, glyphScale: 0.4 })],
    ["icons/favicon-32.png", renderIcon(32, { cornerRadius: 0.22, glyphScale: 0.56 })],
    ["icons/badge-96.png", renderIcon(96, { cornerRadius: 0, glyphScale: 0.62, background: false })],
    // iOS rounds the corners itself and needs an opaque image.
    ["apple-touch-icon.png", renderIcon(180, { cornerRadius: 0, glyphScale: 0.46 })],
  ]);
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const outDir = process.argv[2] ?? "icons-out";
  for (const [path, png] of generateIcons()) {
    const target = join(outDir, path);
    mkdirSync(dirname(target), { recursive: true });
    writeFileSync(target, png);
    console.log(`wrote ${target} (${png.length} bytes)`);
  }
}
