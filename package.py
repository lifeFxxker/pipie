# -*- coding: utf-8 -*-
"""一键打包：编译 → 生成 dist/ → 打 zip。

用法（在项目根目录）：python package.py
产物：
  dist/PiPie.exe               主程序（单文件）
  dist/README.txt              用法说明（源头是 packaging/README.txt）
  dist/LICENSE.txt             MIT 许可声明（随 zip 分发）
  dist/SHA256SUMS              校验清单（在线升级的独立 release asset，不进 zip）
  dist/pipie-v<版本>-win64.zip        可直接发给别人

⚠️ 打包前请确认没有实例在跑 —— 正在运行的 exe 被系统锁着，覆盖会失败
（报 "Device or resource busy" / "另一个程序正在使用此文件"）。
脚本会自动先尝试退出在跑的实例。
"""
import hashlib
import os
import re
import shutil
import struct
import subprocess
import sys
import zipfile

# Windows 控制台默认是 GBK，下面 print 里的 ✓ / ⚠️ 会直接抛 UnicodeEncodeError。
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding='utf-8', errors='replace')
    except Exception:
        pass

HERE = os.path.dirname(os.path.abspath(__file__))
DIST = os.path.join(HERE, 'dist')
BUILT = os.path.join(HERE, 'target', 'release', 'PiPie.exe')
NAME = 'PiPie.exe'


def cargo_version():
    """版本只在 Cargo.toml 写一次，这里读出来，避免打包脚本再抄一份。"""
    with open(os.path.join(HERE, 'Cargo.toml'), encoding='utf-8') as f:
        in_pkg = False
        for line in f:
            s = line.strip()
            if s.startswith('['):
                in_pkg = s == '[package]'
                continue
            if in_pkg and s.startswith('version'):
                return s.split('"')[1]
    raise SystemExit('Cargo.toml 的 [package] 段里找不到 version')


VERSION = cargo_version()


def check_app_rc_version():
    """app.rc 的版本是手写的；和 Cargo.toml 不一致就会打出属性页版本错误的 exe。"""
    txt = open(os.path.join(HERE, 'app.rc'), encoding='utf-8').read()
    m = re.search(r'FILEVERSION\s+(\d+),(\d+),(\d+),(\d+)', txt)
    if not m:
        raise SystemExit('app.rc 里找不到 FILEVERSION')
    got = '%s.%s.%s' % m.group(1, 2, 3)
    if got != VERSION:
        raise SystemExit(
            'app.rc 的 FILEVERSION 是 %s，Cargo.toml 是 %s。\n'
            '先把 app.rc 的 FILEVERSION / PRODUCTVERSION / FileVersion / ProductVersion '
            '一起改掉再打包。' % (got, VERSION))


def sh(cmd, **kw):
    print('$', ' '.join(cmd), flush=True)
    return subprocess.run(cmd, cwd=HERE, **kw)


def kill_running():
    """先干掉在跑的实例，否则覆盖 exe 会失败。"""
    # 旧名也要杀：升级过程中可能还挂着上一个版本的实例
    for n in (NAME, 'Pigo.exe', 'RadialMenu.exe', 'pigo.exe'):
        subprocess.run(['taskkill', '/F', '/IM', n], capture_output=True)


def pe_subsystem(path):
    """读 PE 头的子系统字段 —— 2 = GUI（无黑窗口），3 = Console。"""
    d = open(path, 'rb').read()
    pe = struct.unpack_from('<I', d, 0x3c)[0]
    assert d[pe:pe + 4] == b'PE\x00\x00', '不是有效的 PE 文件'
    return struct.unpack_from('<H', d, pe + 24 + 68)[0]


