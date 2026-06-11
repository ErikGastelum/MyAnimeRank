# -*- mode: python ; coding: utf-8 -*-
from PyInstaller.utils.hooks import collect_data_files, collect_submodules, collect_all

datas = [
    ('MyAnimeRank.html', '.'),
]

binaries = []
hiddenimports = []

# collect_all picks up datas + binaries (e.g. WebView2Loader.dll) + submodules
tmp = collect_all('webview')
datas    += tmp[0]
binaries += tmp[1]
hiddenimports += tmp[2]

# uvicorn uses runtime plugin selection — these modules are not statically imported
hiddenimports += [
    'uvicorn.logging',
    'uvicorn.loops',
    'uvicorn.loops.auto',
    'uvicorn.loops.asyncio',
    'uvicorn.protocols',
    'uvicorn.protocols.http',
    'uvicorn.protocols.http.auto',
    'uvicorn.protocols.http.h11_impl',
    'uvicorn.protocols.websockets',
    'uvicorn.protocols.websockets.auto',
    'uvicorn.lifespan',
    'uvicorn.lifespan.on',
    'uvicorn.lifespan.off',
    'anyio',
    'anyio._backends._asyncio',
    'h11',
    'multipart',
    'multipart.multipart',
    'httptools',
    'uvicorn.protocols.http.httptools_impl',
    '_secrets',
]

a = Analysis(
    ['app.py'],
    pathex=[],
    binaries=binaries,
    datas=datas,
    hiddenimports=hiddenimports,
    hookspath=[],
    hooksconfig={},
    runtime_hooks=['hooks/rthook_path.py'],
    excludes=[],
    noarchive=False,
)

pyz = PYZ(a.pure, a.zipped_data)

# One-directory mode: exe + DLLs all in dist/MyAnimeRank/
# DLLs sit beside the exe so Windows DLL search finds them without temp-dir tricks.
exe = EXE(
    pyz,
    a.scripts,
    [],
    [],
    [],
    name='MyAnimeRank',
    debug=False,
    strip=False,
    upx=False,
    upx_exclude=[],
    console=False,
    icon=None,
)

coll = COLLECT(
    exe,
    a.binaries,
    a.zipfiles,
    a.datas,
    strip=False,
    upx=False,
    name='MyAnimeRank',
)
