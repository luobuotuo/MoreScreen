/**
 * 极简 QR 码生成器（仅供本项目配对页使用）。
 *
 * 为什么自己写：局域网环境没有外网，不能调用在线二维码 API；
 * 引入第三方库又会给静态目录增加体积与供应链风险。
 *
 * 实现范围（有意的取舍，换取可靠性）：
 * - **Byte 模式**（UTF-8 字节流），足以编码 "http://192.168.x.x:8788/..." 这类 URL；
 * - **版本 3（29x29）固定**，纠错等级 **L**（数据 55 码字 / 纠错 15 码字）；
 *   容得下最长 53 字节的内容，覆盖所有局域网地址；
 * - **固定掩码 0**（不计算罚分）。QR 标准允许任意掩码，解码器必须全部支持，
 *   因此固定掩码完全合法，能省掉一大段罚分逻辑，显著降低出错面。
 *
 * 若内容超长，`qrEncode` 返回 `null`，调用方应退化为纯文本展示。
 */
(function (global) {
  'use strict';

  // ---- GF(256) 运算表（本原多项式 0x11D）----
  const EXP = new Uint8Array(512);
  const LOG = new Uint8Array(256);
  (function () {
    let x = 1;
    for (let i = 0; i < 255; i++) {
      EXP[i] = x;
      LOG[x] = i;
      x <<= 1;
      if (x & 0x100) x ^= 0x11d;
    }
    for (let i = 255; i < 512; i++) EXP[i] = EXP[i - 255];
  })();

  const gmul = (a, b) => (a === 0 || b === 0 ? 0 : EXP[LOG[a] + LOG[b]]);

  /** 生成 n 次 RS 生成多项式（系数从高次到低次）。 */
  function rsGenPoly(n) {
    let g = [1];
    for (let i = 0; i < n; i++) {
      const next = new Array(g.length + 1).fill(0);
      for (let j = 0; j < g.length; j++) {
        next[j] ^= g[j];
        next[j + 1] ^= gmul(g[j], EXP[i]);
      }
      g = next;
    }
    return g;
  }

  /** 计算 RS 纠错码字。 */
  function rsEncode(data, ecLen) {
    const gen = rsGenPoly(ecLen);
    const res = new Array(ecLen).fill(0);
    for (const d of data) {
      const factor = d ^ res[0];
      res.shift();
      res.push(0);
      for (let i = 0; i < ecLen; i++) res[i] ^= gmul(gen[i + 1], factor);
    }
    return res;
  }

  // ---- 版本 3-L 参数 ----
  const VERSION = 3;
  const SIZE = 17 + 4 * VERSION;   // 29
  const DATA_CODEWORDS = 55;
  const EC_CODEWORDS = 15;
  const ALIGN_CENTER = 22;         // 版本 3 只有一个对齐图形

  /** UTF-8 编码为字节数组。 */
  function utf8Bytes(str) {
    const out = [];
    for (let i = 0; i < str.length; i++) {
      let c = str.charCodeAt(i);
      if (c < 0x80) out.push(c);
      else if (c < 0x800) {
        out.push(0xc0 | (c >> 6), 0x80 | (c & 0x3f));
      } else {
        out.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 0x3f), 0x80 | (c & 0x3f));
      }
    }
    return out;
  }

  /** 生成最终比特流（数据码字 + 纠错码字）。 */
  function buildBits(text) {
    const bytes = utf8Bytes(text);
    // Byte 模式容量检查：4(mode) + 8(len) + 8*n <= 55*8
    if (bytes.length > DATA_CODEWORDS - 2) return null;

    const bits = [];
    const push = (val, len) => {
      for (let i = len - 1; i >= 0; i--) bits.push((val >> i) & 1);
    };
    push(0b0100, 4);              // Byte 模式
    push(bytes.length, 8);        // 字符计数（版本 1-9 为 8 位）
    for (const b of bytes) push(b, 8);

    // 终止符（最多 4 位）
    const cap = DATA_CODEWORDS * 8;
    for (let i = 0; i < 4 && bits.length < cap; i++) bits.push(0);
    // 补齐到字节边界
    while (bits.length % 8 !== 0) bits.push(0);

    // 转成码字
    const data = [];
    for (let i = 0; i < bits.length; i += 8) {
      let v = 0;
      for (let j = 0; j < 8; j++) v = (v << 1) | bits[i + j];
      data.push(v);
    }
    // 填充码字 0xEC / 0x11 交替
    let pad = 0;
    while (data.length < DATA_CODEWORDS) {
      data.push(pad % 2 === 0 ? 0xec : 0x11);
      pad++;
    }
    const ec = rsEncode(data, EC_CODEWORDS);
    return data.concat(ec);
  }

  /** 15 位格式信息：纠错等级 L(01) + 掩码号 + BCH(15,5) + 固定异或。 */
  function formatInfo(mask) {
    const data = (0b01 << 3) | mask;      // L = 01
    let bch = data << 10;
    for (let i = 4; i >= 0; i--) {
      if (bch & (1 << (i + 10))) bch ^= 0x537 << i;
    }
    return ((data << 10) | bch) ^ 0x5412;
  }

  /**
   * 生成 QR 矩阵。
   * @returns {{size:number, modules:number[][]}|null} modules[row][col] 取 0/1；超长返回 null
   */
  function qrEncode(text) {
    const codewords = buildBits(text);
    if (!codewords) return null;

    // -1 表示未占用；null 矩阵配合 reserved 标记功能图形
    const m = Array.from({ length: SIZE }, () => new Array(SIZE).fill(0));
    const reserved = Array.from({ length: SIZE }, () => new Array(SIZE).fill(false));

    const setF = (r, c, v) => {
      if (r >= 0 && r < SIZE && c >= 0 && c < SIZE) {
        m[r][c] = v;
        reserved[r][c] = true;
      }
    };

    // 定位图形（含分隔符）
    function finder(top, left) {
      for (let r = -1; r <= 7; r++) {
        for (let c = -1; c <= 7; c++) {
          const rr = top + r;
          const cc = left + c;
          if (rr < 0 || rr >= SIZE || cc < 0 || cc >= SIZE) continue;
          const inRing = (r >= 0 && r <= 6 && (c === 0 || c === 6)) ||
                         (c >= 0 && c <= 6 && (r === 0 || r === 6));
          const inCore = r >= 2 && r <= 4 && c >= 2 && c <= 4;
          setF(rr, cc, (inRing || inCore) ? 1 : 0);
        }
      }
    }
    finder(0, 0);
    finder(0, SIZE - 7);
    finder(SIZE - 7, 0);

    // 定时图形
    for (let i = 8; i < SIZE - 8; i++) {
      setF(6, i, i % 2 === 0 ? 1 : 0);
      setF(i, 6, i % 2 === 0 ? 1 : 0);
    }

    // 对齐图形（5x5，中心 22,22）
    for (let r = -2; r <= 2; r++) {
      for (let c = -2; c <= 2; c++) {
        const isEdge = Math.abs(r) === 2 || Math.abs(c) === 2;
        const isCore = r === 0 && c === 0;
        setF(ALIGN_CENTER + r, ALIGN_CENTER + c, (isEdge || isCore) ? 1 : 0);
      }
    }

    // 暗模块
    setF(4 * VERSION + 9, 8, 1);

    // 格式信息（掩码固定 0）
    const fmt = formatInfo(0);
    for (let i = 0; i < 15; i++) {
      const bit = (fmt >> i) & 1;
      // 左上：跳过定时线所在行列
      if (i < 6) setF(8, i, bit);
      else if (i < 8) setF(8, i + 1, bit);
      else if (i === 8) setF(7, 8, bit);
      else setF(14 - i, 8, bit);
      // 右上 / 左下
      if (i < 8) setF(SIZE - 1 - i, 8, bit);
      else setF(8, SIZE - 15 + i, bit);
    }

    // 数据位（zigzag，从右下开始，跳过第 6 列）
    const bits = [];
    for (const cw of codewords) {
      for (let i = 7; i >= 0; i--) bits.push((cw >> i) & 1);
    }
    let bitIdx = 0;
    let upward = true;
    for (let right = SIZE - 1; right >= 1; right -= 2) {
      if (right === 6) right = 5;          // 跳过定时列
      for (let v = 0; v < SIZE; v++) {
        const row = upward ? SIZE - 1 - v : v;
        for (let k = 0; k < 2; k++) {
          const col = right - k;
          if (reserved[row][col]) continue;
          const bit = bitIdx < bits.length ? bits[bitIdx++] : 0;
          // 掩码 0：(row + col) % 2 === 0 时反转
          m[row][col] = ((row + col) % 2 === 0) ? bit ^ 1 : bit;
        }
      }
      upward = !upward;
    }
    return { size: SIZE, modules: m };
  }

  /** 把矩阵画到 canvas 上（含 4 模块静区）。 */
  function drawTo(canvas, qr, moduleSize) {
    const quiet = 4;
    const total = (qr.size + quiet * 2) * moduleSize;
    canvas.width = total;
    canvas.height = total;
    const ctx = canvas.getContext('2d');
    ctx.fillStyle = '#fff';
    ctx.fillRect(0, 0, total, total);
    ctx.fillStyle = '#000';
    for (let r = 0; r < qr.size; r++) {
      for (let c = 0; c < qr.size; c++) {
        if (qr.modules[r][c]) {
          ctx.fillRect((c + quiet) * moduleSize, (r + quiet) * moduleSize, moduleSize, moduleSize);
        }
      }
    }
  }

  global.QR = { encode: qrEncode, drawTo };
})(window);
