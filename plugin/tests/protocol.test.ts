/**
 * 协议帧构造/解析测试 —— 逐字对齐 docs/superpowers/protocols/native-messaging.md。
 * 断言字段名与类型（§2.1 capture / §4 save / 回执判别），以及路径与尺寸的
 * 插件侧预检（§3 / §2.1 的 0=未知 语义）。
 */

import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import {
  assertValidDimensions,
  buildCaptureFrame,
  buildSaveFrame,
  bytesToBase64,
  CHROME_MAX_MESSAGE_BYTES,
  parseReply,
  readPngSize,
  uuid,
  validateCapturePath,
} from '../src/shared/protocol.js';
import { PROBE_PNG_BASE64 } from '../src/shared/native-client.js';

describe('buildCaptureFrame（§2.1 插件 → 宿主）', () => {
  it('字段名与类型逐字对齐契约', () => {
    const f = buildCaptureFrame('req-1', '/tmp/hawksnap-plugin-req-1.png', 1920, 1080);
    assert.deepEqual(Object.keys(f).sort(), ['cmd', 'height', 'id', 'path', 'width']);
    assert.equal(f.cmd, 'capture');
    assert.equal(typeof f.id, 'string');
    assert.equal(typeof f.path, 'string');
    assert.equal(typeof f.width, 'number');
    assert.equal(typeof f.height, 'number');
    // 文档示例的序列化形状。
    assert.equal(
      JSON.stringify(buildCaptureFrame('t1', '/tmp/hawksnap-plugin-t1.png', 1920, 1080)),
      '{"cmd":"capture","id":"t1","path":"/tmp/hawksnap-plugin-t1.png","width":1920,"height":1080}',
    );
  });

  it('宽高允许 0（= 未知，桌面端以 PNG 实际像素为准）', () => {
    const f = buildCaptureFrame('u', '/tmp/hawksnap-plugin-u.png', 0, 0);
    assert.equal(f.width, 0);
    assert.equal(f.height, 0);
  });

  it('非法 path：相对路径 / 非 png / 前缀不符 / 空', () => {
    for (const bad of ['tmp/hawksnap-plugin-x.png', '/tmp/x.jpg', '/tmp/foo.png', '']) {
      assert.throws(() => buildCaptureFrame('id', bad, 1, 1), undefined, `应拒绝: ${bad}`);
    }
  });

  it('非法尺寸：负数 / 非整数 / NaN', () => {
    const p = '/tmp/hawksnap-plugin-x.png';
    assert.throws(() => buildCaptureFrame('id', p, -1, 1));
    assert.throws(() => buildCaptureFrame('id', p, 1.5, 1));
    assert.throws(() => buildCaptureFrame('id', p, Number.NaN, 1));
  });

  it('空 id / 非字符串 id 拒绝', () => {
    assert.throws(() => buildCaptureFrame('  ', '/tmp/hawksnap-plugin-x.png', 1, 1));
  });
});

describe('validateCapturePath（§3 插件侧预检）', () => {
  it('合法路径通过', () => {
    validateCapturePath('/var/folders/xx/T/hawksnap-native-inbox/hawksnap-plugin-abc.png');
  });
  it('大小写不敏感的 .png 扩展名', () => {
    validateCapturePath('/tmp/hawksnap-plugin-x.PNG');
  });
  it('Windows 盘符绝对路径也接受（path 来自宿主 save 回执，不比宿主更严）', () => {
    validateCapturePath('C:\\Users\\you\\AppData\\Local\\Temp\\hawksnap-native-inbox\\hawksnap-plugin-abc.png');
    validateCapturePath('C:/Temp/hawksnap-plugin-abc.png');
  });
  it('仍拒绝：相对路径 / 非 png / 前缀不符（含反斜杠路径）', () => {
    for (const bad of ['tmp/hawksnap-plugin-x.png', './hawksnap-plugin-x.png', '/tmp/x.jpg', '/tmp/foo.png', 'C:\\Temp\\foo.png']) {
      assert.throws(() => validateCapturePath(bad), undefined, `应拒绝: ${bad}`);
    }
  });
});

describe('assertValidDimensions（§2.1：真实像素或 0）', () => {
  it('0 与正整数通过；负数/小数拒绝', () => {
    assertValidDimensions(0, 0);
    assertValidDimensions(2771, 14434);
    assert.throws(() => assertValidDimensions(-1, 0));
    assert.throws(() => assertValidDimensions(0, 2.5));
  });
});

