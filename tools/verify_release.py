# -*- coding: utf-8 -*-
"""独立校验 dist/ 里的发布包 —— CI 和本地都能跑：python tools/verify_release.py

检查项（任何一条不满足 → 退出码 1）：
  1. dist/ 里有 pipie-v<版本>-win64.zip，版本取自 Cargo.toml
  2. zip 内恰好是 PiPie.exe / README.txt / LICENSE.txt
  3. LICENSE.txt 是 MIT
  4. exe 是 GUI 子系统（PE subsystem == 2），不是 Console
  5. SHA256SUMS 与 zip 内的 exe 一致（在线更新依赖它）
  6. exe 体积 < 3 MB（拼音词典让二进制涨到 ~2.06MB）

单独成文件是为了让 CI 的"发布前校验"和本地判断用同一套规则：
package.py 负责生产，这里负责验收，两边不共用同一段判断逻辑。
"""
import hashlib
import os
import struct
import sys
import zipfile

for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding='utf-8', errors='replace')
    except Exception:
        pass

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, HERE)
from package import VERSION, NAME  # noqa: E402

FAILED = []
WANT_FILES = [NAME, 'README.txt', 'LICENSE.txt']


def check(ok, msg):
    print(('  ok    ' if ok else '  FAIL  ') + msg)
    if not ok:
        FAILED.append(msg)


def main():
    zpath = os.path.join(HERE, 'dist', 'pipie-v%s-win64.zip' % VERSION)
    print('校验 %s' % os.path.basename(zpath))
    if not os.path.exists(zpath):
        check(False, '找不到发布包（先跑 python package.py）')
        print('\n❌ 1 项未通过')
        return 1

    with zipfile.ZipFile(zpath) as z:
        names = z.namelist()
        check(sorted(names) == sorted(WANT_FILES), 'zip 内容 = %s' % names)
        lic = z.read('LICENSE.txt').decode('utf-8', 'replace')
        check(lic.lstrip().startswith('MIT License'), 'LICENSE.txt 首行是 MIT License')
        exe = z.read(NAME)
        check(len(exe) < 3 * 1024 * 1024, 'exe 体积 %.1f KB' % (len(exe) / 1024))

    pe = struct.unpack_from('<I', exe, 0x3c)[0]
    if exe[pe:pe + 4] != b'PE\x00\x00':
        check(False, 'exe 不是有效的 PE 文件')
    else:
        subsys = struct.unpack_from('<H', exe, pe + 24 + 68)[0]
        check(subsys == 2, 'PE 子系统 = %d（2 = GUI）' % subsys)

    sums = os.path.join(HERE, 'dist', 'SHA256SUMS')
    if not os.path.exists(sums):
        check(False, '找不到 dist/SHA256SUMS')
    else:
        with open(sums, encoding='ascii') as f:
            parts = f.read().split()
        check(bool(parts) and parts[0] == hashlib.sha256(exe).hexdigest(),
              'SHA256SUMS 与 zip 内的 exe 一致')

    print()
    if FAILED:
        print('❌ %d 项未通过' % len(FAILED))
        return 1
    print('✅ 发布包校验通过')
    return 0


if __name__ == '__main__':
    sys.exit(main())
