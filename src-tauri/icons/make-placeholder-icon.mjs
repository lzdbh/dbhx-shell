// 生成一个 1024x1024 的占位 app 图标（PNG, RGBA），供 `pnpm tauri icon` 生成各平台图标集。
// 只是编译期的占位物，不代表最终视觉。
import { deflateSync } from "node:zlib";
import { writeFileSync } from "node:fs";

const SIZE = 1024;
const BG = [0x14, 0x1a, 0x22]; // 深色底，贴近终端配色
const BAR = [0x3f, 0xb3, 0x7a]; // 一条亮色“提示行”

const crcTable = Array.from({ length: 256 }, (_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});
const crc32 = (buf) => {
  let c = 0xffffffff;
  for (const b of buf) c = crcTable[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
};
const chunk = (type, data) => {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, "latin1"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
};

const raw = Buffer.alloc(SIZE * (1 + SIZE * 4));
for (let y = 0; y < SIZE; y++) {
  const row = y * (1 + SIZE * 4);
  raw[row] = 0; // filter: None
  const inBar = y >= 430 && y <= 620;
  for (let x = 0; x < SIZE; x++) {
    const o = row + 1 + x * 4;
    // 左侧留出一条终端提示符竖线，其余填底色
    const isPrompt = x >= 220 && x <= 250;
    const px = inBar && !isPrompt && x >= 220 && x <= 800 ? BAR : isPrompt && y >= 380 && y <= 670 ? BAR : BG;
    raw[o] = px[0];
    raw[o + 1] = px[1];
    raw[o + 2] = px[2];
    raw[o + 3] = 255;
  }
}

const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(SIZE, 0);
ihdr.writeUInt32BE(SIZE, 4);
ihdr[8] = 8; // bit depth
ihdr[9] = 6; // color type RGBA
const png = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  chunk("IHDR", ihdr),
  chunk("IDAT", deflateSync(raw, { level: 9 })),
  chunk("IEND", Buffer.alloc(0)),
]);

writeFileSync(process.argv[2] ?? "app-icon.png", png);
console.log("wrote", png.length, "bytes");
