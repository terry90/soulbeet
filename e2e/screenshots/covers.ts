// Album rows load their cover from the Cover Art Archive by release MBID,
// which has nothing for the fixture releases. For the pictures those requests
// are answered here with a drawn sleeve: one of a few designs in one of a few
// palettes, picked by the release's place in the fixture dataset, so
// neighbouring releases look different and every run draws the same covers.

import type { Page } from '@playwright/test';
import { releases, type FixtureRelease } from '../fixtures/dataset.js';

const SIZE = 250;

const PALETTES = [
  ['#1d3557', '#457b9d', '#a8dadc', '#f1faee', '#e63946'],
  ['#264653', '#2a9d8f', '#e9c46a', '#f4a261', '#e76f51'],
  ['#0b132b', '#1c2541', '#3a506b', '#5bc0be', '#e0fbfc'],
  ['#3d405b', '#e07a5f', '#f4f1de', '#81b29a', '#f2cc8f'],
  ['#10002b', '#3c096c', '#7b2cbf', '#c77dff', '#e0aaff'],
  ['#283618', '#606c38', '#fefae0', '#dda15e', '#bc6c25'],
  ['#001219', '#005f73', '#0a9396', '#94d2bd', '#ee9b00'],
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

/** A low-contrast grain over the whole sleeve, as printed card has. */
const GRAIN = `
  <filter id="grain"><feTurbulence type="fractalNoise" baseFrequency="0.9" numOctaves="2" stitchTiles="stitch"/>
    <feColorMatrix values="0 0 0 0 0.5  0 0 0 0 0.5  0 0 0 0 0.5  0 0 0 0.09 0"/></filter>`;
const GRAIN_LAYER = `<rect width="${SIZE}" height="${SIZE}" filter="url(#grain)"/>`;

/** A sun over the sea, cut by the horizontal bars of a seventies sleeve. */
function sunset(release: FixtureRelease, [deep, mid, , light, hot]: Palette, rand: () => number): string {
  const cx = 70 + rand() * 110;
  const bars = Array.from({ length: 6 }, (_, i) => {
    const y = 150 + i * 13;
    return `<rect x="0" y="${y}" width="${SIZE}" height="${2 + i * 1.4}" fill="${deep}"/>`;
  }).join('');
  return `
    <defs><linearGradient id="sky" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="${deep}"/><stop offset="1" stop-color="${mid}"/></linearGradient>
      <linearGradient id="sun" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="${light}"/><stop offset="1" stop-color="${hot}"/></linearGradient>${GRAIN}</defs>
    <rect width="${SIZE}" height="${SIZE}" fill="url(#sky)"/>
    <circle cx="${cx}" cy="160" r="72" fill="url(#sun)"/>${bars}
    <rect x="0" y="212" width="${SIZE}" height="38" fill="${deep}"/>
    <text x="18" y="34" fill="${light}" font-family="Helvetica, Arial, sans-serif" font-size="15"
      font-weight="700" letter-spacing="3">${escape(release.title.toUpperCase())}</text>
    <text x="18" y="236" fill="${light}" opacity="0.8" font-family="Helvetica, Arial, sans-serif"
      font-size="11" letter-spacing="2">${escape(release.artist.toUpperCase())}</text>${GRAIN_LAYER}`;
}

/** A grid of tiles, each a circle, a quarter disc or a half disc, in flat print colours. */
function tiles(release: FixtureRelease, palette: Palette, rand: () => number): string {
  const [ink, , , paper] = palette;
  const colors = palette.filter((color) => color !== paper);
  const cell = SIZE / 3;
  const parts: string[] = [];
  for (let row = 0; row < 3; row++) {
    for (let col = 0; col < 3; col++) {
      const x = col * cell;
      const y = row * cell;
      const fill = colors[Math.floor(rand() * colors.length)];
      const back = rand() < 0.35 ? colors[Math.floor(rand() * colors.length)] : paper;
      parts.push(`<rect x="${x}" y="${y}" width="${cell}" height="${cell}" fill="${back}"/>`);
      const kind = Math.floor(rand() * 4);
      if (kind === 0) {
        parts.push(`<circle cx="${x + cell / 2}" cy="${y + cell / 2}" r="${cell * 0.36}" fill="${fill}"/>`);
      } else if (kind === 1) {
        parts.push(`<path d="M${x} ${y + cell} A${cell} ${cell} 0 0 1 ${x + cell} ${y} L${x + cell} ${y + cell} Z" fill="${fill}"/>`);
      } else if (kind === 2) {
        parts.push(`<path d="M${x} ${y + cell / 2} A${cell / 2} ${cell / 2} 0 0 1 ${x + cell} ${y + cell / 2} Z" fill="${fill}"/>`);
      } else {
        parts.push(`<rect x="${x + cell * 0.2}" y="${y + cell * 0.2}" width="${cell * 0.6}" height="${cell * 0.6}" fill="${fill}"/>`);
      }
    }
  }
  return `
    <defs>${GRAIN}</defs>${parts.join('')}
    <rect x="0" y="206" width="${SIZE}" height="44" fill="${paper}"/>
    <text x="14" y="226" fill="${ink}" font-family="Helvetica, Arial, sans-serif" font-size="13"
      font-weight="700">${escape(release.title)}</text>
    <text x="14" y="242" fill="${ink}" opacity="0.75" font-family="Helvetica, Arial, sans-serif"
      font-size="10">${escape(release.artist)}</text>${GRAIN_LAYER}`;
}

/** Hills or waves in layers, each a shade further into the palette. */
function waves(release: FixtureRelease, palette: Palette, rand: () => number): string {
  const layers = palette.map((color, i) => {
    const base = 86 + i * 34;
    const amplitude = 6 + rand() * 14;
    const phase = rand() * Math.PI * 2;
    const points = Array.from({ length: 26 }, (_, step) => {
      const x = step * 10;
      return `${x},${(base + Math.sin(x / 34 + phase) * amplitude).toFixed(1)}`;
    });
    return `<polygon points="0,${SIZE} ${points.join(' ')} ${SIZE},${SIZE}" fill="${color}"/>`;
  });
  return `
    <defs>${GRAIN}</defs>
    <rect width="${SIZE}" height="${SIZE}" fill="${palette[2]}"/>${layers.join('')}
    <text x="${SIZE / 2}" y="44" fill="${palette[0]}" text-anchor="middle" font-family="Georgia, serif"
      font-size="19" font-style="italic">${escape(release.title)}</text>
    <text x="${SIZE / 2}" y="60" fill="${palette[0]}" opacity="0.8" text-anchor="middle"
      font-family="Helvetica, Arial, sans-serif" font-size="9" letter-spacing="3">${escape(release.artist.toUpperCase())}</text>
    ${GRAIN_LAYER}`;
}

/** Soft blurred lights on a dark ground, like an out-of-focus photograph, with a thin title. */
function blur(release: FixtureRelease, palette: Palette, rand: () => number): string {
  const lights = Array.from({ length: 6 }, (_, i) => {
    const color = palette[1 + (i % 4)];
    return `<circle cx="${(rand() * SIZE).toFixed(0)}" cy="${(rand() * SIZE).toFixed(0)}" r="${(30 + rand() * 60).toFixed(0)}"
      fill="${color}" opacity="${(0.55 + rand() * 0.4).toFixed(2)}"/>`;
  });
  return `
    <defs><filter id="soft" x="-50%" y="-50%" width="200%" height="200%"><feGaussianBlur stdDeviation="22"/></filter>${GRAIN}</defs>
    <rect width="${SIZE}" height="${SIZE}" fill="${palette[0]}"/>
    <g filter="url(#soft)">${lights.join('')}</g>
    <text x="${SIZE / 2}" y="132" fill="#ffffff" text-anchor="middle" font-family="Georgia, serif"
      font-size="22" letter-spacing="1">${escape(release.title)}</text>
    <line x1="95" y1="146" x2="155" y2="146" stroke="#ffffff" stroke-width="0.8" opacity="0.7"/>
    <text x="${SIZE / 2}" y="164" fill="#ffffff" opacity="0.8" text-anchor="middle"
      font-family="Helvetica, Arial, sans-serif" font-size="10" letter-spacing="3">${escape(release.artist.toUpperCase())}</text>
    ${GRAIN_LAYER}`;
}

/** Rings spreading from a point off centre, alternating two inks. */
function rings(release: FixtureRelease, palette: Palette, rand: () => number): string {
  const cx = 60 + rand() * 130;
  const cy = 60 + rand() * 130;
  const circles = Array.from({ length: 14 }, (_, i) => {
    const r = 330 - i * 22;
    const color = palette[i % 2 === 0 ? 1 : 3];
    return `<circle cx="${cx.toFixed(0)}" cy="${cy.toFixed(0)}" r="${r}" fill="${color}"/>`;
  });
  return `
    <defs>${GRAIN}</defs>${circles.join('')}
    <circle cx="${cx.toFixed(0)}" cy="${cy.toFixed(0)}" r="14" fill="${palette[4]}"/>
    <rect x="14" y="196" width="${SIZE - 28}" height="40" fill="${palette[0]}"/>
    <text x="24" y="213" fill="#ffffff" font-family="Helvetica, Arial, sans-serif" font-size="12"
      font-weight="700">${escape(release.title)}</text>
    <text x="24" y="228" fill="#ffffff" opacity="0.75" font-family="Helvetica, Arial, sans-serif"
      font-size="9" letter-spacing="1">${escape(release.artist)}</text>
    ${GRAIN_LAYER}`;
}

/** The title alone, set large and tight on a flat colour. */
function type(release: FixtureRelease, palette: Palette): string {
  const words = release.title.toUpperCase().split(/\s+/).slice(0, 4);
  const size = words.some((word) => word.length > 8) ? 30 : 40;
  const lines = words.map(
    (word, i) => `<text x="16" y="${62 + i * size * 0.95}" fill="${palette[0]}" font-family="Helvetica, Arial, sans-serif"
      font-size="${size}" font-weight="900" letter-spacing="-1">${escape(word)}</text>`,
  );
  return `
    <defs>${GRAIN}</defs>
    <rect width="${SIZE}" height="${SIZE}" fill="${palette[3]}"/>
    <rect x="0" y="0" width="${SIZE}" height="10" fill="${palette[4]}"/>${lines.join('')}
    <text x="16" y="236" fill="${palette[0]}" font-family="Helvetica, Arial, sans-serif" font-size="11"
      letter-spacing="2">${escape(release.artist.toUpperCase())}</text>${GRAIN_LAYER}`;
}

const DESIGNS = [sunset, tiles, waves, blur, rings, type];

/** The sleeve of the release at `index` in the dataset, as an SVG document. */
function sleeve(release: FixtureRelease, index: number): string {
  const design = DESIGNS[index % DESIGNS.length] as (typeof DESIGNS)[number];
  const palette = PALETTES[(index * 3) % PALETTES.length] as Palette;
  const body = design(release, palette, random(release.releaseMbid));
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${SIZE}" height="${SIZE}" viewBox="0 0 ${SIZE} ${SIZE}">${body}</svg>`;
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