def main():
    check_app_rc_version()
    print('版本 %s' % VERSION, flush=True)
    print('=== 1/4 编译（release，LTO + strip）===', flush=True)
    r = sh(['cargo', 'build', '--release'])
    if r.returncode != 0:
        print('编译失败')
        return 1
    if not os.path.exists(BUILT):
        print('找不到产物', BUILT)
        return 1

    print('=== 2/4 检查产物 ===', flush=True)
    subsys = pe_subsystem(BUILT)
    size = os.path.getsize(BUILT)
    md5 = hashlib.md5(open(BUILT, 'rb').read()).hexdigest()
    print('大小 %.1f KB' % (size / 1024))
    print('子系统 =', {2: 'GUI（无黑窗口）✓', 3: 'Console（会有黑窗口！）'}.get(subsys, subsys))
    print('md5 =', md5)
    if subsys != 2:
        print('⚠️ 警告：release 应该是 GUI 子系统。检查 main.rs 顶部的')
        print('   `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`')
    if size > 3 * 1024 * 1024:
        print('⚠️ 警告：体积超过 3MB，可能链接了不该有的东西')

    print('=== 3/4 生成 dist/ ===', flush=True)
    kill_running()
    os.makedirs(DIST, exist_ok=True)
    shutil.copy2(BUILT, os.path.join(DIST, NAME))
    # MIT 要求随分发附带版权与许可声明，所以 LICENSE 必须跟着 exe 进 zip。
    # 每次从仓库根目录覆盖，避免 dist/ 里留着旧协议。
    shutil.copy2(os.path.join(HERE, 'LICENSE'), os.path.join(DIST, 'LICENSE.txt'))
    # SHA256SUMS —— 在线升级用（updater 下载后强校验）。
    # ⚠️ 它是**独立的 release asset**，不进 zip（zip 白名单仍只有 exe+README）。
    sha = hashlib.sha256(open(os.path.join(DIST, NAME), 'rb').read()).hexdigest()
    with open(os.path.join(DIST, 'SHA256SUMS'), 'w', encoding='ascii', newline='\n') as f:
        f.write('%s  %s\n' % (sha, NAME))
    print('sha256 =', sha)
    # 发布用 README.txt 来自受版本控制的 packaging/README.txt，每次覆盖
    # （以前是"dist/ 里有就保留"，CI 里没有这个文件，打出来的 zip 就少一份说明）
    shutil.copy2(os.path.join(HERE, 'packaging', 'README.txt'),
                 os.path.join(DIST, 'README.txt'))

    print('=== 4/4 打 zip ===', flush=True)
    zname = 'pipie-v%s-win64.zip' % VERSION
    zpath = os.path.join(DIST, zname)
    # ⚠️⚠️ 只能打**白名单**，绝不能 `sorted(os.listdir(DIST))`！
    # 之前只要在 `dist/` 里跑过程序，运行时留下的 `settings.ini`
    # （**开发者的私有配置**：listening=0 / guide_seen=0 / 自己的应用列表）、
    # `settings.ini.bak`、`radial_log.txt`、`draglog.txt` 就会被**一起打进发布包**
    # → 用户解压后一启动就「默认不监听」（继承了开发者的 listening=0），
    # 引导状态、应用列表也全是别人的。用户 2026-10-14 报的就是这个。
    RELEASE_FILES = [NAME, 'README.txt', 'LICENSE.txt']
    with zipfile.ZipFile(zpath, 'w', zipfile.ZIP_DEFLATED) as z:
        for fn in RELEASE_FILES:
            fp = os.path.join(DIST, fn)
            if os.path.exists(fp):
                z.write(fp, fn)
            else:
                print('⚠️ 缺少发布文件：%s（zip 会不完整）' % fn)
    # 自查：压缩包里**不许出现** ini / 备份 / 日志
    with zipfile.ZipFile(zpath) as z:
        names = z.namelist()
        bad = [n for n in names
               if n.lower().endswith(('.ini', '.bak', '.log'))
               or (n.lower().endswith('.txt')
                   and n.lower() not in ('readme.txt', 'license.txt'))]
        if bad:
            print('❌ 发布包混入了不该有的文件：%s' % bad)
            return 1
        print('发布包内容：%s' % names)
    print('已生成 %s（%.1f KB）' % (zpath, os.path.getsize(zpath) / 1024))
    print()
    print('打包完成：')
    for fn in sorted(os.listdir(DIST)):
        print('  dist/%s' % fn)
    return 0


if __name__ == '__main__':
    sys.exit(main())