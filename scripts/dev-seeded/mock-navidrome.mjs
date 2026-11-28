#!/usr/bin/env node
// Standalone mock Navidrome server (OpenSubsonic /rest/* + Native /auth/login & /api/*)
// for the local seeded dev stack (`scripts/dev-seeded.sh`).
// Zero external npm dependencies; runs on Node 20+.

import { createServer } from 'node:http';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = dirname(dirname(__dirname));

const PORT = Number(process.env.NAVIDROME_PORT ?? 14533);
const HOST = process.env.NAVIDROME_HOST ?? '0.0.0.0';
const LIBRARY_PATH = process.env.NAVIDROME_MUSIC_PATH ?? '/music';

// --- Procedural Space Grotesk SVG album cover generator ---
const FONT_B64 = readFileSync(
  join(REPO_ROOT, 'web', 'assets', 'fonts', 'SpaceGrotesk-Variable.woff2'),
).toString('base64');
const FONT_CSS = `<style>@font-face{font-family:Sleeve;font-weight:300 700;src:url(data:font/woff2;base64,${FONT_B64}) format('woff2')} text{font-family:Sleeve}</style>`;
const COVER_SIZE = 300;

const PALETTES = [
  ['#1c1e33', '#00c476', '#f4f1de', '#6e56cf', '#e9c46a'],
  ['#140b29', '#3c096c', '#9d4edd', '#c77dff', '#f1e4ff'],
  ['#0f1c18', '#1b4332', '#52b788', '#d8f3dc', '#f2cc8f'],
  ['#001219', '#005f73', '#0a9396', '#94d2bd', '#ee9b00'],
  ['#1a1423', '#683b6a', '#c96f8a', '#f5e6e8', '#f2a65a'],
  ['#192a38', '#2a9d8f', '#e9c46a', '#f4a261', '#e76f51'],
  ['#0b132b', '#1c2541', '#3a506b', '#5bc0be', '#e0fbfc'],
  ['#22181c', '#5a189a', '#e07a5f', '#f4f1de', '#81b29a'],
];

