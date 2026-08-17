const sharp = require("sharp");

const oneByOnePng = Buffer.from(
  // A 1x1 PNG whose IDAT CRC is valid. The previous literal here had a CORRUPT
  // IDAT chunk (CRC mismatch), so libvips rejected it with "vipspng: libpng read
  // error" — a fixture-input bug that looked exactly like an island failure.
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmM" +
    "IQAAAABJRU5ErkJggg==",
  "base64",
);

async function main() {
  const image = sharp(oneByOnePng);
  const metadata = await image.metadata();
  const resized = await image.resize(2, 3).raw().toBuffer({ resolveWithObject: true });
  if (metadata.width !== 1 || metadata.height !== 1 || resized.info.width !== 2 || resized.info.height !== 3) {
    throw new Error(`sharp produced unexpected dimensions: ${JSON.stringify({ metadata, info: resized.info })}`);
  }
  if (resized.data.length !== 2 * 3 * resized.info.channels) {
    throw new Error(`sharp raw byte length ${resized.data.length} does not match its output signature`);
  }

  console.log(JSON.stringify({
    marker: "COMPILE_NATIVE_ISLANDS_OK",
    input: [metadata.width, metadata.height],
    output: [resized.info.width, resized.info.height, resized.info.channels, resized.data.length],
  }));
}

main().catch((error) => {
  console.error(error.stack || error);
  process.exitCode = 1;
});
