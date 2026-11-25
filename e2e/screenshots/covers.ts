// Album rows load their cover from the Cover Art Archive by release MBID,
// which has nothing for the fixture releases. For the pictures those requests
// are answered here with a drawn sleeve: one of a few designs in one of a few
// palettes, picked by the release's place in the fixture dataset counted from
// Night Garden, so neighbouring releases look different, every run draws the
// same covers, and Night Garden, which opens the pictures' results, always
// gets the first design and palette: the light ones.
// The lettering is the app's own Space Grotesk, embedded in each sleeve, so
// the covers do not depend on the machine's fonts.

import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import type { Page } from '@playwright/test';
import { nightGarden, releases, type FixtureRelease } from '../fixtures/dataset.js';
import { e2eRoot } from '../helpers/env.js';

const SIZE = 250;

const PALETTES = [
  ['#3d405b', '#e07a5f', '#f4f1de', '#81b29a', '#f2cc8f'],
  ['#10002b', '#3c096c', '#7b2cbf', '#c77dff', '#e0aaff'],
  ['#283618', '#606c38', '#fefae0', '#dda15e', '#bc6c25'],
  ['#001219', '#005f73', '#0a9396', '#94d2bd', '#ee9b00'],
  ['#1d3557', '#457b9d', '#a8dadc', '#f1faee', '#e63946'],
  ['#264653', '#2a9d8f', '#e9c46a', '#f4a261', '#e76f51'],
  ['#0b132b', '#1c2541', '#3a506b', '#5bc0be', '#e0fbfc'],
] as const;

type Palette = (typeof PALETTES)[number];