function mulberry(seed) {
  let s = 0;
  for (const ch of seed) s = (Math.imul(s, 31) + ch.charCodeAt(0)) | 0;
  return () => {
    s = (s + 0x6d2b79f5) | 0;
    let t = Math.imul(s ^ (s >>> 15), 1 | s);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function escXml(str) {
  return String(str)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

function wrapText(text, maxChars) {
  const lines = [];
  for (const word of String(text).split(/\s+/)) {
    const last = lines[lines.length - 1];
    if (last && last.length + 1 + word.length <= maxChars) lines[lines.length - 1] = `${last} ${word}`;
    else lines.push(word);
  }
  return lines.slice(0, 3);
}

function svgText(lines, x, y, leading, attrs) {
  const tspans = lines
    .map((line, i) => `<tspan x="${x}" dy="${i === 0 ? 0 : leading}">${escXml(line)}</tspan>`)
    .join('');
  return `<text x="${x}" y="${y}" ${attrs}>${tspans}</text>`;
}

const GRAIN_DEF =
  '<filter id="g"><feTurbulence type="fractalNoise" baseFrequency="0.85" numOctaves="2" stitchTiles="stitch"/><feColorMatrix values="0 0 0 0 0.5 0 0 0 0 0.5 0 0 0 0 0.5 0 0 0 0.08 0"/></filter>';
const GRAIN_RECT = `<rect width="${COVER_SIZE}" height="${COVER_SIZE}" filter="url(#g)"/>`;

const coverCache = new Map();

function drawCoverSvg(id, title, artist, variant = 0) {
  const cacheKey = `${id}:${variant}`;
  if (coverCache.has(cacheKey)) return coverCache.get(cacheKey);
  const pal = PALETTES[Math.abs(variant) % PALETTES.length];
  const rand = mulberry(`${id}:${title}`);
  const design = Math.abs(variant) % 6;
  let body = '';

  if (design === 0) {
    const [bg, c1, c2, c3, c4] = pal;
    const orbs = [c1, c2, c3, c4, c1]
      .map((c) => {
        const cx = (30 + rand() * 240).toFixed(0);
        const cy = (30 + rand() * 220).toFixed(0);
        const r = (45 + rand() * 65).toFixed(0);
        return `<circle cx="${cx}" cy="${cy}" r="${r}" fill="${c}" opacity="${(0.45 + rand() * 0.4).toFixed(2)}"/>`;
      })
      .join('');
    const lines = wrapText(title, 13);
    body = `
      <defs><filter id="b" x="-40%" y="-40%" width="180%" height="180%"><feGaussianBlur stdDeviation="28"/></filter>${GRAIN_DEF}</defs>
      <rect width="${COVER_SIZE}" height="${COVER_SIZE}" fill="${bg}"/>
      <g filter="url(#b)">${orbs}</g>
      <rect x="0" y="198" width="${COVER_SIZE}" height="102" fill="rgba(9,10,16,0.58)"/>
      ${svgText(lines, 20, 232, 30, 'fill="#ffffff" font-size="26" font-weight="700" letter-spacing="-0.6"')}
      <text x="20" y="282" fill="rgba(255,255,255,0.72)" font-size="13" font-weight="500" letter-spacing="1.4">${escXml(artist.toUpperCase())}</text>
      ${GRAIN_RECT}`;
  } else if (design === 1) {
    const [ink, c1, paper, c2, c3] = pal;
    const cols = [c1, c2, c3];
    const parts = [];
    const cell = COVER_SIZE / 3;
    const h = 66;
    for (let r = 0; r < 3; r++) {
      for (let c = 0; c < 3; c++) {
        const x = c * cell;
        const y = r * h;
        const fill = cols[Math.floor(rand() * cols.length)];
        const back = rand() < 0.35 ? cols[Math.floor(rand() * cols.length)] : paper;
        parts.push(`<rect x="${x}" y="${y}" width="${cell}" height="${h}" fill="${back}"/>`);
        const rad = Math.min(cell, h);
        const cx = x + cell / 2;
        const cy = y + h / 2;
        const k = Math.floor(rand() * 3);
        if (k === 0) parts.push(`<circle cx="${cx}" cy="${cy}" r="${rad * 0.36}" fill="${fill}"/>`);
        else if (k === 1) parts.push(`<path d="M${x} ${y + h} A${rad} ${rad} 0 0 1 ${x + cell} ${y} L${x + cell} ${y + h} Z" fill="${fill}"/>`);
        else parts.push(`<rect x="${cx - rad * 0.28}" y="${cy - rad * 0.28}" width="${rad * 0.56}" height="${rad * 0.56}" fill="${fill}"/>`);
      }
    }
    const lines = wrapText(title, 15);
    body = `
      <defs>${GRAIN_DEF}</defs>${parts.join('')}
      <rect x="0" y="198" width="${COVER_SIZE}" height="102" fill="${paper}"/>
      ${svgText(lines, 18, 236, 28, `fill="${ink}" font-size="25" font-weight="700" letter-spacing="-0.6"`)}
      <text x="18" y="284" fill="${ink}" opacity="0.7" font-size="12" font-weight="600" letter-spacing="1.2">${escXml(artist.toUpperCase())}</text>
      ${GRAIN_RECT}`;
  } else if (design === 2) {
    const [deep, c1, c2, light, accent] = pal;
    const layers = [c1, c2, accent, deep]
      .map((col, idx) => {
        const base = 115 + idx * 40;
        const amp = 8 + rand() * 14;
        const ph = rand() * Math.PI * 2;
        const pts = Array.from({ length: 31 }, (_, s) => `${s * 10},${(base + Math.sin(s * 0.32 + ph) * amp).toFixed(1)}`).join(' ');
        return `<polygon points="0,${COVER_SIZE} ${pts} ${COVER_SIZE},${COVER_SIZE}" fill="${col}"/>`;
      })
      .join('');
    const lines = wrapText(title, 14);
    body = `
      <defs>${GRAIN_DEF}</defs>
      <rect width="${COVER_SIZE}" height="${COVER_SIZE}" fill="${light}"/>
      ${layers}
      ${svgText(lines, COVER_SIZE / 2, 54, 30, `fill="${deep}" text-anchor="middle" font-size="26" font-weight="700"`)}
      <text x="${COVER_SIZE / 2}" y="280" fill="${light}" text-anchor="middle" font-size="12" font-weight="600" letter-spacing="1.6">${escXml(artist.toUpperCase())}</text>
      ${GRAIN_RECT}`;
  } else if (design === 3) {
    const [bg, c1, c2, c3, hot] = pal;
    const cx = 80 + rand() * 140;
    const cy = 65 + rand() * 95;
    const circles = Array.from({ length: 14 }, (_, i) => {
      const r = 360 - i * 24;
      return `<circle cx="${cx.toFixed(0)}" cy="${cy.toFixed(0)}" r="${r}" fill="${i % 2 === 0 ? c1 : c3}"/>`;
    }).join('');
    const lines = wrapText(title, 15);
    body = `
      <defs>${GRAIN_DEF}</defs>${circles}
      <circle cx="${cx.toFixed(0)}" cy="${cy.toFixed(0)}" r="16" fill="${hot}"/>
      <rect x="14" y="204" width="${COVER_SIZE - 28}" height="82" rx="10" fill="${bg}" opacity="0.92"/>
      ${svgText(lines, 26, 236, 26, 'fill="#ffffff" font-size="22" font-weight="700"')}
      <text x="26" y="274" fill="${c2}" font-size="12" font-weight="500" letter-spacing="1.2">${escXml(artist.toUpperCase())}</text>
      ${GRAIN_RECT}`;
  } else if (design === 4) {
    const [deep, mid, , light, hot] = pal;
    const cx = 110 + rand() * 80;
    const bars = Array.from({ length: 6 }, (_, i) => `<rect x="0" y="${174 + i * 14}" width="${COVER_SIZE}" height="${2.2 + i * 1.4}" fill="${deep}"/>`).join('');
    const lines = wrapText(title.toUpperCase(), 14);
    body = `
      <defs>
        <linearGradient id="sky" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="${deep}"/><stop offset="1" stop-color="${mid}"/></linearGradient>
        <linearGradient id="sun" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="${light}"/><stop offset="1" stop-color="${hot}"/></linearGradient>
        ${GRAIN_DEF}
      </defs>
      <rect width="${COVER_SIZE}" height="${COVER_SIZE}" fill="url(#sky)"/>
      <circle cx="${cx.toFixed(0)}" cy="196" r="72" fill="url(#sun)"/>${bars}
      <rect x="0" y="254" width="${COVER_SIZE}" height="46" fill="${deep}"/>
      ${svgText(lines, 20, 48, 28, `fill="${light}" font-size="23" font-weight="700" letter-spacing="1"`)}
      <text x="20" y="282" fill="${hot}" font-size="12" font-weight="600" letter-spacing="1.4">${escXml(artist.toUpperCase())}</text>
      ${GRAIN_RECT}`;
  } else {
    const [bg, c1, c2, c3, accent] = pal;
    const lines = wrapText(title.toUpperCase(), 12);
    body = `
      <defs>${GRAIN_DEF}</defs>
      <rect width="${COVER_SIZE}" height="${COVER_SIZE}" fill="${bg}"/>
      <rect x="0" y="0" width="${COVER_SIZE}" height="12" fill="${accent}"/>
      <circle cx="242" cy="238" r="78" fill="${c1}" opacity="0.55"/>
      <circle cx="242" cy="238" r="42" fill="${c2}" opacity="0.75"/>
      ${svgText(lines, 20, 74, 38, `fill="${c3}" font-size="34" font-weight="700" letter-spacing="-1"`)}
      <text x="20" y="276" fill="${accent}" font-size="13" font-weight="600" letter-spacing="1.6">${escXml(artist.toUpperCase())}</text>
      ${GRAIN_RECT}`;
  }

  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="${COVER_SIZE}" height="${COVER_SIZE}" viewBox="0 0 ${COVER_SIZE} ${COVER_SIZE}">${FONT_CSS}${body}</svg>`;
  coverCache.set(cacheKey, svg);
  return svg;
}

// --- Synthesized playable ambient pad WAV (16-bit PCM, 22050 Hz, 24s) ---
function buildAmbientPadWav(durationSecs = 24) {
  const sampleRate = 22050;
  const numSamples = sampleRate * durationSecs;
  const dataBytes = numSamples * 2;
  const buf = Buffer.alloc(44 + dataBytes);

  buf.write('RIFF', 0);
  buf.writeUInt32LE(36 + dataBytes, 4);
  buf.write('WAVE', 8);
  buf.write('fmt ', 12);
  buf.writeUInt32LE(16, 16);
  buf.writeUInt16LE(1, 20); // PCM
  buf.writeUInt16LE(1, 22); // mono
  buf.writeUInt32LE(sampleRate, 24);
  buf.writeUInt32LE(sampleRate * 2, 28);
  buf.writeUInt16LE(2, 32);
  buf.writeUInt16LE(16, 34);
  buf.write('data', 36);
  buf.writeUInt32LE(dataBytes, 40);

  // 4 gentle 6-second ambient chords (Dm9 -> Bbmaj7 -> Fmaj7 -> C6)
  const chords = [
    [146.83, 220.0, 261.63, 329.63],
    [116.54, 174.61, 220.0, 293.66],
    [174.61, 220.0, 261.63, 329.63],
    [130.81, 196.0, 220.0, 261.63],
  ];

  for (let i = 0; i < numSamples; i++) {
    const t = i / sampleRate;
    const chordIdx = Math.min(chords.length - 1, Math.floor(t / 6));
    const localT = t - chordIdx * 6;
    const env = Math.min(1, localT / 0.8) * Math.min(1, (6 - localT) / 0.8);
    const fadeEnds = Math.min(1, t / 0.5) * Math.min(1, (durationSecs - t) / 0.8);
    const notes = chords[chordIdx];
    let sample = 0;
    for (let n = 0; n < notes.length; n++) {
      const f = notes[n];
      sample += Math.sin(2 * Math.PI * f * t) * (0.22 - n * 0.03);
      sample += Math.sin(2 * Math.PI * (f * 2) * t) * 0.04;
    }
    const lfo = 0.85 + 0.15 * Math.sin(2 * Math.PI * 0.25 * t);
    const val = Math.max(-1, Math.min(1, sample * env * fadeEnds * lfo * 0.35));
    buf.writeInt16LE(Math.round(val * 32767), 44 + i * 2);
  }
  return buf;
}

const DEMO_WAV = buildAmbientPadWav(24);

// --- Seeded library dataset ---
const ARTISTS = [
  { id: 'ar-hollow', name: 'Hollow Pines', mbzArtistId: 'a1000000-0000-4000-8000-000000000001', variant: 0 },
  { id: 'ar-vela', name: 'Vela Norte', mbzArtistId: 'a1000000-0000-4000-8000-000000000002', variant: 1 },
  { id: 'ar-static', name: 'Static Harbor', mbzArtistId: 'a1000000-0000-4000-8000-000000000003', variant: 2 },
  { id: 'ar-marrow', name: 'Marrow Choir', mbzArtistId: 'a1000000-0000-4000-8000-000000000004', variant: 3 },
  { id: 'ar-luma', name: 'Luma Drift', mbzArtistId: 'a1000000-0000-4000-8000-000000000005', variant: 4 },
  { id: 'ar-orchid', name: 'Orchid Telegraph', mbzArtistId: 'a1000000-0000-4000-8000-000000000006', variant: 5 },
  { id: 'ar-cinder', name: 'Cinder Vale', mbzArtistId: 'a1000000-0000-4000-8000-000000000007', variant: 6 },
  { id: 'ar-clair', name: 'Clair Obscur', mbzArtistId: 'a1000000-0000-4000-8000-000000000008', variant: 7 },
];

const ALBUMS = [
  { id: 'al-night-garden', name: 'Night Garden', artistId: 'ar-hollow', artist: 'Hollow Pines', year: 2016, genre: 'Dream Pop', variant: 0, created: '2026-10-12T10:00:00Z' },
  { id: 'al-senders', name: 'Senders', artistId: 'ar-vela', artist: 'Vela Norte', year: 2020, genre: 'Electronic', variant: 1, created: '2026-10-18T12:00:00Z' },
  { id: 'al-glass-atlas', name: 'Glass Atlas', artistId: 'ar-static', artist: 'Static Harbor', year: 2004, genre: 'Indie Rock', variant: 2, created: '2026-10-22T14:00:00Z' },
  { id: 'al-copper-season', name: 'Copper Season', artistId: 'ar-marrow', artist: 'Marrow Choir', year: 1994, genre: 'Shoegaze', variant: 3, created: '2026-10-28T16:00:00Z' },
  { id: 'al-solstice', name: 'Solstice Echoes', artistId: 'ar-luma', artist: 'Luma Drift', year: 2024, genre: 'Ambient', variant: 4, created: '2026-11-05T18:00:00Z' },
  { id: 'al-velvet', name: 'Velvet Horizon', artistId: 'ar-orchid', artist: 'Orchid Telegraph', year: 1986, genre: 'Post-Rock', variant: 5, created: '2026-11-12T09:00:00Z' },
  { id: 'al-northbound', name: 'Northbound Ephemera', artistId: 'ar-cinder', artist: 'Cinder Vale', year: 2025, genre: 'Indie Rock', variant: 6, created: '2026-11-19T11:00:00Z' },
  { id: 'al-minuit', name: 'Minuit Analogique', artistId: 'ar-clair', artist: 'Clair Obscur', year: 2025, genre: 'Electronic', variant: 7, created: '2026-11-22T20:00:00Z' },
];

const SONGS = [
  // Night Garden (Hollow Pines)
  { id: 'song-moth', albumId: 'al-night-garden', artistId: 'ar-hollow', title: 'Moth Lantern', track: 1, duration: 252, rating: 5, playCount: 148, starred: '2026-11-18T21:14:00Z', bitDepth: 24, samplingRate: 96000, genre: 'Dream Pop' },
  { id: 'song-cold-frame', albumId: 'al-night-garden', artistId: 'ar-hollow', title: 'Cold Frame', track: 2, duration: 227, rating: 4, playCount: 84, starred: null, bitDepth: 24, samplingRate: 96000, genre: 'Dream Pop' },
  { id: 'song-wisteria', albumId: 'al-night-garden', artistId: 'ar-hollow', title: 'Wisteria', track: 3, duration: 303, rating: 5, playCount: 119, starred: '2026-11-09T23:12:00Z', bitDepth: 24, samplingRate: 96000, genre: 'Dream Pop' },
  { id: 'song-greenhouse', albumId: 'al-night-garden', artistId: 'ar-hollow', title: 'Greenhouse Radio', track: 4, duration: 238, rating: 4, playCount: 62, starred: null, bitDepth: 24, samplingRate: 96000, genre: 'Dream Pop' },
  // Senders (Vela Norte)
  { id: 'song-antenna', albumId: 'al-senders', artistId: 'ar-vela', title: 'Quiet Antenna', track: 1, duration: 238, rating: 5, playCount: 131, starred: '2026-11-15T19:02:00Z', bitDepth: 24, samplingRate: 48000, genre: 'Electronic' },
  { id: 'song-telemetry', albumId: 'al-senders', artistId: 'ar-vela', title: 'Night Telemetry', track: 2, duration: 264, rating: 4, playCount: 77, starred: null, bitDepth: 24, samplingRate: 48000, genre: 'Electronic' },
  { id: 'song-relay', albumId: 'al-senders', artistId: 'ar-vela', title: 'Substation Relay', track: 3, duration: 211, rating: 3, playCount: 38, starred: null, bitDepth: 16, samplingRate: 44100, genre: 'Electronic', lastPlayed: '2025-02-10T18:00:00Z' },
  // Glass Atlas (Static Harbor, 2004 -> Decade 2000s + Forgotten Gems)
  { id: 'song-lanterns', albumId: 'al-glass-atlas', artistId: 'ar-static', title: 'Paper Lanterns', track: 1, duration: 274, rating: 4, playCount: 104, starred: '2026-11-11T08:40:00Z', bitDepth: 16, samplingRate: 44100, genre: 'Indie Rock' },
  { id: 'song-harbor', albumId: 'al-glass-atlas', artistId: 'ar-static', title: 'Low Tide Lights', track: 2, duration: 245, rating: 4, playCount: 69, starred: null, bitDepth: 16, samplingRate: 44100, genre: 'Indie Rock', lastPlayed: '2025-01-20T14:00:00Z' },
  { id: 'song-breakwater', albumId: 'al-glass-atlas', artistId: 'ar-static', title: 'Breakwater Hymn', track: 3, duration: 289, rating: 4, playCount: 41, starred: null, bitDepth: 16, samplingRate: 44100, genre: 'Indie Rock', lastPlayed: '2025-03-04T20:00:00Z' },
  // Copper Season (Marrow Choir, 1994 -> Decade 90s + Forgotten Gems)
  { id: 'song-kiln', albumId: 'al-copper-season', artistId: 'ar-marrow', title: 'Kiln Weather', track: 1, duration: 221, rating: 4, playCount: 92, starred: '2026-11-04T17:30:00Z', bitDepth: 16, samplingRate: 44100, genre: 'Shoegaze' },
  { id: 'song-cinder', albumId: 'al-copper-season', artistId: 'ar-marrow', title: 'Amber Glass', track: 2, duration: 256, rating: 5, playCount: 88, starred: null, bitDepth: 24, samplingRate: 48000, genre: 'Shoegaze', lastPlayed: '2025-02-18T11:00:00Z' },
  { id: 'song-oxide', albumId: 'al-copper-season', artistId: 'ar-marrow', title: 'Oxide Glow', track: 3, duration: 198, rating: 2, playCount: 14, starred: null, bitDepth: 16, samplingRate: 44100, genre: 'Shoegaze' },
  // Solstice Echoes (Luma Drift)
  { id: 'song-midnight', albumId: 'al-solstice', artistId: 'ar-luma', title: 'Solstice Echoes', track: 1, duration: 265, rating: 5, playCount: 96, starred: '2026-11-01T20:45:00Z', bitDepth: 24, samplingRate: 88200, genre: 'Ambient' },
  { id: 'song-frost', albumId: 'al-solstice', artistId: 'ar-luma', title: 'First Frost Bell', track: 2, duration: 234, rating: 4, playCount: 53, starred: null, bitDepth: 24, samplingRate: 88200, genre: 'Ambient' },
  // Velvet Horizon (Orchid Telegraph, 1986 -> Decade 80s + Forgotten Gems)
  { id: 'song-velvet', albumId: 'al-velvet', artistId: 'ar-orchid', title: 'Velvet Horizon', track: 1, duration: 318, rating: 5, playCount: 74, starred: '2026-10-29T22:15:00Z', bitDepth: 24, samplingRate: 96000, genre: 'Post-Rock' },
  { id: 'song-signal', albumId: 'al-velvet', artistId: 'ar-orchid', title: 'Distant Signal', track: 2, duration: 276, rating: 4, playCount: 47, starred: null, bitDepth: 24, samplingRate: 96000, genre: 'Post-Rock', lastPlayed: '2025-01-12T19:00:00Z' },
  { id: 'song-static-tape', albumId: 'al-velvet', artistId: 'ar-orchid', title: 'Static Tape Loop', track: 3, duration: 184, rating: 1, playCount: 4, starred: null, bitDepth: 16, samplingRate: 44100, genre: 'Post-Rock' },
  // Discovery Triage tracks (matching seeded discovery_tracks.song_id!)
  { id: 'tri-1', albumId: 'al-solstice', artistId: 'ar-luma', title: 'Winter Nocturne', track: 3, duration: 248, rating: 0, playCount: 2, starred: null, bitDepth: 24, samplingRate: 48000, genre: 'Dream Pop', discoveryPath: 'library/Discovery/Balanced/Winter Nocturne.flac' },
  { id: 'tri-2', albumId: 'al-northbound', artistId: 'ar-cinder', title: 'Snowline Telemetry', track: 1, duration: 231, rating: 0, playCount: 1, starred: null, bitDepth: 16, samplingRate: 44100, genre: 'Indie Rock', discoveryPath: 'library/Discovery/Balanced/Snowline Telemetry.flac' },
  { id: 'tri-3', albumId: 'al-minuit', artistId: 'ar-clair', title: 'Rue des Étoiles', track: 1, duration: 262, rating: 0, playCount: 0, starred: null, bitDepth: 24, samplingRate: 96000, genre: 'Electronic', discoveryPath: 'library/Discovery/Adventurous/Rue des Etoiles.flac' },
  { id: 'tri-4', albumId: 'al-glass-atlas', artistId: 'ar-static', title: 'Phosphor Drift', track: 4, duration: 254, rating: 0, playCount: 1, starred: null, bitDepth: 24, samplingRate: 48000, genre: 'Shoegaze', discoveryPath: 'library/Discovery/Balanced/Phosphor Drift.flac' },
  { id: 'tri-5', albumId: 'al-solstice', artistId: 'ar-luma', title: 'Cedar & Frost', track: 4, duration: 219, rating: 0, playCount: 0, starred: null, bitDepth: 16, samplingRate: 44100, genre: 'Ambient', discoveryPath: 'library/Discovery/Conservative/Cedar and Frost.flac' },
];

const PLAYLISTS = [
  {
    id: 'pl-drive',
    name: 'Late Night Drive',
    comment: 'Curated favorites for the road',
    songIds: ['song-moth', 'song-antenna', 'song-lanterns', 'song-velvet', 'song-wisteria', 'song-kiln'],
    rules: null,
  },
  {
    id: 'pl-analog',
    name: 'Analog Warmth',
    comment: 'Tape-saturated indie & dream pop',
    songIds: ['song-wisteria', 'song-cold-frame', 'song-kiln', 'song-cinder', 'song-midnight', 'song-harbor'],
    rules: null,
  },
  {
    id: 'pl-fresh',
    name: 'Discovery · Fresh Picks',
    comment: 'Soulbeet Discovery · Balanced',
    songIds: ['tri-1', 'tri-2', 'tri-3', 'tri-4', 'tri-5'],
    rules: { all: [{ contains: { filepath: 'Discovery/Balanced/' } }] },
  },
  {
    id: 'pl-winter',
    name: 'Christmas Mix',
    comment: 'soulbeet:contextual:winter_holiday',
    songIds: ['song-midnight', 'song-frost', 'tri-1', 'tri-3', 'tri-5', 'song-moth', 'song-velvet'],
    rules: { all: [{ contains: { filepath: 'Discovery/Seasonal/winter_holiday/' } }] },
  },
  {
    id: 'pl-drive-mix',
    name: 'Drive Mix',
    comment: 'soulbeet:contextual:drive',
    songIds: ['song-moth', 'song-antenna', 'song-lanterns', 'song-greenhouse', 'song-telemetry', 'song-velvet'],
    rules: null,
  },
  {
    id: 'pl-late-night',
    name: 'Late Night',
    comment: 'soulbeet:contextual:late_night',
    songIds: ['song-wisteria', 'song-midnight', 'song-signal', 'tri-3', 'tri-4', 'song-frost'],
    rules: null,
  },
];

const albumMap = new Map(ALBUMS.map((a) => [a.id, a]));
const artistMap = new Map(ARTISTS.map((a) => [a.id, a]));
const songMap = new Map(SONGS.map((s) => [s.id, s]));

function songPath(s) {
  if (s.discoveryPath) return s.discoveryPath;
  const alb = albumMap.get(s.albumId);
  const art = artistMap.get(s.artistId);
  return `${art.name}/${alb.name}/${String(s.track).padStart(2, '0')} ${s.title}.flac`;
}

function toSubsonicSong(s) {
  const alb = albumMap.get(s.albumId);
  const art = artistMap.get(s.artistId);
  const out = {
    id: s.id,
    parent: s.albumId,
    isDir: false,
    title: s.title,
    album: alb.name,
    artist: art.name,
    track: s.track,
    year: alb.year,
    genre: s.genre,
    genres: [{ name: s.genre }],
    coverArt: s.id,
    size: 26_000_000 + s.duration * 35_000,
    contentType: 'audio/flac',
    suffix: 'flac',
    duration: s.duration,
    bitRate: s.bitDepth === 24 ? 1480 : 960,
    bitDepth: s.bitDepth,
    samplingRate: s.samplingRate,
    path: songPath(s),
    playCount: s.playCount,
    albumId: s.albumId,
    artistId: s.artistId,
    type: 'music',
    created: alb.created,
    played: s.playCount > 0 ? (s.lastPlayed ?? '2026-11-26T20:00:00Z') : undefined,
  };
  if (s.rating > 0) out.userRating = s.rating;
  if (s.starred) out.starred = s.starred;
  return out;
}

function toNativeSong(s) {
  const alb = albumMap.get(s.albumId);
  const art = artistMap.get(s.artistId);
  return {
    id: s.id,
    path: songPath(s),
    title: s.title,
    size: 26_000_000 + s.duration * 35_000,
    libraryId: 1,
    libraryPath: LIBRARY_PATH,
    missing: false,
    updatedAt: '2026-11-24T10:00:00Z',
    createdAt: alb.created,
    artist: art.name,
    album: alb.name,
    albumArtist: art.name,
    albumId: s.albumId,
    artistId: s.artistId,
    trackNumber: s.track,
    year: alb.year,
    duration: Number(s.duration),
    suffix: 'flac',
    bitRate: s.bitDepth === 24 ? 1480 : 960,
    rating: s.rating,
    averageRating: s.rating,
    starred: Boolean(s.starred),
    starredAt: s.starred ?? null,
    ratedAt: s.rating > 0 ? '2026-11-20T12:00:00Z' : null,
    playCount: s.playCount,
    playDate: s.playCount > 0 ? (s.lastPlayed ?? '2026-11-26T20:00:00Z') : null,
    genre: s.genre,
    genres: [{ name: s.genre }],
    participants: {
      artist: [{ id: art.id, name: art.name }],
      albumartist: [{ id: art.id, name: art.name }],
    },
  };
}

function toSubsonicAlbum(a) {
  const songs = SONGS.filter((s) => s.albumId === a.id);
  const duration = songs.reduce((sum, s) => sum + s.duration, 0);
  const playCount = songs.reduce((sum, s) => sum + s.playCount, 0);
  const rated = songs.filter((s) => s.rating > 0);
  const avgRating = rated.length ? Math.round(rated.reduce((sum, s) => sum + s.rating, 0) / rated.length) : 0;
  return {
    id: a.id,
    name: a.name,
    title: a.name,
    artist: a.artist,
    artistId: a.artistId,
    coverArt: a.id,
    songCount: songs.length,
    duration,
    playCount,
    year: a.year,
    genre: a.genre,
    genres: [{ name: a.genre }],
    userRating: avgRating || undefined,
    created: a.created,
    played: '2026-11-26T20:00:00Z',
  };
}

function toSubsonicPlaylist(pl) {
  const entries = pl.songIds.map((id) => songMap.get(id)).filter(Boolean);
  const duration = entries.reduce((sum, s) => sum + s.duration, 0);
  return {
    id: pl.id,
    name: pl.name,
    comment: pl.comment,
    owner: 'admin',
    public: false,
    songCount: entries.length,
    duration,
    coverArt: entries[0]?.id ?? 'al-night-garden',
    created: '2026-11-01T10:00:00Z',
    changed: '2026-11-27T18:00:00Z',
  };
}

function toNativePlaylist(pl) {
  return {
    id: pl.id,
    name: pl.name,
    comment: pl.comment,
    ownerName: 'admin',
    public: false,
    sync: false,
    songCount: pl.songIds.length,
    rules: pl.rules ?? null,
  };
}

function subsonicOk(body = {}) {
  return {
    'subsonic-response': {
      status: 'ok',
      version: '1.16.1',
      type: 'navidrome',
      serverVersion: '0.64.2 (dev-seeded)',
      openSubsonic: true,
      ...body,
    },
  };
}

function setCors(res) {
  res.setHeader('Access-Control-Allow-Origin', '*');
  res.setHeader('Access-Control-Allow-Methods', 'GET, POST, PUT, DELETE, OPTIONS');
  res.setHeader('Access-Control-Allow-Headers', '*');
  res.setHeader('Access-Control-Expose-Headers', 'X-Total-Count, Content-Range, Accept-Ranges');
}

function sendJson(res, status, obj, extraHeaders = {}) {
  setCors(res);
  const payload = JSON.stringify(obj);
  res.writeHead(status, {
    'Content-Type': 'application/json; charset=utf-8',
    'Content-Length': Buffer.byteLength(payload),
    ...extraHeaders,
  });
  res.end(payload);
}

async function readBody(req) {
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  return Buffer.concat(chunks).toString('utf8');
}

async function parseParams(req, url) {
  const params = new URLSearchParams(url.searchParams);
  if (req.method === 'POST' || req.method === 'PUT') {
    const raw = await readBody(req);
    const ct = (req.headers['content-type'] ?? '').toLowerCase();
    if (ct.includes('application/x-www-form-urlencoded') && raw) {
      for (const [k, v] of new URLSearchParams(raw)) params.append(k, v);
    } else if (raw) {
      try {
        return { params, json: JSON.parse(raw), raw };
      } catch {}
    }
    return { params, json: null, raw };
  }
  return { params, json: null, raw: '' };
}

function resolveCoverInfo(id) {
  const song = songMap.get(id);
  if (song) {
    const alb = albumMap.get(song.albumId);
    const art = artistMap.get(song.artistId);
    return { key: alb.id, title: alb.name, artist: art.name, variant: alb.variant };
  }
  const alb = albumMap.get(id);
  if (alb) return { key: alb.id, title: alb.name, artist: alb.artist, variant: alb.variant };
  const art = artistMap.get(id);
  if (art) return { key: art.id, title: art.name, artist: 'Artist', variant: art.variant };
  const pl = PLAYLISTS.find((p) => p.id === id);
  if (pl) return { key: pl.id, title: pl.name, artist: 'Soulbeet', variant: 1 };
  return { key: 'fallback-default', title: 'Soulbeet Studio', artist: 'Dev Seeded', variant: 0 };
}

function buildLyrics(song) {
  const art = artistMap.get(song.artistId);
  return {
    displayArtist: art.name,
    displayTitle: song.title,
    lang: 'en',
    synced: true,
    offset: 0,
    line: [
      { start: 1000, value: `Midnight tape hiss under ${song.title}` },
      { start: 4500, value: 'Amber filaments glow along the console glass' },
      { start: 8500, value: 'Analog horizons drifting out across the bay' },
      { start: 12500, value: 'Every quiet chord settles in the dark' },
      { start: 16500, value: 'Wait for the morning light to clear the haze' },
      { start: 20500, value: 'Still spinning softly in the studio' },
    ],
  };
}

let nextPlId = 100;

const server = createServer(async (req, res) => {
  try {
    setCors(res);
    if (req.method === 'OPTIONS') {
      res.writeHead(204);
      res.end();
      return;
    }

    const url = new URL(req.url ?? '/', `http://${req.headers.host ?? '127.0.0.1'}`);
    const pathname = url.pathname;
    const { params, json } = await parseParams(req, url);

    // --- Health & Native Navidrome Auth ---
    if (pathname === '/ping') {
      res.writeHead(200, { 'Content-Type': 'text/plain' });
      res.end('ok');
      return;
    }

    if (pathname === '/auth/login' && req.method === 'POST') {
      const username = json?.username ?? 'admin';
      sendJson(res, 200, {
        id: 'u-admin',
        username,
        name: username,
        isAdmin: true,
        token: 'dev-seeded-navidrome-jwt',
        subsonicSalt: 'devseed',
        subsonicToken: 'deadbeef01234567',
      });
      return;
    }

    // --- Native Navidrome REST API (/api/*) ---
    if (pathname === '/api/song') {
      const ids = params.getAll('id');
      const titleFilter = (params.get('title') ?? '').toLowerCase();
      const missingParam = params.get('missing');
      let list = SONGS.map(toNativeSong);
      if (missingParam === 'true') list = list.filter((s) => s.missing === true);
      else if (missingParam === 'false') list = list.filter((s) => s.missing === false);
      if (ids.length > 0) list = list.filter((s) => ids.includes(s.id));
      if (titleFilter) list = list.filter((s) => s.title.toLowerCase().includes(titleFilter));
      const start = Number(params.get('_start') ?? 0);
      const end = Number(params.get('_end') ?? list.length);
      const page = list.slice(start, end);
      sendJson(res, 200, page, { 'X-Total-Count': String(list.length) });
      return;
    }

    const songPlaylistsMatch = pathname.match(/^\/api\/song\/([^/]+)\/playlists$/);
    if (songPlaylistsMatch) {
      const songId = decodeURIComponent(songPlaylistsMatch[1]);
      const matching = PLAYLISTS.filter((p) => p.songIds.includes(songId)).map(toNativePlaylist);
      sendJson(res, 200, matching, { 'X-Total-Count': String(matching.length) });
      return;
    }

    if (pathname === '/api/artist') {
      sendJson(res, 200, ARTISTS, { 'X-Total-Count': String(ARTISTS.length) });
      return;
    }

    if (pathname === '/api/user') {
      sendJson(res, 200, [{ id: 'u-admin', userName: 'admin', isAdmin: true }], { 'X-Total-Count': '1' });
      return;
    }

    if (pathname === '/api/library') {
      sendJson(res, 200, [{ id: 1, name: 'Music Library', path: LIBRARY_PATH }], { 'X-Total-Count': '1' });
      return;
    }

    if (pathname === '/api/playlist' && req.method === 'GET') {
      const list = PLAYLISTS.map(toNativePlaylist);
      sendJson(res, 200, list, { 'X-Total-Count': String(list.length) });
      return;
    }

    if (pathname === '/api/playlist' && req.method === 'POST') {
      const id = `pl-dev-${nextPlId++}`;
      const pl = {
        id,
        name: json?.name ?? 'Untitled Playlist',
        comment: json?.comment ?? '',
        songIds: [],
        rules: json?.rules ?? null,
      };
      PLAYLISTS.push(pl);
      sendJson(res, 200, { id: pl.id });
      return;
    }

    const plTracksMatch = pathname.match(/^\/api\/playlist\/([^/]+)\/tracks$/);
    if (plTracksMatch) {
      const plId = decodeURIComponent(plTracksMatch[1]);
      const pl = PLAYLISTS.find((p) => p.id === plId);
      if (!pl) {
        sendJson(res, 404, { error: `playlist(id:${plId}) not found` });
        return;
      }
      const rows = pl.songIds
        .map((sid) => songMap.get(sid))
        .filter(Boolean)
        .map((s, idx) => ({ ...toNativeSong(s), id: String(idx + 1), mediaFileId: s.id }));
      sendJson(res, 200, rows, { 'X-Total-Count': String(rows.length) });
      return;
    }

    const plMatch = pathname.match(/^\/api\/playlist\/([^/]+)$/);
    if (plMatch) {
      const plId = decodeURIComponent(plMatch[1]);
      const idx = PLAYLISTS.findIndex((p) => p.id === plId);
      if (idx === -1) {
        sendJson(res, 404, { error: `playlist(id:${plId}) not found` });
        return;
      }
      if (req.method === 'GET') {
        sendJson(res, 200, toNativePlaylist(PLAYLISTS[idx]));
        return;
      }
      if (req.method === 'PUT') {
        if (json?.name !== undefined) PLAYLISTS[idx].name = json.name;
        if (json?.comment !== undefined) PLAYLISTS[idx].comment = json.comment;
        if (json?.rules !== undefined) PLAYLISTS[idx].rules = json.rules;
        sendJson(res, 200, { id: plId });
        return;
      }
      if (req.method === 'DELETE') {
        PLAYLISTS.splice(idx, 1);
        sendJson(res, 200, {});
        return;
      }
    }

    // --- OpenSubsonic API (/rest/*) ---
    if (pathname.startsWith('/rest/')) {
      const ep = pathname.slice('/rest/'.length).replace(/\.view$/, '');

      if (ep === 'getCoverArt') {
        const id = params.get('id') ?? 'al-night-garden';
        const info = resolveCoverInfo(id);
        const svg = drawCoverSvg(info.key, info.title, info.artist, info.variant);
        res.writeHead(200, {
          'Content-Type': 'image/svg+xml; charset=utf-8',
          'Content-Security-Policy': "default-src 'none'; style-src 'unsafe-inline'",
          'Cache-Control': 'public, max-age=3600',
          'Content-Length': Buffer.byteLength(svg),
        });
        res.end(req.method === 'HEAD' ? undefined : svg);
        return;
      }

      if (ep === 'stream' || ep === 'download') {
        const total = DEMO_WAV.length;
        const range = req.headers.range;
        if (range) {
          const m = range.match(/^bytes=(\d*)-(\d*)$/);
          if (!m || (!m[1] && !m[2])) {
            res.writeHead(416, { 'Content-Range': `bytes */${total}` });
            res.end();
            return;
          }
          let start;
          let end;
          if (!m[1]) {
            // Suffix range: bytes=-N (last N bytes)
            const suffix = Number(m[2]);
            if (suffix <= 0) {
              res.writeHead(416, { 'Content-Range': `bytes */${total}` });
              res.end();
              return;
            }
            start = Math.max(0, total - suffix);
            end = total - 1;
          } else {
            start = Number(m[1]);
            end = m[2] ? Math.min(total - 1, Number(m[2])) : total - 1;
          }
          if (start >= total || start > end) {
            res.writeHead(416, { 'Content-Range': `bytes */${total}` });
            res.end();
            return;
          }
          const chunk = DEMO_WAV.subarray(start, end + 1);
          res.writeHead(206, {
            'Content-Type': 'audio/wav',
            'Accept-Ranges': 'bytes',
            'Content-Range': `bytes ${start}-${end}/${total}`,
            'Content-Length': String(chunk.length),
          });
          res.end(req.method === 'HEAD' ? undefined : chunk);
        } else {
          res.writeHead(200, {
            'Content-Type': 'audio/wav',
            'Accept-Ranges': 'bytes',
            'Content-Length': String(total),
          });
          res.end(req.method === 'HEAD' ? undefined : DEMO_WAV);
        }
        return;
      }

      if (ep === 'ping') {
        sendJson(res, 200, subsonicOk());
        return;
      }

      if (ep === 'getUser') {
        sendJson(res, 200, subsonicOk({ user: { username: params.get('username') ?? 'admin', adminRole: true } }));
        return;
      }

      if (ep === 'getScanStatus') {
        sendJson(
          res,
          200,
          subsonicOk({
            scanStatus: {
              scanning: false,
              count: SONGS.length,
              lastScan: '2026-11-27T12:00:00Z',
              elapsedTime: 420_000_000,
            },
          }),
        );
        return;
      }

      if (ep === 'startScan') {
        sendJson(res, 200, subsonicOk({ scanStatus: { scanning: false, count: SONGS.length } }));
        return;
      }

      if (ep === 'getAlbumList2') {
        const offset = Number(params.get('offset') ?? 0);
        const size = Number(params.get('size') ?? 20);
        const list = ALBUMS.map(toSubsonicAlbum).slice(offset, offset + size);
        sendJson(res, 200, subsonicOk({ albumList2: { album: list } }));
        return;
      }

      if (ep === 'getAlbum') {
        const id = params.get('id') ?? '';
        const alb = albumMap.get(id);
        if (!alb) {
          sendJson(res, 200, { 'subsonic-response': { status: 'failed', version: '1.16.1', error: { code: 70, message: 'Album not found' } } });
          return;
        }
        const songs = SONGS.filter((s) => s.albumId === id).map(toSubsonicSong);
        sendJson(res, 200, subsonicOk({ album: { ...toSubsonicAlbum(alb), song: songs } }));
        return;
      }

      if (ep === 'getArtist') {
        const id = params.get('id') ?? '';
        const art = artistMap.get(id);
        if (!art) {
          sendJson(res, 200, { 'subsonic-response': { status: 'failed', version: '1.16.1', error: { code: 70, message: 'Artist not found' } } });
          return;
        }
        const albums = ALBUMS.filter((a) => a.artistId === id).map(toSubsonicAlbum);
        sendJson(
          res,
          200,
          subsonicOk({
            artist: {
              id: art.id,
              name: art.name,
              coverArt: art.id,
              albumCount: albums.length,
              album: albums,
            },
          }),
        );
        return;
      }

      if (ep === 'getArtists') {
        const artists = ARTISTS.map((art) => ({
          id: art.id,
          name: art.name,
          coverArt: art.id,
          albumCount: ALBUMS.filter((a) => a.artistId === art.id).length,
        }));
        sendJson(res, 200, subsonicOk({ artists: { index: [{ name: 'A-Z', artist: artists }] } }));
        return;
      }

      if (ep === 'getSong') {
        const id = params.get('id') ?? '';
        const s = songMap.get(id);
        if (!s) {
          sendJson(res, 200, { 'subsonic-response': { status: 'failed', version: '1.16.1', error: { code: 70, message: 'Song not found' } } });
          return;
        }
        sendJson(res, 200, subsonicOk({ song: toSubsonicSong(s) }));
        return;
      }

      if (ep === 'getPlaylists') {
        sendJson(res, 200, subsonicOk({ playlists: { playlist: PLAYLISTS.map(toSubsonicPlaylist) } }));
        return;
      }

      if (ep === 'getPlaylist') {
        const id = params.get('id') ?? '';
        const pl = PLAYLISTS.find((p) => p.id === id);
        if (!pl) {
          sendJson(res, 200, { 'subsonic-response': { status: 'failed', version: '1.16.1', error: { code: 70, message: 'Playlist not found' } } });
          return;
        }
        const entries = pl.songIds.map((sid) => songMap.get(sid)).filter(Boolean).map(toSubsonicSong);
        sendJson(res, 200, subsonicOk({ playlist: { ...toSubsonicPlaylist(pl), entry: entries } }));
        return;
      }

      if (ep === 'getStarred2') {
        const songs = SONGS.filter((s) => Boolean(s.starred)).map(toSubsonicSong);
        sendJson(res, 200, subsonicOk({ starred2: { song: songs, album: [], artist: [] } }));
        return;
      }

      if (ep === 'search3') {
        const q = (params.get('query') ?? '').trim().replace(/^"+|"+$/g, '').toLowerCase();
        const songCount = Number(params.get('songCount') ?? 20);
        const songOffset = Number(params.get('songOffset') ?? 0);
        const albumCount = Number(params.get('albumCount') ?? 20);
        const artistCount = Number(params.get('artistCount') ?? 20);

        const matchSong = (s) => {
          if (!q) return true;
          const alb = albumMap.get(s.albumId);
          const art = artistMap.get(s.artistId);
          return (
            s.title.toLowerCase().includes(q) ||
            alb.name.toLowerCase().includes(q) ||
            art.name.toLowerCase().includes(q) ||
            s.genre.toLowerCase().includes(q)
          );
        };
        const matchAlbum = (a) => !q || a.name.toLowerCase().includes(q) || a.artist.toLowerCase().includes(q);
        const matchArtist = (a) => !q || a.name.toLowerCase().includes(q);

        const songs = SONGS.filter(matchSong).slice(songOffset, songOffset + songCount).map(toSubsonicSong);
        const albums = ALBUMS.filter(matchAlbum).slice(0, albumCount).map(toSubsonicAlbum);
        const artists = ARTISTS.filter(matchArtist)
          .slice(0, artistCount)
          .map((art) => ({
            id: art.id,
            name: art.name,
            coverArt: art.id,
            albumCount: ALBUMS.filter((a) => a.artistId === art.id).length,
          }));

        sendJson(res, 200, subsonicOk({ searchResult3: { song: songs, album: albums, artist: artists } }));
        return;
      }

      if (ep === 'getLyricsBySongId') {
        const id = params.get('id') ?? '';
        const song = songMap.get(id) ?? SONGS[0];
        sendJson(res, 200, subsonicOk({ lyricsList: { structuredLyrics: [buildLyrics(song)] } }));
        return;
      }

      if (ep === 'getLyrics') {
        const song = SONGS[0];
        sendJson(
          res,
          200,
          subsonicOk({
            lyrics: {
              artist: params.get('artist') ?? 'Hollow Pines',
              title: params.get('title') ?? song.title,
              value: buildLyrics(song)
                .line.map((l) => l.value)
                .join('\n'),
            },
          }),
        );
        return;
      }

      if (ep === 'getRandomSongs') {
        const size = Number(params.get('size') ?? 10);
        sendJson(res, 200, subsonicOk({ randomSongs: { song: SONGS.slice(0, size).map(toSubsonicSong) } }));
        return;
      }

      if (ep === 'getSimilarSongs2') {
        const count = Number(params.get('count') ?? 10);
        sendJson(res, 200, subsonicOk({ similarSongs2: { song: SONGS.slice(0, count).map(toSubsonicSong) } }));
        return;
      }

      if (ep === 'setRating') {
        const id = params.get('id') ?? '';
        const rating = Number(params.get('rating') ?? 0);
        const s = songMap.get(id);
        if (s) s.rating = Math.max(0, Math.min(5, rating));
        sendJson(res, 200, subsonicOk());
        return;
      }

      if (ep === 'star') {
        for (const id of params.getAll('id')) {
          const s = songMap.get(id);
          if (s) s.starred = new Date().toISOString();
        }
        sendJson(res, 200, subsonicOk());
        return;
      }

      if (ep === 'unstar') {
        for (const id of params.getAll('id')) {
          const s = songMap.get(id);
          if (s) s.starred = null;
        }
        sendJson(res, 200, subsonicOk());
        return;
      }

      if (ep === 'scrobble') {
        const id = params.get('id') ?? '';
        const submission = params.get('submission') !== 'false';
        const s = songMap.get(id);
        if (s && submission) s.playCount += 1;
        sendJson(res, 200, subsonicOk());
        return;
      }

      if (ep === 'createPlaylist') {
        const name = params.get('name') ?? 'New Playlist';
        const songIds = params.getAll('songId');
        const pl = { id: `pl-dev-${nextPlId++}`, name, comment: '', songIds, rules: null };
        PLAYLISTS.push(pl);
        sendJson(res, 200, subsonicOk({ playlist: toSubsonicPlaylist(pl) }));
        return;
      }

      if (ep === 'updatePlaylist') {
        const plId = params.get('playlistId') ?? '';
        const pl = PLAYLISTS.find((p) => p.id === plId);
        if (pl) {
          if (params.has('name')) pl.name = params.get('name');
          if (params.has('comment')) pl.comment = params.get('comment');
          for (const addId of params.getAll('songIdToAdd')) {
            if (!pl.songIds.includes(addId)) pl.songIds.push(addId);
          }
          const removeIdxs = new Set(params.getAll('songIndexToRemove').map(Number));
          if (removeIdxs.size > 0) {
            pl.songIds = pl.songIds.filter((_, idx) => !removeIdxs.has(idx));
          }
        }
        sendJson(res, 200, subsonicOk());
        return;
      }

      if (ep === 'deletePlaylist') {
        const id = params.get('id') ?? '';
        const idx = PLAYLISTS.findIndex((p) => p.id === id);
        if (idx !== -1) PLAYLISTS.splice(idx, 1);
        sendJson(res, 200, subsonicOk());
        return;
      }

      // Catch-all ok for any auxiliary Subsonic call
      sendJson(res, 200, subsonicOk());
      return;
    }

    res.writeHead(404, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ error: 'Not found', path: pathname }));
  } catch (err) {
    console.error('[mock-navidrome] error:', err);
    res.writeHead(500, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ error: String(err) }));
  }
});

server.listen(PORT, HOST, () => {
  console.log(`[mock-navidrome] listening on http://${HOST}:${PORT} (${SONGS.length} songs, ${ALBUMS.length} albums, ${PLAYLISTS.length} playlists)`);
});

process.on('SIGTERM', () => {
  server.close(() => process.exit(0));
});