describe('buildSaveFrame（§4 插件 → 宿主）', () => {
  it('字段名与类型逐字对齐契约（data 无 data: 前缀）', () => {
    const f = buildSaveFrame('req-2', 'aGk=');
    assert.deepEqual(Object.keys(f).sort(), ['cmd', 'data', 'id']);
    assert.equal(f.cmd, 'save');
    assert.equal(f.data, 'aGk=');
  });

  it('载荷超过浏览器单消息 4 MB → 抛错（调用点降级本地下载）', () => {
    const tooBig = 'A'.repeat(CHROME_MAX_MESSAGE_BYTES - 128); // +256 开销 → 超限
    assert.throws(() => buildSaveFrame('id', tooBig));
    const justFits = 'A'.repeat(CHROME_MAX_MESSAGE_BYTES - 256);
    assert.doesNotThrow(() => buildSaveFrame('id', justFits));
  });

  it('空 data 拒绝', () => {
    assert.throws(() => buildSaveFrame('id', ''));
  });
});

describe('parseReply（§2.1 / §4 宿主 → 插件回执）', () => {
  it('capture 成功回执（ok+savedPath）→ capture-ok', () => {
    const r = parseReply({ id: 'i1', ok: true, savedPath: '/Users/you/Pictures/HawkSnap/browser 2026-10-27 153000.png' });
    assert.deepEqual(r, { kind: 'capture-ok', id: 'i1', savedPath: '/Users/you/Pictures/HawkSnap/browser 2026-10-27 153000.png' });
  });

  it('save 成功回执（ok+path）→ save-ok（字段是 path，不是 savedPath）', () => {
    const r = parseReply({ id: 'i2', ok: true, path: '/var/folders/x/T/hawksnap-native-inbox/hawksnap-plugin-i2.png' });
    assert.equal(r.kind, 'save-ok');
    assert.equal(r.kind === 'save-ok' && r.path.endsWith('hawksnap-plugin-i2.png'), true);
  });

  it('失败回执（ok=false + 中文 error）→ error，原因透传', () => {
    const r = parseReply({ id: 'i3', ok: false, error: '路径不在系统临时目录内' });
    assert.deepEqual(r, { kind: 'error', id: 'i3', error: '路径不在系统临时目录内' });
  });

  it('成功回执两个路径字段同时出现 → error（契约要求互斥）', () => {
    const r = parseReply({ id: 'i4', ok: true, savedPath: '/a.png', path: '/b.png' });
    assert.equal(r.kind, 'error');
  });

  it('ok=true 但两个路径字段都缺 → error', () => {
    assert.equal(parseReply({ id: 'i5', ok: true }).kind, 'error');
  });

  it('缺 ok 字段 → error；id 缺失 → 空串（镜像契约）', () => {
    const r = parseReply({ nonsense: true });
    assert.equal(r.kind, 'error');
    assert.equal(r.id, '');
  });

  it('接受 JSON 字符串（宿主原始帧）', () => {
    const r = parseReply('{"id":"s1","ok":true,"savedPath":"/p.png"}');
    assert.deepEqual(r, { kind: 'capture-ok', id: 's1', savedPath: '/p.png' });
  });

  it('非对象输入 → error', () => {
    assert.equal(parseReply(42).kind, 'error');
    assert.equal(parseReply(null).kind, 'error');
  });
});

describe('readPngSize（§2.1 真实像素的生成侧来源）', () => {
  it('手工构造的 PNG 头：读出 IHDR 宽高', () => {
    const bytes = new Uint8Array(26);
    [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a].forEach((b, i) => (bytes[i] = b));
    const view = new DataView(bytes.buffer);
    view.setUint32(8, 13); // IHDR 数据长度
    bytes.set([0x49, 0x48, 0x44, 0x52], 12); // "IHDR"
    view.setUint32(16, 1920);
    view.setUint32(20, 10000);
    assert.deepEqual(readPngSize(bytes), { width: 1920, height: 10000 });
  });

  it('非 PNG / 过短字节 → null（调用点改填 0）', () => {
    assert.equal(readPngSize(new Uint8Array([1, 2, 3])), null);
    assert.equal(readPngSize(new Uint8Array(24).fill(0x00)), null);
  });

  it('探测用的 1×1 PNG 头部合法（防内嵌常量被改坏）', () => {
    const bin = atob(PROBE_PNG_BASE64);
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    assert.deepEqual(readPngSize(bytes), { width: 1, height: 1 });
  });
});

describe('bytesToBase64 / uuid', () => {
  it('小数组往返一致', () => {
    assert.equal(bytesToBase64(Uint8Array.from([104, 105])), 'aGk=');
    const bin = atob(bytesToBase64(Uint8Array.from([0, 1, 2, 250, 255])));
    assert.equal(bin.charCodeAt(3), 250);
    assert.equal(bin.charCodeAt(4), 255);
  });

  it('大数组分块转换不抛错（>32k 触发多块路径）', () => {
    const big = new Uint8Array(300_000).fill(7);
    assert.equal(atob(bytesToBase64(big)).length, 300_000);
  });

  it('uuid 是 RFC 4122 v4 形状', () => {
    assert.match(
      uuid(),
      /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/,
    );
  });
});