/** A repeatable stream of numbers in [0, 1) for one release. */
function random(seed: string): () => number {
  let state = 0;
  for (const char of seed) {
    state = (Math.imul(state, 31) + char.charCodeAt(0)) | 0;
  }
  return () => {
    state = (state + 0x6d2b79f5) | 0;
    let t = Math.imul(state ^ (state >>> 15), 1 | state);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function escape(text: string): string {
  return text.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

/** The app's Space Grotesk, every weight it has, as an SVG style. */
const FONTS = (() => {
  const file = join(dirname(e2eRoot), 'web', 'assets', 'fonts', 'SpaceGrotesk-Variable.woff2');
  const data = readFileSync(file).toString('base64');
  return `<style>@font-face{font-family:Sleeve;font-weight:300 700;src:url(data:font/woff2;base64,${data}) format('woff2')} text{font-family:Sleeve}</style>`;
})();

/** Relative luminance of a #rrggbb colour, enough to tell light from dark. */
function luminance(color: string): number {
  const [r, g, b] = [1, 3, 5].map((at) => parseInt(color.slice(at, at + 2), 16)) as [number, number, number];
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function lightest(palette: Palette): string {
  return [...palette].sort((a, b) => luminance(b) - luminance(a))[0] as string;
}

function darkest(palette: Palette): string {
  return [...palette].sort((a, b) => luminance(a) - luminance(b))[0] as string;
}

/** `text` broken into lines of at most `width` characters, word by word. */
function wrap(text: string, width: number): string[] {
  const lines: string[] = [];
  for (const word of text.split(/\s+/)) {
    const last = lines[lines.length - 1];
    if (last !== undefined && last.length + 1 + word.length <= width) {
      lines[lines.length - 1] = `${last} ${word}`;
    } else {
      lines.push(word);
    }
  }
  return lines;
}

/** `lines` as one text element, a line every `leading` px from `y`. */
function lettering(lines: string[], x: number, y: number, leading: number, attributes: string): string {
  const spans = lines.map((line, i) => `<tspan x="${x}" dy="${i === 0 ? 0 : leading}">${escape(line)}</tspan>`);
  return `<text x="${x}" y="${y}" ${attributes}>${spans.join('')}</text>`;
}

/** A low-contrast grain over the whole sleeve, as printed card has. */
const GRAIN = `
  <filter id="grain"><feTurbulence type="fractalNoise" baseFrequency="0.9" numOctaves="2" stitchTiles="stitch"/>
    <feColorMatrix values="0 0 0 0 0.5  0 0 0 0 0.5  0 0 0 0 0.5  0 0 0 0.09 0"/></filter>`;
const GRAIN_LAYER = `<rect width="${SIZE}" height="${SIZE}" filter="url(#grain)"/>`;

/** A grid of tiles, each a circle, a quarter disc, a half disc or a square, on a light card. */
function tiles(release: FixtureRelease, palette: Palette, rand: () => number): string {
  const paper = lightest(palette);
  const ink = darkest(palette);
  const colors = palette.filter((color) => color !== paper);
  const title = wrap(release.title, 15);
  const band = 24 + title.length * 28;
  const rows = 3;
  const cell = SIZE / 3;
  const height = (SIZE - band) / rows;
  const parts: string[] = [];
  for (let row = 0; row < rows; row++) {
    for (let col = 0; col < 3; col++) {
      const x = col * cell;
      const y = row * height;
      const fill = colors[Math.floor(rand() * colors.length)];
      const back = rand() < 0.35 ? colors[Math.floor(rand() * colors.length)] : paper;
      parts.push(`<rect x="${x}" y="${y}" width="${cell}" height="${height}" fill="${back}"/>`);
      const r = Math.min(cell, height);
      const cx = x + cell / 2;
      const cy = y + height / 2;
      const kind = Math.floor(rand() * 4);
      if (kind === 0) {
        parts.push(`<circle cx="${cx}" cy="${cy}" r="${r * 0.36}" fill="${fill}"/>`);
      } else if (kind === 1) {
        parts.push(`<path d="M${cx - r / 2} ${cy + r / 2} A${r} ${r} 0 0 1 ${cx + r / 2} ${cy - r / 2} L${cx + r / 2} ${cy + r / 2} Z" fill="${fill}"/>`);
      } else if (kind === 2) {
        parts.push(`<path d="M${cx - r / 2} ${cy + r / 4} A${r / 2} ${r / 2} 0 0 1 ${cx + r / 2} ${cy + r / 4} Z" fill="${fill}"/>`);
      } else {
        parts.push(`<rect x="${cx - r * 0.3}" y="${cy - r * 0.3}" width="${r * 0.6}" height="${r * 0.6}" fill="${fill}"/>`);
      }
    }
  }
  return `
    <defs>${GRAIN}</defs>${parts.join('')}
    <rect x="0" y="${SIZE - band}" width="${SIZE}" height="${band}" fill="${paper}"/>
    ${lettering(title, 14, SIZE - band + 36, 28, `fill="${ink}" font-size="26" font-weight="700" letter-spacing="-0.5"`)}
    ${GRAIN_LAYER}`;
}

/** Hills or waves in layers, each a shade further into the palette, under a light title. */
function waves(release: FixtureRelease, palette: Palette, rand: () => number): string {
  const sky = lightest(palette);
  const layers = palette
    .filter((color) => color !== sky)
    .map((color, i) => {
      const base = 120 + i * 32;
      const amplitude = 6 + rand() * 12;
      const phase = rand() * Math.PI * 2;
      const points = Array.from({ length: 26 }, (_, step) => {
        const x = step * 10;
        return `${x},${(base + Math.sin(x / 34 + phase) * amplitude).toFixed(1)}`;
      });
      return `<polygon points="0,${SIZE} ${points.join(' ')} ${SIZE},${SIZE}" fill="${color}"/>`;
    });
  const title = wrap(release.title, 14);
  return `
    <defs>${GRAIN}</defs>
    <rect width="${SIZE}" height="${SIZE}" fill="${sky}"/>${layers.join('')}
    ${lettering(title, SIZE / 2, 46, 30, `fill="${darkest(palette)}" text-anchor="middle" font-size="28" font-weight="300"`)}
    ${GRAIN_LAYER}`;
}

/** Soft blurred lights on a dark ground, like an out-of-focus photograph, with a thin title. */
function blur(release: FixtureRelease, palette: Palette, rand: () => number): string {
  const ground = darkest(palette);
  const glows = palette.filter((color) => color !== ground);
  const lights = Array.from({ length: 6 }, (_, i) => {
    const color = glows[i % glows.length];
    return `<circle cx="${(rand() * SIZE).toFixed(0)}" cy="${(rand() * SIZE).toFixed(0)}" r="${(30 + rand() * 60).toFixed(0)}"
      fill="${color}" opacity="${(0.55 + rand() * 0.4).toFixed(2)}"/>`;
  });
  const title = wrap(release.title, 13);
  const top = SIZE / 2 - ((title.length - 1) * 32) / 2 + 10;
  return `
    <defs><filter id="soft" x="-50%" y="-50%" width="200%" height="200%"><feGaussianBlur stdDeviation="22"/></filter>${GRAIN}</defs>
    <rect width="${SIZE}" height="${SIZE}" fill="${ground}"/>
    <g filter="url(#soft)">${lights.join('')}</g>
    ${lettering(title, SIZE / 2, top, 32, 'fill="#ffffff" text-anchor="middle" font-size="30" font-weight="300"')}
    ${GRAIN_LAYER}`;
}

/** Rings spreading from a point off centre, alternating two inks, with a label across the bottom. */
function rings(release: FixtureRelease, palette: Palette, rand: () => number): string {
  const cx = 60 + rand() * 130;
  const cy = 50 + rand() * 90;
  const circles = Array.from({ length: 14 }, (_, i) => {
    const r = 330 - i * 22;
    const color = palette[i % 2 === 0 ? 1 : 3];
    return `<circle cx="${cx.toFixed(0)}" cy="${cy.toFixed(0)}" r="${r}" fill="${color}"/>`;
  });
  const title = wrap(release.title, 15);
  const band = 20 + title.length * 26;
  return `
    <defs>${GRAIN}</defs>${circles.join('')}
    <circle cx="${cx.toFixed(0)}" cy="${cy.toFixed(0)}" r="14" fill="${palette[4]}"/>
    <rect x="12" y="${SIZE - 12 - band}" width="${SIZE - 24}" height="${band}" fill="${darkest(palette)}"/>
    ${lettering(title, 24, SIZE - 12 - band + 32, 26, 'fill="#ffffff" font-size="24" font-weight="700"')}
    ${GRAIN_LAYER}`;
}

/** A sun over the sea, cut by the horizontal bars of a seventies sleeve. */
function sunset(release: FixtureRelease, palette: Palette, rand: () => number): string {
  const [deep, mid, , light, hot] = palette;
  const cx = 70 + rand() * 110;
  const bars = Array.from({ length: 6 }, (_, i) => {
    const y = 150 + i * 13;
    return `<rect x="0" y="${y}" width="${SIZE}" height="${2 + i * 1.4}" fill="${deep}"/>`;
  }).join('');
  const title = wrap(release.title.toUpperCase(), 13);
  return `
    <defs><linearGradient id="sky" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="${deep}"/><stop offset="1" stop-color="${mid}"/></linearGradient>
      <linearGradient id="sun" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="${light}"/><stop offset="1" stop-color="${hot}"/></linearGradient>${GRAIN}</defs>
    <rect width="${SIZE}" height="${SIZE}" fill="url(#sky)"/>
    <circle cx="${cx}" cy="175" r="64" fill="url(#sun)"/>${bars}
    <rect x="0" y="222" width="${SIZE}" height="28" fill="${deep}"/>
    ${lettering(title, 16, 40, 28, `fill="${light}" font-size="24" font-weight="700" letter-spacing="1"`)}
    ${GRAIN_LAYER}`;
}

/** The title alone, set large and tight on a flat colour. */
function type(release: FixtureRelease, palette: Palette): string {
  const words = release.title.toUpperCase().split(/\s+/).slice(0, 4);
  const size = words.some((word) => word.length > 8) ? 30 : 40;
  const lines = words.map(
    (word, i) => `<text x="16" y="${62 + i * size * 0.95}" fill="${palette[0]}" font-size="${size}" font-weight="700"
      letter-spacing="-1">${escape(word)}</text>`,
  );
  return `
    <defs>${GRAIN}</defs>
    <rect width="${SIZE}" height="${SIZE}" fill="${palette[3]}"/>
    <rect x="0" y="0" width="${SIZE}" height="10" fill="${palette[4]}"/>${lines.join('')}
    <text x="16" y="234" fill="${palette[0]}" font-size="16" letter-spacing="1">${escape(release.artist.toUpperCase())}</text>
    ${GRAIN_LAYER}`;
}

const DESIGNS = [tiles, waves, blur, rings, sunset, type];

/** `items[place]`, counting around the list in both directions. */
function around<T>(items: readonly T[], place: number): T {
  return items[((place % items.length) + items.length) % items.length] as T;
}

/** The sleeve of the release at `index` in the dataset, as an SVG document. */
function sleeve(release: FixtureRelease, index: number): string {
  const place = index - releases.indexOf(nightGarden);
  const design = around(DESIGNS, place);
  const palette = around(PALETTES, place);
  const body = design(release, palette, random(release.releaseMbid));
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${SIZE}" height="${SIZE}" viewBox="0 0 ${SIZE} ${SIZE}">${FONTS}${body}</svg>`;
}

/**
 * Answers the page's Cover Art Archive requests with drawn sleeves. A release
 * the dataset does not hold gets the archive's own answer for a release
 * without art: 404.
 */
export async function drawCovers(page: Page): Promise<void> {
  await page.route('https://coverartarchive.org/release/*/front-250', (route) => {
    const mbid = new URL(route.request().url()).pathname.split('/')[2];
    const index = releases.findIndex((release) => release.releaseMbid === mbid);
    const release = releases[index];
    if (release === undefined) {
      return route.fulfill({ status: 404 });
    }
    return route.fulfill({ contentType: 'image/svg+xml', body: sleeve(release, index) });
  });
}
