import { writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import sharp from "sharp";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");

async function renderPng(input, output, size) {
  await sharp(resolve(root, input))
    .resize(size, size)
    .png()
    .toFile(resolve(root, output));
}

function icoEntry(size, png, offset) {
  const entry = Buffer.alloc(16);
  entry.writeUInt8(size, 0);
  entry.writeUInt8(size, 1);
  entry.writeUInt8(0, 2);
  entry.writeUInt8(0, 3);
  entry.writeUInt16LE(1, 4);
  entry.writeUInt16LE(32, 6);
  entry.writeUInt32LE(png.length, 8);
  entry.writeUInt32LE(offset, 12);
  return entry;
}

async function renderIco(input, output) {
  const images = await Promise.all(
    [16, 32].map(async (size) => ({
      size,
      png: await sharp(resolve(root, input)).resize(size, size).png().toBuffer(),
    })),
  );
  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0);
  header.writeUInt16LE(1, 2);
  header.writeUInt16LE(images.length, 4);

  let offset = header.length + images.length * 16;
  const entries = [];
  for (const image of images) {
    entries.push(icoEntry(image.size, image.png, offset));
    offset += image.png.length;
  }

  writeFileSync(
    resolve(root, output),
    Buffer.concat([header, ...entries, ...images.map((image) => image.png)]),
  );
}

await Promise.all([
  renderPng("assets/logo.svg", "assets/logo.png", 960),
  renderPng("docs/public/logo.svg", "docs/public/logo.png", 960),
  renderPng("docs/public/favicon.svg", "docs/public/android-chrome-192x192.png", 192),
  renderPng("docs/public/favicon.svg", "docs/public/android-chrome-512x512.png", 512),
  renderPng("docs/public/favicon.svg", "docs/public/apple-touch-icon.png", 180),
  renderPng("docs/public/favicon.svg", "docs/public/favicon-16x16.png", 16),
  renderPng("docs/public/favicon.svg", "docs/public/favicon-32x32.png", 32),
  renderIco("docs/public/favicon.svg", "docs/public/favicon.ico"),
]);
